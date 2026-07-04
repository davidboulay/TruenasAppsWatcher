// SPDX-License-Identifier: GPL-3.0-only
//
// TrueNAS apps backend.
//
// Talks to the TrueNAS SCALE REST API (`/api/v2.0`) over HTTPS with an API
// key. Queries the installed apps for pending upgrades (catalog version bumps
// and newer Docker images), and drives the `app.upgrade` / `app.pull_images`
// jobs to apply them. Long-running operations are middleware *jobs*: the call
// returns a job id which is then polled via `core.get_jobs`.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

/// How to reach the TrueNAS server. Persisted via cosmic-config and editable
/// from the applet's settings panel.
#[derive(Debug, Clone, Default)]
pub struct Connection {
    /// Host or URL, e.g. "192.168.1.100" or "https://truenas.local".
    pub base_url: String,
    pub api_key: String,
    /// Accept self-signed TLS certificates (the TrueNAS default).
    pub accept_invalid_certs: bool,
}

impl Connection {
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

    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, String> {
        let url = format!("{}{path}", self.normalized_base());
        let mut req = self.client()?.request(method, &url).bearer_auth(self.api_key.trim());
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("Could not reach TrueNAS ({e})"))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| format!("{path}: {e}"))?;
        if !status.is_success() {
            let snippet: String = text.chars().take(200).collect();
            return Err(match status.as_u16() {
                401 | 403 => "Authentication failed — check the API key".to_string(),
                _ => format!("{path}: HTTP {status}: {snippet}"),
            });
        }
        serde_json::from_str(&text).map_err(|e| format!("{path}: invalid JSON ({e})"))
    }

    async fn get(&self, path: &str) -> Result<Value, String> {
        self.request(reqwest::Method::GET, path, None).await
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value, String> {
        self.request(reqwest::Method::POST, path, Some(body)).await
    }
}

/// What kind of update an app has pending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateKind {
    /// A newer catalog version — applied with `app.upgrade`.
    App,
    /// The same version but newer Docker image(s) — applied with
    /// `app.pull_images` (typical for custom apps tracking a `latest` tag).
    Image,
    /// A container outside TrueNAS's apps with a newer image at the registry —
    /// applied by pulling `image` (with streamed progress), then recreating
    /// the container through Portainer.
    Container {
        endpoint_id: i64,
        container_id: String,
        image: String,
    },
}

/// A single app with a pending update.
#[derive(Debug, Clone)]
pub struct UpdateItem {
    /// The app name/id used by the API (e.g. "immich").
    pub name: String,
    /// Human title from the catalog metadata; falls back to `name`.
    pub title: String,
    /// Currently installed version (human-readable).
    pub current: String,
    /// Latest available catalog version. Empty for image updates.
    pub latest: String,
    pub kind: UpdateKind,
}

/// Progress events emitted while applying updates.
#[derive(Debug, Clone)]
pub enum InstallEvent {
    /// Overall completion fraction in `0.0..=1.0`.
    Progress(f32),
    /// All updates finished; `Err` carries a human-readable failure summary.
    Done(Result<(), String>),
}

/// The result of a check: pending updates plus any non-fatal errors so the UI
/// can show partial results.
#[derive(Debug, Clone, Default)]
pub struct AppsReport {
    /// Catalog upgrades (`upgrade_available`).
    pub upgrades: Vec<UpdateItem>,
    /// Docker image updates (`image_updates_available`) without a catalog bump.
    pub images: Vec<UpdateItem>,
    /// Total number of installed apps, for the "all up to date" summary.
    pub total_apps: usize,
    pub errors: Vec<String>,
}

impl AppsReport {
    pub fn total(&self) -> usize {
        self.upgrades.len() + self.images.len()
    }

    fn from_error(e: String) -> Self {
        Self {
            errors: vec![e],
            ..Self::default()
        }
    }
}

// The subset of `app.query` fields the applet cares about.
#[derive(Deserialize)]
struct RawApp {
    name: String,
    #[serde(default)]
    upgrade_available: bool,
    #[serde(default)]
    image_updates_available: bool,
    #[serde(default)]
    human_version: Option<String>,
    #[serde(default)]
    latest_version: Option<String>,
    #[serde(default)]
    metadata: Option<RawMetadata>,
}

#[derive(Deserialize)]
struct RawMetadata {
    #[serde(default)]
    title: Option<String>,
}

/// A middleware job, as returned by `core.get_jobs`.
#[derive(Deserialize)]
struct Job {
    state: String,
    #[serde(default)]
    progress: Option<JobProgress>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct JobProgress {
    #[serde(default)]
    percent: Option<f64>,
}

const JOB_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Upgrades pull container images; give each job plenty of time.
const JOB_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Poll a job until it finishes. `on_progress` receives the job's own
/// completion fraction (`0.0..=1.0`).
async fn wait_job(
    conn: &Connection,
    job_id: i64,
    on_progress: impl Fn(f32),
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + JOB_TIMEOUT;
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("Job {job_id} timed out"));
        }
        let jobs: Vec<Job> = serde_json::from_value(
            conn.get(&format!("/api/v2.0/core/get_jobs?id={job_id}")).await?,
        )
        .map_err(|e| format!("core.get_jobs: {e}"))?;
        let Some(job) = jobs.first() else {
            return Err(format!("Job {job_id} not found"));
        };
        match job.state.as_str() {
            "SUCCESS" => return Ok(()),
            "FAILED" | "ABORTED" | "ERROR" => {
                let detail = job.error.clone().unwrap_or_else(|| job.state.clone());
                return Err(detail.lines().next().unwrap_or("failed").to_string());
            }
            // WAITING / RUNNING — report progress and keep polling.
            _ => {
                if let Some(pct) = job.progress.as_ref().and_then(|p| p.percent)
                    && (0.0..=100.0).contains(&pct)
                {
                    on_progress((pct / 100.0) as f32);
                }
                tokio::time::sleep(JOB_POLL_INTERVAL).await;
            }
        }
    }
}

/// Query the installed apps and sort out which have updates pending.
async fn query_apps(conn: &Connection) -> Result<AppsReport, String> {
    let raw: Vec<RawApp> = serde_json::from_value(conn.get("/api/v2.0/app").await?)
        .map_err(|e| format!("app.query: unexpected response ({e})"))?;

    let mut report = AppsReport {
        total_apps: raw.len(),
        ..AppsReport::default()
    };
    for app in raw {
        let title = app
            .metadata
            .as_ref()
            .and_then(|m| m.title.clone())
            .unwrap_or_else(|| app.name.clone());
        let current = app.human_version.clone().unwrap_or_default();
        if app.upgrade_available {
            report.upgrades.push(UpdateItem {
                name: app.name,
                title,
                current,
                latest: app.latest_version.clone().unwrap_or_default(),
                kind: UpdateKind::App,
            });
        } else if app.image_updates_available {
            report.images.push(UpdateItem {
                name: app.name,
                title,
                current,
                latest: String::new(),
                kind: UpdateKind::Image,
            });
        }
    }
    let by_title =
        |a: &UpdateItem, b: &UpdateItem| a.title.to_lowercase().cmp(&b.title.to_lowercase());
    report.upgrades.sort_by(by_title);
    report.images.sort_by(by_title);
    Ok(report)
}

/// Check the server for pending app updates. With `refresh`, first ask TrueNAS
/// to re-sync its app catalog (the same thing its own daily cron does) so the
/// answer reflects the latest published versions. Never fails as a whole —
/// errors are collected in the report.
pub async fn check_apps(conn: Connection, refresh: bool) -> AppsReport {
    if !conn.is_configured() {
        return AppsReport::from_error(
            "Not configured — set the server address and API key in Settings".to_string(),
        );
    }

    let mut errors = Vec::new();
    if refresh {
        // A sync failure shouldn't hide the updates we can still read from the
        // server's current state, so log it and carry on. `catalog.sync` takes
        // no arguments, which the REST layer maps to GET (POST returns 405).
        match conn.get("/api/v2.0/catalog/sync").await {
            Ok(Value::Number(id)) if id.as_i64().is_some() => {
                if let Err(e) = wait_job(&conn, id.as_i64().unwrap(), |_| {}).await {
                    tracing::warn!("catalog sync reported: {e}");
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("catalog sync failed: {e}");
                errors.push(format!("Catalog refresh failed: {e}"));
            }
        }
    }

    match query_apps(&conn).await {
        Ok(mut report) => {
            report.errors.extend(errors);
            report
        }
        Err(e) => {
            errors.push(e);
            AppsReport {
                errors,
                ..AppsReport::default()
            }
        }
    }
}

/// Start the job that applies one pending update and return its id.
async fn start_update_job(conn: &Connection, item: &UpdateItem) -> Result<i64, String> {
    let (path, body) = match &item.kind {
        // `app_version` defaults to "latest" server-side; spelled out for clarity.
        UpdateKind::App => (
            "/api/v2.0/app/upgrade",
            json!({ "app_name": item.name, "options": { "app_version": "latest" } }),
        ),
        UpdateKind::Image => (
            "/api/v2.0/app/pull_images",
            json!({ "app_name": item.name }),
        ),
        UpdateKind::Container { .. } => {
            return Err("container updates go through Portainer".to_string());
        }
    };
    match conn.post(path, body).await? {
        Value::Number(id) if id.as_i64().is_some() => Ok(id.as_i64().unwrap()),
        other => Err(format!("unexpected job response: {other}")),
    }
}

/// Apply the given updates in the background, one at a time (parallel
/// upgrades would compete for the same Docker daemon and pool datasets),
/// streaming overall progress. TrueNAS app items run as middleware jobs on
/// `conn`; container items go through Portainer's recreate endpoint. Partial
/// failures are collected and reported together so one item failing doesn't
/// stop the rest.
pub fn apply_updates(
    conn: Connection,
    portainer: crate::docker::PortainerConnection,
    items: Vec<UpdateItem>,
) -> futures::channel::mpsc::UnboundedReceiver<InstallEvent> {
    let (tx, rx) = futures::channel::mpsc::unbounded();
    tokio::spawn(async move {
        let n = items.len().max(1) as f32;
        let mut errors = Vec::new();

        for (i, item) in items.iter().enumerate() {
            let base = i as f32 / n;
            let _ = tx.unbounded_send(InstallEvent::Progress(base));
            let result = match &item.kind {
                UpdateKind::Container {
                    endpoint_id,
                    container_id,
                    image,
                } => {
                    // Pull first with streamed layer progress (that's nearly
                    // all the wall time), then recreate without re-pulling.
                    let tx_p = tx.clone();
                    let pulled =
                        crate::docker::pull_image(&portainer, *endpoint_id, image, move |f| {
                            let _ = tx_p
                                .unbounded_send(InstallEvent::Progress(base + f * 0.9 / n));
                        })
                        .await;
                    match pulled {
                        Ok(()) => {
                            let _ =
                                tx.unbounded_send(InstallEvent::Progress(base + 0.9 / n));
                            crate::docker::recreate_container(
                                &portainer,
                                *endpoint_id,
                                container_id,
                                false,
                            )
                            .await
                        }
                        // If the streamed pull isn't possible (older daemon,
                        // registry auth…), fall back to the pull-inside-
                        // recreate path — no progress, but it works.
                        Err(e) => {
                            tracing::warn!("streamed pull failed, recreating with pull: {e}");
                            crate::docker::recreate_container(
                                &portainer,
                                *endpoint_id,
                                container_id,
                                true,
                            )
                            .await
                        }
                    }
                }
                _ => match start_update_job(&conn, item).await {
                    Ok(job_id) => {
                        let tx_p = tx.clone();
                        wait_job(&conn, job_id, move |f| {
                            let _ = tx_p.unbounded_send(InstallEvent::Progress(base + f / n));
                        })
                        .await
                    }
                    Err(e) => Err(e),
                },
            };
            if let Err(e) = result {
                errors.push(format!("{}: {e}", item.title));
            }
        }

        let _ = tx.unbounded_send(InstallEvent::Progress(1.0));
        let result = if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("\n"))
        };
        let _ = tx.unbounded_send(InstallEvent::Done(result));
    });
    rx
}
