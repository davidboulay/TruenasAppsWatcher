// SPDX-License-Identifier: GPL-3.0-only
//
// TrueNAS apps backend.
//
// Talks to the TrueNAS SCALE middleware over its JSON-RPC 2.0 WebSocket API
// (`/api/current`, TrueNAS 25.04 and newer), authenticating the session with
// an API key. Queries the installed apps for pending upgrades (catalog
// version bumps and newer Docker images), and drives the `app.upgrade` /
// `app.pull_images` jobs to apply them. Long-running operations are
// middleware *jobs*: the call returns a job id which is then polled via
// `core.get_jobs` on the same session.
//
// The REST layer (`/api/v2.0`) this used to speak is deprecated in 25.10 and
// gone in 26.04; TrueNAS flags every call to it.

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

/// Where the JSON-RPC API lives, relative to the server's base URL.
const API_PATH: &str = "/api/current";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest message accepted from the server. The apps list is the biggest
/// legitimate reply at a few hundred kilobytes for a full NAS.
const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

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

    /// Base with a scheme and no trailing slash; bare hosts default to https
    /// (the API accepts the self-signed certificate when the toggle is on).
    pub fn normalized_base(&self) -> String {
        let b = self.base_url.trim().trim_end_matches('/');
        if b.starts_with("http://") || b.starts_with("https://") {
            b.to_string()
        } else {
            format!("https://{b}")
        }
    }

    /// Base for opening the *web UI* in a browser. Unlike the API, a browser
    /// has no "accept self-signed certificate" setting — https on a bare host
    /// just throws a certificate warning — so this defaults to http unless
    /// the user explicitly typed an https:// URL.
    pub fn web_ui_base(&self) -> String {
        let b = self.base_url.trim().trim_end_matches('/');
        if b.starts_with("http://") || b.starts_with("https://") {
            b.to_string()
        } else {
            format!("http://{b}")
        }
    }

    /// The WebSocket URL of the JSON-RPC endpoint: `wss://` for an https base,
    /// `ws://` for a box deliberately put back on plain http.
    pub fn rpc_url(&self) -> String {
        let base = self.normalized_base();
        let ws = if let Some(rest) = base.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = base.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            format!("wss://{base}")
        };
        format!("{ws}{API_PATH}")
    }

    fn tls_connector(&self) -> Result<Connector, String> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("TLS: {e}"))?;
        let config = if self.accept_invalid_certs {
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider)))
                .with_no_client_auth()
        } else {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            builder.with_root_certificates(roots).with_no_client_auth()
        };
        Ok(Connector::Rustls(Arc::new(config)))
    }

    /// Open a WebSocket to the middleware and log the session in with the
    /// API key. Transport failures come back as "Could not reach TrueNAS
    /// (…)", which callers treat as transient.
    pub async fn connect(&self) -> Result<Session, String> {
        let url = self.rpc_url();
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_MESSAGE_BYTES));
        let connecting = tokio_tungstenite::connect_async_tls_with_config(
            url.as_str(),
            Some(config),
            false,
            Some(self.tls_connector()?),
        );
        let (socket, _response) = tokio::time::timeout(CONNECT_TIMEOUT, connecting)
            .await
            .map_err(|_| "Could not reach TrueNAS (timed out)".to_string())?
            .map_err(connect_error)?;
        let mut session = Session { socket, next_id: 0 };
        // `auth.login_with_api_key` is the API-key front door of the JSON-RPC
        // API: the server resolves the key's user and runs the API_KEY_PLAIN
        // mechanism of `auth.login_ex` on its behalf.
        let ok = session
            .call("auth.login_with_api_key", json!([self.api_key.trim()]))
            .await?;
        if ok != Value::Bool(true) {
            return Err("Authentication failed — check the API key".to_string());
        }
        Ok(session)
    }
}

/// Accepts whatever certificate the server presents — the "self-signed
/// certificate" toggle. Signatures are still checked, so the connection is
/// at least talking to whoever holds the key for that certificate.
#[derive(Debug)]
struct AcceptAnyCert(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Why a connection attempt failed, in the words the UI shows. Everything
/// that says nothing about the request itself — no route, refused, a proxy's
/// 502/503/504, TLS trouble — is "Could not reach", so automatic checks
/// retry quietly instead of alarming.
fn connect_error(e: tungstenite::Error) -> String {
    match e {
        tungstenite::Error::Http(response) => {
            let status = response.status().as_u16();
            match status {
                401 | 403 => "Authentication failed — check the API key".to_string(),
                404 => format!(
                    "No JSON-RPC endpoint at {API_PATH} — TrueNAS 25.04 or newer is required"
                ),
                408 | 502 | 503 | 504 | 522 | 524 => {
                    format!("Could not reach TrueNAS (gateway returned HTTP {status})")
                }
                _ => format!("Could not reach TrueNAS (HTTP {status} during handshake)"),
            }
        }
        tungstenite::Error::Url(e) => format!("Bad server address: {e}"),
        tungstenite::Error::Tls(e) => {
            let text = e.to_string();
            if text.contains("certificate") || text.contains("Certificate") {
                "Could not reach TrueNAS (certificate not trusted)".to_string()
            } else {
                format!("Could not reach TrueNAS (TLS: {text})")
            }
        }
        tungstenite::Error::Io(e) => format!("Could not reach TrueNAS ({})", io_error_text(&e)),
        other => format!("Could not reach TrueNAS ({other})"),
    }
}

fn io_error_text(e: &std::io::Error) -> String {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::ConnectionRefused => "connection refused".to_string(),
        ErrorKind::ConnectionReset => "connection reset".to_string(),
        ErrorKind::TimedOut => "timed out".to_string(),
        ErrorKind::UnexpectedEof => "connection closed".to_string(),
        _ => {
            let text = e.to_string();
            if text.contains("failed to lookup") || text.contains("Name or service") {
                "host not found".to_string()
            } else {
                text
            }
        }
    }
}

/// An authenticated JSON-RPC session on one WebSocket.
pub struct Session {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
}

impl Session {
    /// Make one call and wait for its reply. Notifications from the server
    /// (collection updates and the like) are skipped; this client subscribes
    /// to nothing.
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.socket
            .send(Message::Text(request.to_string().into()))
            .await
            .map_err(|e| format!("Could not reach TrueNAS ({})", transport_error(e)))?;
        loop {
            let next = tokio::time::timeout(CALL_TIMEOUT, self.socket.next())
                .await
                .map_err(|_| format!("Could not reach TrueNAS ({method}: no reply)"))?;
            let message = match next {
                None => return Err("Could not reach TrueNAS (connection closed)".to_string()),
                Some(Err(e)) => {
                    return Err(format!("Could not reach TrueNAS ({})", transport_error(e)));
                }
                Some(Ok(m)) => m,
            };
            let text = match message {
                Message::Text(text) => text,
                Message::Close(_) => {
                    return Err("Could not reach TrueNAS (connection closed by TrueNAS)".to_string());
                }
                // Pings are answered by the library; pongs, binary frames
                // and raw frames carry nothing for us.
                _ => continue,
            };
            let reply: Value = serde_json::from_str(&text)
                .map_err(|e| format!("{method}: invalid JSON from TrueNAS ({e})"))?;
            if reply.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = reply.get("error") {
                return Err(rpc_error(method, error));
            }
            return Ok(reply.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Say goodbye. Errors are ignored: the session is over either way.
    pub async fn close(mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(2), self.socket.close(None)).await;
    }
}

fn transport_error(e: tungstenite::Error) -> String {
    match e {
        tungstenite::Error::Io(e) => io_error_text(&e),
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
            "connection closed".to_string()
        }
        tungstenite::Error::Capacity(e) => format!("reply too large: {e}"),
        other => other.to_string(),
    }
}

/// A JSON-RPC error object in a sentence. The middleware puts the useful
/// text in `data.reason`; `data.errname` says whether it was a permissions
/// problem (a key without the role for this call).
fn rpc_error(method: &str, error: &Value) -> String {
    let code = error.get("code").and_then(Value::as_i64);
    let data = error.get("data");
    let reason = data
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str)
        .or_else(|| error.get("message").and_then(Value::as_str))
        .unwrap_or("error");
    let reason = reason.lines().next().unwrap_or("error").trim();
    let reason: String = reason.chars().take(300).collect();
    if code == Some(-32601) {
        return format!("{method}: no such method on this TrueNAS version");
    }
    format!("{method}: {reason}")
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
    /// Why this item must not be applied from here, if it must not be.
    ///
    /// Set only for containers something else depends on: Portainer's recreate
    /// replaces the container with a new id, which breaks anything pinned to
    /// the old one. Such an item is still *listed* — an update that exists must
    /// be visible even when this applet is the wrong tool for it — but it is
    /// kept out of the apply queue. See `docker::dependency_block`.
    #[allow(clippy::struct_field_names)]
    pub blocked: Option<String>,
}

impl UpdateItem {
    pub fn is_blocked(&self) -> bool {
        self.blocked.is_some()
    }
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
    /// The query failed at the transport level (server or network down) —
    /// automatic checks treat this as transient and retry quietly rather
    /// than alarming (e.g. Wi-Fi not up yet right after login).
    pub unreachable: bool,
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
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
/// Upgrades pull container images; give each job plenty of time.
const JOB_TIMEOUT: Duration = Duration::from_secs(30 * 60);

fn is_unreachable(e: &str) -> bool {
    e.starts_with("Could not reach")
}

/// Make sure there is a live session, opening one if there isn't.
async fn ensure_session(
    conn: &Connection,
    session: &mut Option<Session>,
) -> Result<(), String> {
    if session.is_none() {
        *session = Some(conn.connect().await?);
    }
    Ok(())
}

/// Poll a job until it finishes. `on_progress` receives the job's own
/// completion fraction (`0.0..=1.0`).
///
/// A dropped connection says nothing about the job, which is very likely
/// still running on the NAS, so the poll comes back on a fresh session and
/// keeps going; the deadline is the only honest limit on an image pull.
async fn wait_job(
    conn: &Connection,
    session: &mut Option<Session>,
    job_id: i64,
    on_progress: impl Fn(f32),
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + JOB_TIMEOUT;
    let mut missing = 0;
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("Job {job_id} timed out"));
        }
        if let Err(e) = ensure_session(conn, session).await {
            if is_unreachable(&e) {
                tokio::time::sleep(RECONNECT_DELAY).await;
                continue;
            }
            return Err(e);
        }
        let Some(live) = session.as_mut() else { continue };
        let reply = match live
            .call("core.get_jobs", json!([[["id", "=", job_id]]]))
            .await
        {
            Ok(v) => v,
            Err(e) if is_unreachable(&e) => {
                tracing::warn!("lost the session while watching job {job_id}: {e}");
                *session = None;
                tokio::time::sleep(RECONNECT_DELAY).await;
                continue;
            }
            Err(e) => return Err(e),
        };
        let jobs: Vec<Job> =
            serde_json::from_value(reply).map_err(|e| format!("core.get_jobs: {e}"))?;
        let Some(job) = jobs.first() else {
            // A job can take a moment to appear right after it is created;
            // only a persistent absence is a failure.
            missing += 1;
            if missing >= 3 {
                return Err(format!("Job {job_id} not found"));
            }
            tokio::time::sleep(JOB_POLL_INTERVAL).await;
            continue;
        };
        missing = 0;
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
async fn query_apps(session: &mut Session) -> Result<AppsReport, String> {
    let raw: Vec<RawApp> = serde_json::from_value(session.call("app.query", json!([])).await?)
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
                blocked: None,
            });
        } else if app.image_updates_available {
            report.images.push(UpdateItem {
                name: app.name,
                title,
                current,
                latest: String::new(),
                kind: UpdateKind::Image,
                blocked: None,
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
    let mut session = match conn.connect().await {
        Ok(s) => Some(s),
        Err(e) => {
            let unreachable = is_unreachable(&e);
            return AppsReport {
                errors: vec![e],
                unreachable,
                ..AppsReport::default()
            };
        }
    };

    if refresh {
        // A sync failure shouldn't hide the updates we can still read from the
        // server's current state, so log it and carry on. `catalog.sync` is a
        // job: the call hands back an id and the work happens in the background.
        let started = match session.as_mut() {
            Some(live) => live.call("catalog.sync", json!([])).await,
            None => Err("Could not reach TrueNAS (no session)".to_string()),
        };
        match started {
            Ok(Value::Number(id)) if id.as_i64().is_some() => {
                if let Err(e) = wait_job(&conn, &mut session, id.as_i64().unwrap(), |_| {}).await
                {
                    tracing::warn!("catalog sync reported: {e}");
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("catalog sync failed: {e}");
                errors.push(format!("Catalog refresh failed: {e}"));
                if is_unreachable(&e) {
                    session = None;
                }
            }
        }
    }

    let queried = match ensure_session(&conn, &mut session).await {
        Ok(()) => match session.as_mut() {
            Some(live) => query_apps(live).await,
            None => Err("Could not reach TrueNAS (no session)".to_string()),
        },
        Err(e) => Err(e),
    };
    if let Some(live) = session.take() {
        live.close().await;
    }

    match queried {
        Ok(mut report) => {
            report.errors.extend(errors);
            report
        }
        Err(e) => {
            let unreachable = is_unreachable(&e);
            errors.push(e);
            AppsReport {
                errors,
                unreachable,
                ..AppsReport::default()
            }
        }
    }
}

/// Start the job that applies one pending update and return its id.
async fn start_update_job(session: &mut Session, item: &UpdateItem) -> Result<i64, String> {
    let (method, params) = match &item.kind {
        // `app_version` defaults to "latest" server-side; spelled out for clarity.
        UpdateKind::App => (
            "app.upgrade",
            json!([item.name, { "app_version": "latest" }]),
        ),
        UpdateKind::Image => ("app.pull_images", json!([item.name, { "redeploy": true }])),
        UpdateKind::Container { .. } => {
            return Err("container updates go through Portainer".to_string());
        }
    };
    match session.call(method, params).await? {
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
        // One session serves the whole queue; `wait_job` reopens it if the
        // line drops mid-upgrade.
        let mut session: Option<Session> = None;

        for (i, item) in items.iter().enumerate() {
            let base = i as f32 / n;
            let _ = tx.unbounded_send(InstallEvent::Progress(base));
            // Callers already filter these out, so arriving here would be a
            // bug rather than a user action. Refuse anyway: the recreate below
            // destroys the old container, and the cost of being wrong is an
            // outage (see the 2026-09-09 gluetun incident).
            if let Some(reason) = &item.blocked {
                errors.push(format!("{}: {reason}", item.title));
                continue;
            }
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
                _ => {
                    let started = match ensure_session(&conn, &mut session).await {
                        Ok(()) => match session.as_mut() {
                            Some(live) => start_update_job(live, item).await,
                            None => Err("Could not reach TrueNAS (no session)".to_string()),
                        },
                        Err(e) => Err(e),
                    };
                    match started {
                        Ok(job_id) => {
                            let tx_p = tx.clone();
                            wait_job(&conn, &mut session, job_id, move |f| {
                                let _ =
                                    tx_p.unbounded_send(InstallEvent::Progress(base + f / n));
                            })
                            .await
                        }
                        Err(e) => {
                            if is_unreachable(&e) {
                                session = None;
                            }
                            Err(e)
                        }
                    }
                }
            };
            if let Err(e) = result {
                errors.push(format!("{}: {e}", item.title));
            }
        }

        if let Some(live) = session.take() {
            live.close().await;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(base: &str) -> Connection {
        Connection {
            base_url: base.to_string(),
            api_key: "1-key".to_string(),
            accept_invalid_certs: true,
        }
    }

    #[test]
    fn rpc_url_follows_the_scheme_of_the_base() {
        assert_eq!(conn("truenas.local").rpc_url(), "wss://truenas.local/api/current");
        assert_eq!(conn("https://nas:8443/").rpc_url(), "wss://nas:8443/api/current");
        assert_eq!(conn("http://192.168.1.10").rpc_url(), "ws://192.168.1.10/api/current");
    }

    #[test]
    fn rpc_errors_read_the_middleware_reason() {
        let e = json!({ "code": -32001, "message": "Method call error",
                        "data": { "error": 13, "errname": "EACCES",
                                  "reason": "Not authorized\nmore detail" } });
        assert_eq!(rpc_error("app.upgrade", &e), "app.upgrade: Not authorized");
        let missing = json!({ "code": -32601, "message": "Method does not exist" });
        assert_eq!(
            rpc_error("app.query", &missing),
            "app.query: no such method on this TrueNAS version"
        );
        let bare = json!({ "code": -32602, "message": "Invalid params" });
        assert_eq!(rpc_error("x", &bare), "x: Invalid params");
    }

    #[test]
    fn transport_failures_are_unreachable() {
        assert!(is_unreachable("Could not reach TrueNAS (connection refused)"));
        assert!(!is_unreachable("Authentication failed — check the API key"));
    }
}
