// SPDX-License-Identifier: GPL-3.0-only
//
// Unmanaged-container backend.
//
// TrueNAS only tracks updates for its own apps, so containers deployed
// outside them (compose stacks, Dockge, hand-run containers…) are watched
// through a Portainer instance instead:
//
//  - containers are listed via Portainer's Docker API proxy
//    (`/api/endpoints/{id}/docker/...`), skipping the `ix-*` compose projects
//    that TrueNAS manages (those are covered by the apps check);
//  - "update available" means the image tag's digest at the registry no
//    longer matches any local RepoDigest — the same check Watchtower does;
//  - applying an update uses Portainer's pull-and-recreate endpoint
//    (`POST /api/docker/{env}/containers/{id}/recreate`), which pulls the
//    newer image and recreates the container with its existing config.
//
// Registry digest lookups count toward Docker Hub's anonymous rate limit, so
// callers should keep this check infrequent (the applet: manual checks plus
// a few times a day).

use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::backend::{UpdateItem, UpdateKind};

/// How to reach Portainer. Persisted via cosmic-config and editable from the
/// applet's settings panel. Optional — when unset, the container check is off.
#[derive(Debug, Clone, Default)]
pub struct PortainerConnection {
    /// e.g. "https://truenas.local:31015".
    pub base_url: String,
    /// A Portainer user access token (X-API-Key).
    pub api_key: String,
    pub accept_invalid_certs: bool,
}

impl PortainerConnection {
    pub fn is_configured(&self) -> bool {
        !self.base_url.trim().is_empty() && !self.api_key.trim().is_empty()
    }

    /// Base with a scheme and no trailing slash; bare hosts default to https.
    pub fn normalized_base(&self) -> String {
        let b = self.base_url.trim().trim_end_matches('/');
        if b.starts_with("http://") || b.starts_with("https://") {
            b.to_string()
        } else {
            format!("https://{b}")
        }
    }

    fn client(&self) -> Result<reqwest::Client, String> {
        reqwest::Client::builder()
            .danger_accept_invalid_certs(self.accept_invalid_certs)
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| format!("HTTP client: {e}"))
    }

    async fn get(&self, path: &str) -> Result<Value, String> {
        let url = format!("{}{path}", self.normalized_base());
        let resp = self
            .client()?
            .get(&url)
            .header("X-API-Key", self.api_key.trim())
            .send()
            .await
            .map_err(|e| format!("Could not reach Portainer ({e})"))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| format!("{path}: {e}"))?;
        if !status.is_success() {
            let snippet: String = text.chars().take(160).collect();
            return Err(match status.as_u16() {
                401 | 403 => "Portainer authentication failed — check the access token".to_string(),
                _ => format!("Portainer {path}: HTTP {status}: {snippet}"),
            });
        }
        serde_json::from_str(&text).map_err(|e| format!("{path}: invalid JSON ({e})"))
    }
}

/// The result of a container check, kept separate from the TrueNAS apps
/// report because the two run on different schedules.
#[derive(Debug, Clone, Default)]
pub struct ContainerReport {
    pub updates: Vec<UpdateItem>,
    /// How many (non-TrueNAS) running containers were examined.
    pub total_containers: usize,
    pub errors: Vec<String>,
}

#[derive(Deserialize)]
struct Endpoint {
    #[serde(rename = "Id")]
    id: i64,
    #[serde(rename = "Type", default)]
    kind: i64,
}

#[derive(Deserialize)]
struct ApiContainer {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Names", default)]
    names: Vec<String>,
    #[serde(rename = "Image", default)]
    image: String,
    #[serde(rename = "ImageID", default)]
    image_id: String,
    #[serde(rename = "Labels", default)]
    labels: HashMap<String, String>,
}

impl ApiContainer {
    fn display_name(&self) -> String {
        self.names
            .first()
            .map(|n| n.trim_start_matches('/').to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| self.id.chars().take(12).collect())
    }

    /// Containers belonging to a TrueNAS app run in an `ix-<app>` compose
    /// project; those are already covered by the apps check.
    fn is_truenas_managed(&self) -> bool {
        self.labels
            .get("com.docker.compose.project")
            .is_some_and(|p| p.starts_with("ix-"))
    }
}

/// Check all Docker environments known to Portainer for containers whose
/// image tag has a newer build at the registry. Never fails as a whole.
pub async fn check_containers(conn: PortainerConnection) -> ContainerReport {
    let mut report = ContainerReport::default();
    if !conn.is_configured() {
        return report;
    }

    let endpoints: Vec<Endpoint> = match conn
        .get("/api/endpoints?limit=100")
        .await
        .and_then(|v| serde_json::from_value(v).map_err(|e| format!("endpoints: {e}")))
    {
        Ok(e) => e,
        Err(e) => {
            report.errors.push(e);
            return report;
        }
    };

    // Registry client: separate from the Portainer client only in spirit —
    // same TLS posture, so a self-signed private registry also works.
    let registry_client = match conn.client() {
        Ok(c) => c,
        Err(e) => {
            report.errors.push(e);
            return report;
        }
    };
    // Containers often share an image; ask the registry once per unique ref.
    let mut digest_cache: HashMap<String, Result<String, String>> = HashMap::new();

    // Types 1 and 2 are Docker environments (local socket / agent).
    for ep in endpoints.iter().filter(|e| e.kind == 1 || e.kind == 2) {
        let containers: Vec<ApiContainer> = match conn
            .get(&format!("/api/endpoints/{}/docker/containers/json", ep.id))
            .await
            .and_then(|v| serde_json::from_value(v).map_err(|e| format!("containers: {e}")))
        {
            Ok(c) => c,
            Err(e) => {
                report.errors.push(e);
                continue;
            }
        };

        for c in containers {
            if c.is_truenas_managed() {
                continue;
            }
            // Images pinned by digest or referenced by raw id can't drift.
            if c.image.contains('@') || c.image.starts_with("sha256:") || c.image.is_empty() {
                continue;
            }
            report.total_containers += 1;

            // Local digests of the image the container actually runs.
            let local = match image_repo_digests(&conn, ep.id, &c.image_id).await {
                Ok(d) => d,
                Err(e) => {
                    report.errors.push(format!("{}: {e}", c.display_name()));
                    continue;
                }
            };
            if local.is_empty() {
                // Locally built image — nothing at a registry to compare with.
                continue;
            }

            let remote = match digest_cache.get(&c.image) {
                Some(cached) => cached.clone(),
                None => {
                    let fresh = remote_digest(&registry_client, &c.image).await;
                    digest_cache.insert(c.image.clone(), fresh.clone());
                    fresh
                }
            };
            match &remote {
                Ok(digest) if !local.contains(digest) => {
                    report.updates.push(UpdateItem {
                        name: c.display_name(),
                        title: c.display_name(),
                        current: c.image.clone(),
                        latest: String::new(),
                        kind: UpdateKind::Container {
                            endpoint_id: ep.id,
                            container_id: c.id.clone(),
                            image: c.image.clone(),
                        },
                    });
                }
                Ok(_) => {}
                Err(e) => report
                    .errors
                    .push(format!("{} ({}): {e}", c.display_name(), c.image)),
            }
        }
    }

    report
        .updates
        .sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
    report
}

/// The `sha256:…` parts of an image's RepoDigests, via the Docker API proxy.
async fn image_repo_digests(
    conn: &PortainerConnection,
    endpoint_id: i64,
    image_id: &str,
) -> Result<Vec<String>, String> {
    #[derive(Deserialize)]
    struct Inspect {
        #[serde(rename = "RepoDigests", default)]
        repo_digests: Vec<String>,
    }
    let v = conn
        .get(&format!(
            "/api/endpoints/{endpoint_id}/docker/images/{image_id}/json"
        ))
        .await?;
    let inspect: Inspect =
        serde_json::from_value(v).map_err(|e| format!("image inspect: {e}"))?;
    Ok(inspect
        .repo_digests
        .iter()
        .filter_map(|d| d.split_once('@').map(|(_, digest)| digest.to_string()))
        .collect())
}

/// Recreate a container with its existing configuration. With `pull`, the
/// image is re-pulled inside this (blocking, progress-less) request — callers
/// wanting a live progress bar should [`pull_image`] first and pass `false`.
pub async fn recreate_container(
    conn: &PortainerConnection,
    endpoint_id: i64,
    container_id: &str,
    pull: bool,
) -> Result<(), String> {
    let url = format!(
        "{}/api/docker/{endpoint_id}/containers/{container_id}/recreate",
        conn.normalized_base()
    );
    let resp = conn
        .client()?
        .post(&url)
        .header("X-API-Key", conn.api_key.trim())
        .json(&json!({ "PullImage": pull }))
        // Recreating still stops/starts the container (and pulls, when asked);
        // allow it time.
        .timeout(Duration::from_secs(15 * 60))
        .send()
        .await
        .map_err(|e| format!("recreate failed ({e})"))?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        let text = resp.text().await.unwrap_or_default();
        let snippet: String = text.chars().take(160).collect();
        Err(format!("recreate failed: HTTP {status}: {snippet}"))
    }
}

/// Pull an image on the given environment via the Docker API proxy, reporting
/// overall progress (`0.0..=1.0`) aggregated from the per-layer events the
/// daemon streams back. Docker weighs nothing itself, so each layer counts
/// its download as 70% and its extraction as 30%.
pub async fn pull_image(
    conn: &PortainerConnection,
    endpoint_id: i64,
    image: &str,
    on_progress: impl Fn(f32),
) -> Result<(), String> {
    use futures::StreamExt;

    let (name, tag) = split_name_tag(image);
    let url = format!(
        "{}/api/endpoints/{endpoint_id}/docker/images/create",
        conn.normalized_base()
    );
    let resp = conn
        .client()?
        .post(&url)
        .query(&[("fromImage", name), ("tag", tag)])
        .header("X-API-Key", conn.api_key.trim())
        .timeout(Duration::from_secs(30 * 60))
        .send()
        .await
        .map_err(|e| format!("pull failed ({e})"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let snippet: String = text.chars().take(160).collect();
        return Err(format!("pull failed: HTTP {status}: {snippet}"));
    }

    // The body is a stream of newline-delimited JSON progress events.
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut layers: HashMap<String, f32> = HashMap::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("pull stream: {e}"))?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(pos) = buf.find('\n') {
            let line: String = buf.drain(..=pos).collect();
            let Ok(event) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if let Some(err) = event.get("error").and_then(Value::as_str) {
                return Err(format!("pull: {err}"));
            }
            let Some(id) = event.get("id").and_then(Value::as_str) else {
                continue; // digest/status summary lines carry no layer id
            };
            let detail = |ev: &Value| -> Option<f32> {
                let d = ev.get("progressDetail")?;
                let current = d.get("current")?.as_f64()?;
                let total = d.get("total")?.as_f64()?;
                (total > 0.0).then(|| (current / total).min(1.0) as f32)
            };
            let layer_frac = match event.get("status").and_then(Value::as_str).unwrap_or("") {
                "Pulling fs layer" | "Waiting" => Some(0.0),
                "Downloading" => detail(&event).map(|f| f * 0.7),
                "Verifying Checksum" | "Download complete" => Some(0.7),
                "Extracting" => detail(&event).map(|f| 0.7 + f * 0.3),
                "Pull complete" | "Already exists" => Some(1.0),
                _ => None,
            };
            if let Some(f) = layer_frac {
                layers.insert(id.to_string(), f);
                let sum: f32 = layers.values().sum();
                on_progress(sum / layers.len() as f32);
            }
        }
    }
    Ok(())
}

/// Split an image reference into name and tag (default "latest"). A ':' only
/// counts as a tag separator after the last '/', otherwise it's a registry port.
fn split_name_tag(image: &str) -> (&str, &str) {
    match image.rsplit_once(':') {
        Some((name, tag)) if !tag.contains('/') => (name, tag),
        _ => (image, "latest"),
    }
}

// --- Registry digest lookup -------------------------------------------------

/// Accept headers covering both Docker and OCI manifests (and their multi-arch
/// list/index forms, which is what a tag's top-level digest usually is).
const MANIFEST_ACCEPT: &str = "application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.oci.image.index.v1+json, \
     application/vnd.docker.distribution.manifest.v2+json, \
     application/vnd.oci.image.manifest.v1+json";

struct ImageRef {
    registry: String,
    repo: String,
    tag: String,
}

/// Split an image reference into registry host, repository, and tag, applying
/// Docker's defaulting rules (Docker Hub, `library/`, `latest`).
fn parse_image_ref(image: &str) -> ImageRef {
    let (name, tag) = match image.rsplit_once(':') {
        // A ':' after the last '/' is a tag; otherwise it's a registry port.
        Some((n, t)) if !t.contains('/') => (n, t),
        _ => (image, "latest"),
    };
    match name.split_once('/') {
        // First segment is a host only if it looks like one (dot, port, or
        // "localhost") — that's how Docker itself disambiguates.
        Some((host, rest)) if host.contains('.') || host.contains(':') || host == "localhost" => {
            ImageRef {
                registry: host.to_string(),
                repo: rest.to_string(),
                tag: tag.to_string(),
            }
        }
        _ => ImageRef {
            registry: "registry-1.docker.io".to_string(),
            repo: if name.contains('/') {
                name.to_string()
            } else {
                format!("library/{name}")
            },
            tag: tag.to_string(),
        },
    }
}

/// Ask the image's registry for the current digest of its tag. Handles the
/// anonymous Bearer-token dance used by Docker Hub, ghcr.io, and friends.
async fn remote_digest(client: &reqwest::Client, image: &str) -> Result<String, String> {
    let r = parse_image_ref(image);
    let url = format!(
        "https://{}/v2/{}/manifests/{}",
        r.registry, r.repo, r.tag
    );

    let head = |token: Option<String>| {
        let client = client.clone();
        let url = url.clone();
        async move {
            let mut req = client.head(&url).header("Accept", MANIFEST_ACCEPT);
            if let Some(t) = token {
                req = req.bearer_auth(t);
            }
            req.send().await.map_err(|e| format!("registry: {e}"))
        }
    };

    let mut resp = head(None).await?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        let challenge = resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let token = fetch_token(client, &challenge, &r.repo).await?;
        resp = head(Some(token)).await?;
    }

    if !resp.status().is_success() {
        return Err(format!("registry: HTTP {} for {}", resp.status(), r.tag));
    }
    resp.headers()
        .get("docker-content-digest")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .ok_or_else(|| "registry did not return a digest".to_string())
}

/// Fetch an anonymous pull token from the auth service named in a
/// `WWW-Authenticate: Bearer realm="…",service="…",scope="…"` challenge.
async fn fetch_token(
    client: &reqwest::Client,
    challenge: &str,
    repo: &str,
) -> Result<String, String> {
    let params = parse_challenge(challenge);
    let realm = params
        .get("realm")
        .ok_or_else(|| "registry auth: no realm in challenge".to_string())?;
    let mut req = client.get(realm);
    if let Some(service) = params.get("service") {
        req = req.query(&[("service", service.as_str())]);
    }
    let scope = params
        .get("scope")
        .cloned()
        .unwrap_or_else(|| format!("repository:{repo}:pull"));
    req = req.query(&[("scope", scope.as_str())]);

    let resp = req
        .send()
        .await
        .map_err(|e| format!("registry auth: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("registry auth: HTTP {}", resp.status()));
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| format!("registry auth: {e}"))?;
    v.get("token")
        .or_else(|| v.get("access_token"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "registry auth: no token in response".to_string())
}

/// Parse the `k="v"` pairs of a Bearer challenge.
fn parse_challenge(challenge: &str) -> HashMap<String, String> {
    challenge
        .trim_start_matches("Bearer")
        .split(',')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((
                k.trim().to_string(),
                v.trim().trim_matches('"').to_string(),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_ref_docker_hub_official() {
        let r = parse_image_ref("nginx");
        assert_eq!(r.registry, "registry-1.docker.io");
        assert_eq!(r.repo, "library/nginx");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn image_ref_docker_hub_user() {
        let r = parse_image_ref("louislam/dockge:1");
        assert_eq!(r.registry, "registry-1.docker.io");
        assert_eq!(r.repo, "louislam/dockge");
        assert_eq!(r.tag, "1");
    }

    #[test]
    fn image_ref_other_registry() {
        let r = parse_image_ref("ghcr.io/immich-app/immich-server:v1.99.0");
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repo, "immich-app/immich-server");
        assert_eq!(r.tag, "v1.99.0");
    }

    #[test]
    fn image_ref_registry_with_port() {
        let r = parse_image_ref("localhost:5000/my/app");
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.repo, "my/app");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn challenge_parsing() {
        let p = parse_challenge(
            r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/nginx:pull""#,
        );
        assert_eq!(p.get("realm").unwrap(), "https://auth.docker.io/token");
        assert_eq!(p.get("service").unwrap(), "registry.docker.io");
        assert_eq!(
            p.get("scope").unwrap(),
            "repository:library/nginx:pull"
        );
    }
}
