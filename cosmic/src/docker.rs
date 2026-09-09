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
    /// Portainer wasn't reachable at all — treated as transient by
    /// automatic checks (see [`crate::backend::AppsReport::unreachable`]).
    pub unreachable: bool,
}

#[derive(Deserialize)]
struct Endpoint {
    #[serde(rename = "Id")]
    id: i64,
    #[serde(rename = "Type", default)]
    kind: i64,
}

#[derive(Deserialize, Default)]
struct ApiHostConfig {
    /// For a container sharing another's network namespace this is the literal
    /// `container:<id>` — which is why recreating the carrier breaks it.
    #[serde(rename = "NetworkMode", default)]
    network_mode: String,
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
    /// "running", "exited", … Current Docker always sends it; `Status` ("Up 3
    /// days") is the older spelling and is used as a fallback.
    #[serde(rename = "State", default)]
    state: String,
    #[serde(rename = "Status", default)]
    status: String,
    #[serde(rename = "HostConfig", default)]
    host_config: ApiHostConfig,
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

    /// Only a running container is a candidate for an update. The list is
    /// fetched with `?all=1` so stopped *dependents* are visible to the checks
    /// below; without this filter that same flag would make the applet offer to
    /// recreate — and thereby start — containers deliberately stopped.
    ///
    /// A list carrying neither field is treated as running: that is what it
    /// meant before `?all=1`, and updating nothing at all is a worse failure
    /// than the one this guards against.
    fn is_running(&self) -> bool {
        if !self.state.is_empty() {
            return self.state == "running";
        }
        if !self.status.is_empty() {
            return self.status.starts_with("Up");
        }
        true
    }

    fn label(&self, name: &str) -> &str {
        self.labels.get(name).map(String::as_str).unwrap_or_default()
    }

    fn compose_project(&self) -> &str {
        self.label("com.docker.compose.project")
    }

    fn compose_service(&self) -> &str {
        self.label("com.docker.compose.service")
    }

    /// Where the stack's compose file lives, so a refusal can say where to go.
    fn stack_working_dir(&self) -> &str {
        self.label("com.docker.compose.project.working_dir")
    }
}

/// `com.docker.compose.depends_on` is a comma-separated list of
/// `<service>:<condition>:<restart>` entries, e.g.
/// "gluetun:service_healthy:false".
fn parse_depends_on(value: &str) -> Vec<&str> {
    value
        .split(',')
        .filter_map(|entry| {
            let name = entry.split(':').next().unwrap_or_default().trim();
            (!name.is_empty()).then_some(name)
        })
        .collect()
}

/// Containers riding this one's network namespace — the fatal case.
///
/// compose's `network_mode: "service:x"` is stored by Docker per container as a
/// literal `HostConfig.NetworkMode = "container:<x-id>"`. Recreating x gives it
/// a *new* id, so the passenger's namespace target stops existing: it cannot
/// start ("No such container"), and one that was already running can keep
/// reporting healthy with no network at all, which is worse because nothing
/// alarms.
fn network_passengers(target: &ApiContainer, all: &[ApiContainer]) -> Vec<String> {
    let needle = format!("container:{}", target.id);
    all.iter()
        .filter(|c| c.id != target.id && c.host_config.network_mode == needle)
        .map(ApiContainer::display_name)
        .collect()
}

/// Containers in the same stack that declared a compose dependency on this
/// one's service.
fn compose_dependants(target: &ApiContainer, all: &[ApiContainer]) -> Vec<String> {
    let (project, service) = (target.compose_project(), target.compose_service());
    if project.is_empty() || service.is_empty() {
        return Vec::new();
    }
    all.iter()
        .filter(|c| c.id != target.id && c.compose_project() == project)
        .filter(|c| parse_depends_on(c.label("com.docker.compose.depends_on")).contains(&service))
        .map(ApiContainer::display_name)
        .collect()
}

/// Why this container must not be recreated on its own, if it must not be.
///
/// Portainer's recreate is per container: it renames the old one aside, creates
/// a replacement with a new id, and destroys the original. Safe for a
/// standalone container, destructive for one anything else is attached to.
/// Both signals come out of the container list already fetched, so this costs
/// no extra request.
///
/// Refusing is the complete fix rather than half of one. Portainer's recreate
/// reuses the container's existing config, which still names the dead namespace
/// id, so recreating the dependents afterwards reproduces the breakage, and
/// `docker restart` fails too. Only `docker compose up -d` re-resolves
/// `service:x` to the new id, and this applet has no shell on the NAS.
fn dependency_block(target: &ApiContainer, all: &[ApiContainer]) -> Option<String> {
    let mut names = network_passengers(target, all);
    for name in compose_dependants(target, all) {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if names.is_empty() {
        return None;
    }
    names.sort();
    let shown = if names.len() > 3 {
        format!("{}, +{} more", names[..3].join(", "), names.len() - 3)
    } else {
        names.join(", ")
    };
    let plural = if names.len() == 1 { "" } else { "s" };
    let verb = if names.len() == 1 { "s" } else { "" };
    let object = if names.len() == 1 { "it" } else { "them" };
    let head = format!(
        "{} container{plural} depend{verb} on {} ({shown}). \
         Recreating it alone would give it a new id and break {object}.",
        names.len(),
        target.display_name()
    );
    let dir = target.stack_working_dir();
    Some(if dir.is_empty() {
        format!("{head} Update the whole stack instead (docker compose up -d).")
    } else {
        format!("{head} Update the stack instead: {dir}")
    })
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
            report.unreachable = e.starts_with("Could not reach");
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
        // `?all=1`: a *stopped* dependent is the one most at risk, since it
        // cannot be restarted once the container it rides has a new id.
        let containers: Vec<ApiContainer> = match conn
            .get(&format!(
                "/api/endpoints/{}/docker/containers/json?all=1",
                ep.id
            ))
            .await
            .and_then(|v| serde_json::from_value(v).map_err(|e| format!("containers: {e}")))
        {
            Ok(c) => c,
            Err(e) => {
                report.errors.push(e);
                continue;
            }
        };

        for c in &containers {
            if c.is_truenas_managed() {
                continue;
            }
            // Images pinned by digest or referenced by raw id can't drift.
            if c.image.contains('@') || c.image.starts_with("sha256:") || c.image.is_empty() {
                continue;
            }
            if !c.is_running() {
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
                        // Decided here so the popup can show the reason, and
                        // so the apply queue cannot pick it up by accident.
                        blocked: dependency_block(c, &containers),
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

    /// A container list entry, with only the fields the guard reads.
    fn container(
        id: &str,
        name: &str,
        state: &str,
        network_mode: &str,
        labels: &[(&str, &str)],
    ) -> ApiContainer {
        ApiContainer {
            id: id.to_string(),
            names: vec![format!("/{name}")],
            image: "example/image:latest".to_string(),
            image_id: format!("sha256:{name}"),
            labels: labels
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            state: state.to_string(),
            status: String::new(),
            host_config: ApiHostConfig {
                network_mode: network_mode.to_string(),
            },
        }
    }

    /// The 2026-09-09 outage: gluetun on :latest carrying qbittorrent and
    /// flaresolverr in its network namespace. Both are tag-pinned, so gluetun
    /// is the only one that ever appears as updatable — and it is the one that
    /// must not be recreated alone.
    fn vpn_stack() -> Vec<ApiContainer> {
        let gluetun_id = "31099bfa11e8";
        vec![
            container(
                gluetun_id,
                "gluetun",
                "running",
                "bridge",
                &[
                    ("com.docker.compose.project", "qbittorrent-vpn"),
                    ("com.docker.compose.service", "gluetun"),
                    (
                        "com.docker.compose.project.working_dir",
                        "/mnt/Homelab-Apps/Apps_Data/Dockge/Stacks/qbittorrent-vpn",
                    ),
                ],
            ),
            container(
                "aa",
                "qbittorrent",
                "running",
                &format!("container:{gluetun_id}"),
                &[
                    ("com.docker.compose.project", "qbittorrent-vpn"),
                    ("com.docker.compose.service", "qbittorrent"),
                    ("com.docker.compose.depends_on", "gluetun:service_healthy:false"),
                ],
            ),
            container(
                "bb",
                "flaresolverr",
                "exited",
                &format!("container:{gluetun_id}"),
                &[
                    ("com.docker.compose.project", "qbittorrent-vpn"),
                    ("com.docker.compose.service", "flaresolverr"),
                ],
            ),
            container("cc", "watchstate", "running", "bridge", &[]),
        ]
    }

    #[test]
    fn carrier_of_network_passengers_is_blocked() {
        let all = vpn_stack();
        let reason = dependency_block(&all[0], &all).expect("gluetun must be blocked");
        assert!(reason.contains("qbittorrent"), "{reason}");
        assert!(reason.contains("flaresolverr"), "{reason}");
        assert!(reason.contains("new id"), "{reason}");
        assert!(reason.contains("/Dockge/Stacks/qbittorrent-vpn"), "{reason}");
    }

    /// A stopped passenger is the case that cannot be restarted at all once
    /// the carrier's id changes, so it must still block.
    #[test]
    fn stopped_passenger_still_blocks() {
        let all = vpn_stack();
        assert_eq!(
            network_passengers(&all[0], &all),
            vec!["qbittorrent".to_string(), "flaresolverr".to_string()]
        );
    }

    /// Acceptance criterion: the guard must not block ordinary containers.
    #[test]
    fn standalone_container_is_not_blocked() {
        let all = vpn_stack();
        assert!(dependency_block(&all[3], &all).is_none());
    }

    #[test]
    fn declared_dependency_blocks_without_a_shared_namespace() {
        let all = vec![
            container(
                "d1",
                "db",
                "running",
                "bridge",
                &[
                    ("com.docker.compose.project", "app"),
                    ("com.docker.compose.service", "db"),
                ],
            ),
            container(
                "d2",
                "web",
                "running",
                "bridge",
                &[
                    ("com.docker.compose.project", "app"),
                    ("com.docker.compose.service", "web"),
                    ("com.docker.compose.depends_on", "db:service_started:true,cache:x:false"),
                ],
            ),
        ];
        let reason = dependency_block(&all[0], &all).expect("db must be blocked");
        assert!(reason.contains("web"), "{reason}");
        // No working_dir label, so the advice falls back to compose itself.
        assert!(reason.contains("docker compose up -d"), "{reason}");
    }

    #[test]
    fn dependency_in_another_project_is_not_a_dependency() {
        let all = vec![
            container(
                "e1",
                "one",
                "running",
                "bridge",
                &[
                    ("com.docker.compose.project", "alpha"),
                    ("com.docker.compose.service", "svc"),
                ],
            ),
            container(
                "e2",
                "two",
                "running",
                "bridge",
                &[
                    ("com.docker.compose.project", "beta"),
                    ("com.docker.compose.service", "other"),
                    ("com.docker.compose.depends_on", "svc:service_started:false"),
                ],
            ),
        ];
        assert!(dependency_block(&all[0], &all).is_none());
    }

    #[test]
    fn depends_on_is_parsed_out_of_its_condition_fields() {
        assert_eq!(parse_depends_on("gluetun:service_healthy:false"), vec!["gluetun"]);
        assert_eq!(parse_depends_on("a:x:false, b:y:true"), vec!["a", "b"]);
        assert!(parse_depends_on("").is_empty());
    }

    /// Only running containers are candidates; the list is fetched with
    /// `?all=1` purely so stopped dependents are visible.
    #[test]
    fn only_running_containers_are_candidates() {
        let all = vpn_stack();
        assert!(all[0].is_running());
        assert!(!all[2].is_running());
        // Older spelling, for a proxy that only forwards Status.
        let mut older = container("f1", "old", "", "bridge", &[]);
        older.status = "Up 3 days".to_string();
        assert!(older.is_running());
        older.status = "Exited (0) 2 hours ago".to_string();
        assert!(!older.is_running());
        // Neither field: keep updating rather than silently doing nothing.
        let neither = container("f2", "neither", "", "bridge", &[]);
        assert!(neither.is_running());
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
