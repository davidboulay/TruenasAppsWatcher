// SPDX-License-Identifier: GPL-3.0-only

use std::sync::LazyLock;
use std::time::Duration;

use cosmic::{
    Application, Element, Task, app,
    applet::{cosmic_panel_config::PanelAnchor, menu_button, padded_control},
    cctk::sctk::reexports::calloop,
    cosmic_theme::Spacing,
    iced::{
        Alignment, Length, Subscription, stream,
        futures::{SinkExt, StreamExt, channel::mpsc},
        platform_specific::shell::wayland::commands::popup::{destroy_popup, get_popup},
        widget::{column, row},
        window,
    },
    theme,
    widget::{
        Id, autosize, button, container, divider, icon, scrollable, secure_input, settings, text,
        text_input, toggler,
    },
};

use cosmic::applet::token::subscription::{
    TokenRequest, TokenUpdate, activation_token_subscription,
};
use cosmic::cosmic_config::{self, ConfigGet, ConfigSet};

use crate::backend::{self, AppsReport, Connection, UpdateItem};
use crate::docker::{self, ContainerReport, PortainerConnection};
use crate::updater;

static AUTOSIZE_MAIN_ID: LazyLock<Id> = LazyLock::new(|| Id::new("truenas-apps-autosize-main"));

/// Bump if the persisted config layout ever changes incompatibly.
const CONFIG_VERSION: u64 = 1;
/// Config keys for the TrueNAS connection.
const SERVER_URL_KEY: &str = "server-url";
const API_KEY_KEY: &str = "api-key";
const ACCEPT_INVALID_CERTS_KEY: &str = "accept-invalid-certs";
/// Config keys for the optional Portainer connection (unmanaged containers).
const PORTAINER_URL_KEY: &str = "portainer-url";
const PORTAINER_API_KEY_KEY: &str = "portainer-api-key";
/// Config key for the "automatically update the applet" toggle.
const AUTO_UPDATE_KEY: &str = "auto-update";

/// Where the applet's own version sits relative to the latest GitHub release.
#[derive(Debug, Clone)]
enum ReleaseStatus {
    /// No check has completed yet.
    Unknown,
    Checking,
    UpToDate,
    /// A newer release exists; holds its tag (e.g. "v0.2.0").
    Available(String),
    Error(String),
}

// Status badges with the colour baked in: a NAS chassis (drive bays up top)
// in TrueNAS blue with an up-arrow when updates are pending, and a muted blue
// one with a checkmark when everything is up to date — deliberately unlike
// the yellow/green seal of the system-updates applet.
const ICON_AVAILABLE_SVG: &[u8] = include_bytes!("../icons/updates-available.svg");
const ICON_UP_TO_DATE_SVG: &[u8] = include_bytes!("../icons/updates-ok.svg");

pub struct Window {
    core: app::Core,
    popup: Option<window::Id>,
    token_tx: Option<calloop::channel::Sender<TokenRequest>>,
    checking: bool,
    report: AppsReport,
    last_checked: Option<String>,
    /// The saved TrueNAS connection currently in use.
    conn: Connection,
    /// The saved (optional) Portainer connection for unmanaged containers.
    portainer: PortainerConnection,
    /// Latest unmanaged-container results; checked on its own (slower)
    /// schedule because registry lookups are rate-limited.
    containers: ContainerReport,
    checking_containers: bool,
    /// In-progress edits of the connection fields in the settings panel.
    edit_url: String,
    edit_key: String,
    edit_insecure: bool,
    edit_portainer_url: String,
    edit_portainer_key: String,
    /// Whether the API-key fields show their text. Off whenever a key is
    /// already saved; the fields' eye button toggles them.
    show_truenas_key: bool,
    show_portainer_key: bool,
    /// Set while updates are being applied; `install_progress` is the overall
    /// fraction (None until the first progress event arrives).
    installing: bool,
    install_progress: Option<f32>,
    install_error: Option<String>,
    /// Persisted settings handle (None if the config backend is unavailable).
    config: Option<cosmic_config::Config>,
    /// Whether to auto-install newer releases of the applet itself.
    auto_update: bool,
    /// True while the settings panel is showing instead of the updates list.
    show_settings: bool,
    /// Latest-release status for the applet's own version.
    release: ReleaseStatus,
    /// True while a self-update download is in progress.
    self_updating: bool,
    /// True when the in-flight check came from the "Check for updates"
    /// button — its failures always show, unlike automatic checks.
    last_check_manual: bool,
    /// Whether any apps check has succeeded since launch.
    ever_succeeded: bool,
    /// Consecutive quiet retries after transient "server unreachable"
    /// failures of automatic checks (e.g. Wi-Fi not up yet after login).
    silent_retries: u32,
    silent_container_retries: u32,
    /// A persistent connectivity error worth showing (manual check failed,
    /// or the quiet retries ran out).
    check_error: Option<String>,
}

/// Quiet 20-second retries before an unreachable server is reported —
/// roughly five minutes of grace after login.
const MAX_SILENT_RETRIES: u32 = 15;

#[derive(Debug, Clone)]
pub enum Message {
    TogglePopup,
    CloseRequested(window::Id),
    /// Run a TrueNAS apps check. `refresh` asks TrueNAS to re-sync its app
    /// catalog first; when false we only read the server's current state.
    Check {
        refresh: bool,
    },
    Checked(AppsReport),
    /// Run an unmanaged-container check via Portainer.
    CheckContainers,
    ContainersChecked(ContainerReport),
    /// Manual "Check for updates": both of the above.
    CheckAll,
    /// Apply all pending updates (app upgrades + image pulls).
    Install,
    InstallProgress(f32),
    Installed(Result<(), String>),
    /// Open the TrueNAS web UI's Apps page in the browser.
    OpenWebUi,
    /// Show/hide the settings panel.
    ToggleSettings,
    /// Edits to the connection fields in the settings panel.
    EditUrl(String),
    EditApiKey(String),
    EditInsecure(bool),
    EditPortainerUrl(String),
    EditPortainerKey(String),
    /// The eye buttons on the API-key fields.
    ToggleShowTruenasKey,
    ToggleShowPortainerKey,
    /// Persist the edited connection and re-check against it.
    SaveConnection,
    /// Check GitHub for a newer release of the applet.
    CheckRelease,
    ReleaseChecked(Result<String, String>),
    SetAutoUpdate(bool),
    /// Download and install the given release tag of the applet, then relaunch.
    SelfUpdate(String),
    /// Ok carries the path of the replaced binary to relaunch.
    SelfUpdated(Result<std::path::PathBuf, String>),
    Token(TokenUpdate),
}

impl Window {
    fn run_check(&mut self, refresh: bool) -> app::Task<Message> {
        if self.checking || !self.conn.is_configured() {
            return Task::none();
        }
        self.checking = true;
        let conn = self.conn.clone();
        cosmic::task::future(async move {
            let report = backend::check_apps(conn, refresh).await;
            cosmic::Action::App(Message::Checked(report))
        })
    }

    fn run_container_check(&mut self) -> app::Task<Message> {
        if self.checking_containers || !self.portainer.is_configured() {
            return Task::none();
        }
        self.checking_containers = true;
        let portainer = self.portainer.clone();
        cosmic::task::future(async move {
            let report = docker::check_containers(portainer).await;
            cosmic::Action::App(Message::ContainersChecked(report))
        })
    }

    /// Everything pending: TrueNAS app updates plus unmanaged containers.
    fn total_updates(&self) -> usize {
        self.report.total() + self.containers.updates.len()
    }

    /// Query GitHub for the latest release tag in the background.
    fn check_release() -> app::Task<Message> {
        cosmic::task::future(async move {
            cosmic::Action::App(Message::ReleaseChecked(updater::latest_release().await))
        })
    }

    /// Download and install the given release tag in the background.
    fn do_self_update(tag: String) -> app::Task<Message> {
        cosmic::task::future(async move {
            cosmic::Action::App(Message::SelfUpdated(updater::self_update(&tag).await))
        })
    }

    /// Launch a command using an activation token so it is focused correctly.
    fn spawn_with_token(&self, exec: &str) {
        if let Some(tx) = self.token_tx.as_ref() {
            let _ = tx.send(TokenRequest {
                app_id: Self::APP_ID.to_string(),
                exec: exec.to_string(),
            });
        } else {
            tracing::error!("activation token channel unavailable");
        }
    }

    /// The coloured status badge, sized for the given pixel size.
    fn status_icon(&self, size: u16) -> cosmic::widget::icon::Icon {
        let bytes: &'static [u8] = if self.total_updates() > 0 || !self.conn.is_configured() {
            ICON_AVAILABLE_SVG
        } else {
            ICON_UP_TO_DATE_SVG
        };
        icon::from_svg_bytes(bytes).icon().size(size)
    }

    fn section(&self, title: &str, items: &[UpdateItem]) -> Option<Element<'_, Message>> {
        if items.is_empty() {
            return None;
        }
        let Spacing { space_xxs, .. } = theme::active().cosmic().spacing;

        let mut col = column![text::heading(format!("{title} ({})", items.len()))].spacing(space_xxs);
        for item in items {
            let mut info = column![text::body(item.title.clone())];
            let secondary = if item.latest.is_empty() {
                // Image update: same catalog version, newer image.
                if item.current.is_empty() {
                    "New image available".to_string()
                } else {
                    format!("{} — new image available", item.current)
                }
            } else if item.current.is_empty() {
                item.latest.clone()
            } else {
                format!("{} → {}", item.current, item.latest)
            };
            if !secondary.is_empty() {
                info = info.push(text::caption(secondary));
            }
            col = col.push(padded_control(info).padding([space_xxs, 0]));
        }
        Some(col.into())
    }
}

impl cosmic::Application for Window {
    type Message = Message;
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = ();
    const APP_ID: &'static str = "com.github.davidboulay.CosmicAppletTruenasApps";

    fn init(core: app::Core, _flags: Self::Flags) -> (Self, app::Task<Self::Message>) {
        let config = cosmic_config::Config::new(Self::APP_ID, CONFIG_VERSION).ok();
        let get_string = |key: &str| {
            config
                .as_ref()
                .and_then(|c| c.get::<String>(key).ok())
                .unwrap_or_default()
        };
        let conn = Connection {
            base_url: get_string(SERVER_URL_KEY),
            api_key: get_string(API_KEY_KEY),
            // TrueNAS ships with a self-signed certificate, so default to
            // accepting one; the settings toggle can turn this off.
            accept_invalid_certs: config
                .as_ref()
                .and_then(|c| c.get::<bool>(ACCEPT_INVALID_CERTS_KEY).ok())
                .unwrap_or(true),
        };
        let portainer = PortainerConnection {
            base_url: get_string(PORTAINER_URL_KEY),
            api_key: get_string(PORTAINER_API_KEY_KEY),
            accept_invalid_certs: config
                .as_ref()
                .and_then(|c| c.get::<bool>(ACCEPT_INVALID_CERTS_KEY).ok())
                .unwrap_or(true),
        };
        let auto_update = config
            .as_ref()
            .and_then(|c| c.get::<bool>(AUTO_UPDATE_KEY).ok())
            .unwrap_or(false);

        let mut window = Self {
            core,
            popup: None,
            token_tx: None,
            checking: false,
            report: AppsReport::default(),
            last_checked: None,
            containers: ContainerReport::default(),
            checking_containers: false,
            edit_url: conn.base_url.clone(),
            edit_key: conn.api_key.clone(),
            edit_insecure: conn.accept_invalid_certs,
            edit_portainer_url: portainer.base_url.clone(),
            edit_portainer_key: portainer.api_key.clone(),
            // Only show key text while none is saved yet (first setup).
            show_truenas_key: conn.api_key.is_empty(),
            show_portainer_key: portainer.api_key.is_empty(),
            conn,
            portainer,
            installing: false,
            install_progress: None,
            install_error: None,
            config,
            auto_update,
            // Until a connection is configured, open straight into settings.
            show_settings: false,
            release: ReleaseStatus::Unknown,
            self_updating: false,
            last_check_manual: false,
            ever_succeeded: false,
            silent_retries: 0,
            silent_container_retries: 0,
            check_error: None,
        };
        window.show_settings = !window.conn.is_configured();
        // Populate counts on startup (plain query, no catalog sync), and learn
        // whether a newer applet release exists (auto-updating if enabled).
        let task = Task::batch([
            window.run_check(false),
            window.run_container_check(),
            Self::check_release(),
        ]);
        (window, task)
    }

    fn core(&self) -> &app::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut app::Core {
        &mut self.core
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }

    fn subscription(&self) -> Subscription<Message> {
        // Re-query the server periodically so the badge stays current. TrueNAS
        // syncs its app catalog on its own daily cron, so a plain query (no
        // catalog sync) is enough to pick up newly published versions.
        fn periodic_check() -> Subscription<Message> {
            const INTERVAL: Duration = Duration::from_secs(30 * 60); // every 30 min
            Subscription::run_with("truenas-apps-periodic-check", |_| {
                stream::channel(1, |mut output: mpsc::Sender<Message>| async move {
                    let mut timer = tokio::time::interval(INTERVAL);
                    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    // The first tick is immediate; skip it since init() already
                    // ran a check on startup.
                    timer.tick().await;
                    loop {
                        timer.tick().await;
                        if output.send(Message::Check { refresh: false }).await.is_err() {
                            break;
                        }
                    }
                })
            })
        }

        // Unmanaged containers are checked far less often: every image lookup
        // hits its registry, and Docker Hub rate-limits anonymous requests.
        // Manual "Check for updates" also runs this on demand.
        fn periodic_container_check() -> Subscription<Message> {
            const INTERVAL: Duration = Duration::from_secs(6 * 60 * 60); // 6 hours
            Subscription::run_with("truenas-apps-container-check", |_| {
                stream::channel(1, |mut output: mpsc::Sender<Message>| async move {
                    let mut timer = tokio::time::interval(INTERVAL);
                    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    // First tick is immediate; skip it since init() already checked.
                    timer.tick().await;
                    loop {
                        timer.tick().await;
                        if output.send(Message::CheckContainers).await.is_err() {
                            break;
                        }
                    }
                })
            })
        }

        // Periodically check whether a newer applet release is out (and auto-
        // update if the user enabled it). Far less frequent than the app check
        // since releases are rare.
        fn periodic_release_check() -> Subscription<Message> {
            const INTERVAL: Duration = Duration::from_secs(6 * 60 * 60); // 6 hours
            Subscription::run_with("truenas-apps-release-check", |_| {
                stream::channel(1, |mut output: mpsc::Sender<Message>| async move {
                    let mut timer = tokio::time::interval(INTERVAL);
                    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    // First tick is immediate; skip it since init() already checked.
                    timer.tick().await;
                    loop {
                        timer.tick().await;
                        if output.send(Message::CheckRelease).await.is_err() {
                            break;
                        }
                    }
                })
            })
        }

        Subscription::batch([
            activation_token_subscription(0).map(Message::Token),
            periodic_check(),
            periodic_container_check(),
            periodic_release_check(),
        ])
    }

    fn update(&mut self, message: Self::Message) -> app::Task<Self::Message> {
        match message {
            Message::TogglePopup => {
                if let Some(p) = self.popup.take() {
                    destroy_popup(p)
                } else {
                    let new_id = window::Id::unique();
                    self.popup = Some(new_id);
                    let popup_settings = self.core.applet.get_popup_settings(
                        self.core.main_window_id().unwrap(),
                        new_id,
                        None,
                        None,
                        None,
                    );
                    get_popup(popup_settings)
                }
            }
            Message::CloseRequested(id) => {
                if Some(id) == self.popup {
                    self.popup = None;
                }
                Task::none()
            }
            Message::Check { refresh } => {
                self.last_check_manual = false;
                self.run_check(refresh)
            }
            Message::CheckContainers => {
                self.last_check_manual = false;
                self.run_container_check()
            }
            Message::CheckAll => {
                self.last_check_manual = true;
                Task::batch([self.run_check(true), self.run_container_check()])
            }
            Message::Checked(report) => {
                self.checking = false;
                if report.unreachable {
                    // Keep whatever we last knew instead of clobbering it.
                    // Automatic checks retry quietly first — right after
                    // login the network is often not up yet, and a red
                    // "no internet" flash helps nobody.
                    if !self.last_check_manual && self.silent_retries < MAX_SILENT_RETRIES {
                        self.silent_retries += 1;
                        return cosmic::task::future(async move {
                            tokio::time::sleep(Duration::from_secs(20)).await;
                            cosmic::Action::App(Message::Check { refresh: false })
                        });
                    }
                    self.check_error = report.errors.first().cloned();
                    return Task::none();
                }
                self.ever_succeeded = true;
                self.silent_retries = 0;
                self.check_error = None;
                self.report = report;
                self.last_checked = Some(
                    jiff::Zoned::now()
                        .strftime("%H:%M")
                        .to_string(),
                );
                Task::none()
            }
            Message::ContainersChecked(report) => {
                self.checking_containers = false;
                if report.unreachable {
                    if !self.last_check_manual
                        && self.silent_container_retries < MAX_SILENT_RETRIES
                    {
                        self.silent_container_retries += 1;
                        return cosmic::task::future(async move {
                            tokio::time::sleep(Duration::from_secs(20)).await;
                            cosmic::Action::App(Message::CheckContainers)
                        });
                    }
                    // Persistent or manual: show it with the report errors.
                    self.containers.errors = report.errors;
                    return Task::none();
                }
                self.silent_container_retries = 0;
                self.containers = report;
                Task::none()
            }
            Message::Install => {
                if self.installing || self.checking || self.total_updates() == 0 {
                    return Task::none();
                }
                self.installing = true;
                self.install_progress = None;
                self.install_error = None;
                let items: Vec<UpdateItem> = self
                    .report
                    .upgrades
                    .iter()
                    .chain(self.report.images.iter())
                    .chain(self.containers.updates.iter())
                    .cloned()
                    .collect();
                let conn = self.conn.clone();
                let portainer = self.portainer.clone();
                cosmic::task::stream(backend::apply_updates(conn, portainer, items).map(|ev| {
                    cosmic::Action::App(match ev {
                        backend::InstallEvent::Progress(p) => Message::InstallProgress(p),
                        backend::InstallEvent::Done(r) => Message::Installed(r),
                    })
                }))
            }
            Message::InstallProgress(p) => {
                self.install_progress = Some(p);
                Task::none()
            }
            Message::Installed(result) => {
                self.installing = false;
                self.install_progress = None;
                self.install_error = result.err();
                // Re-query so the counts/badge drop immediately.
                Task::batch([self.run_check(false), self.run_container_check()])
            }
            Message::OpenWebUi => {
                if self.conn.is_configured() {
                    let url = format!("{}/ui/apps/installed", self.conn.web_ui_base());
                    self.spawn_with_token(&format!("xdg-open {url}"));
                }
                Task::none()
            }
            Message::ToggleSettings => {
                self.show_settings = !self.show_settings;
                if self.show_settings {
                    // Refresh the edit fields from the saved state and the
                    // release status each time the panel opens.
                    self.edit_url = self.conn.base_url.clone();
                    self.edit_key = self.conn.api_key.clone();
                    self.edit_insecure = self.conn.accept_invalid_certs;
                    self.edit_portainer_url = self.portainer.base_url.clone();
                    self.edit_portainer_key = self.portainer.api_key.clone();
                    // Saved keys come back hidden each time the panel opens.
                    self.show_truenas_key = self.conn.api_key.is_empty();
                    self.show_portainer_key = self.portainer.api_key.is_empty();
                    if !matches!(self.release, ReleaseStatus::Checking) {
                        self.release = ReleaseStatus::Checking;
                        return Self::check_release();
                    }
                }
                Task::none()
            }
            Message::EditUrl(v) => {
                self.edit_url = v;
                Task::none()
            }
            Message::EditApiKey(v) => {
                self.edit_key = v;
                Task::none()
            }
            Message::EditInsecure(on) => {
                self.edit_insecure = on;
                Task::none()
            }
            Message::EditPortainerUrl(v) => {
                self.edit_portainer_url = v;
                Task::none()
            }
            Message::EditPortainerKey(v) => {
                self.edit_portainer_key = v;
                Task::none()
            }
            Message::ToggleShowTruenasKey => {
                self.show_truenas_key = !self.show_truenas_key;
                Task::none()
            }
            Message::ToggleShowPortainerKey => {
                self.show_portainer_key = !self.show_portainer_key;
                Task::none()
            }
            Message::SaveConnection => {
                self.conn = Connection {
                    base_url: self.edit_url.trim().to_string(),
                    api_key: self.edit_key.trim().to_string(),
                    accept_invalid_certs: self.edit_insecure,
                };
                self.portainer = PortainerConnection {
                    base_url: self.edit_portainer_url.trim().to_string(),
                    api_key: self.edit_portainer_key.trim().to_string(),
                    accept_invalid_certs: self.edit_insecure,
                };
                if let Some(cfg) = &self.config {
                    let results = [
                        cfg.set(SERVER_URL_KEY, self.conn.base_url.clone()),
                        cfg.set(API_KEY_KEY, self.conn.api_key.clone()),
                        cfg.set(ACCEPT_INVALID_CERTS_KEY, self.conn.accept_invalid_certs),
                        cfg.set(PORTAINER_URL_KEY, self.portainer.base_url.clone()),
                        cfg.set(PORTAINER_API_KEY_KEY, self.portainer.api_key.clone()),
                    ];
                    for r in results {
                        if let Err(e) = r {
                            tracing::warn!("could not persist connection setting: {e}");
                        }
                    }
                }
                // Try the new connections right away; the summary line and any
                // error captions on the main view show the outcome.
                self.report = AppsReport::default();
                self.containers = ContainerReport::default();
                self.ever_succeeded = false;
                self.silent_retries = 0;
                self.silent_container_retries = 0;
                self.check_error = None;
                self.show_settings = false;
                Task::batch([self.run_check(false), self.run_container_check()])
            }
            Message::CheckRelease => {
                if matches!(self.release, ReleaseStatus::Checking) || self.self_updating {
                    return Task::none();
                }
                self.release = ReleaseStatus::Checking;
                Self::check_release()
            }
            Message::ReleaseChecked(Ok(tag)) => {
                if updater::is_newer(&tag, updater::CURRENT_VERSION) {
                    self.release = ReleaseStatus::Available(tag.clone());
                    // Auto-install the new version if the user opted in.
                    if self.auto_update && !self.self_updating {
                        self.self_updating = true;
                        return Self::do_self_update(tag);
                    }
                } else {
                    self.release = ReleaseStatus::UpToDate;
                }
                Task::none()
            }
            Message::ReleaseChecked(Err(e)) => {
                self.release = ReleaseStatus::Error(e);
                Task::none()
            }
            Message::SetAutoUpdate(on) => {
                self.auto_update = on;
                if let Some(cfg) = &self.config
                    && let Err(e) = cfg.set(AUTO_UPDATE_KEY, on)
                {
                    tracing::warn!("could not persist auto-update setting: {e}");
                }
                // If switching on while an update is already pending, apply it now.
                if on
                    && !self.self_updating
                    && let ReleaseStatus::Available(tag) = &self.release
                {
                    let tag = tag.clone();
                    self.self_updating = true;
                    return Self::do_self_update(tag);
                }
                Task::none()
            }
            Message::SelfUpdate(tag) => {
                if self.self_updating {
                    return Task::none();
                }
                self.self_updating = true;
                Self::do_self_update(tag)
            }
            Message::SelfUpdated(Ok(exe)) => {
                // The binary has been replaced — now run the new version.
                //
                // Under cosmic-panel, exec-ing ourselves can't re-attach to
                // the panel slot: the panel hands each applet a private
                // Wayland session when *it* spawns them, and that session
                // died with the old process image — the exec'd binary falls
                // back to the regular compositor socket and opens as a
                // floating window while the panel keeps a dead slot. Exit
                // instead: cosmic-panel respawns exited applets, and the
                // respawn runs the replaced binary properly in the panel.
                if std::env::var_os("COSMIC_PANEL_NAME").is_some() {
                    tracing::info!(
                        "self-update installed; exiting so the panel respawns the new version"
                    );
                    // Non-zero on purpose: cosmic-panel respawns applets that
                    // die abnormally but treats a clean exit 0 as an
                    // intentional quit and leaves the slot dead.
                    std::process::exit(1);
                }
                // Standalone (e.g. launched from a terminal): exec in place.
                // This only returns if the exec itself fails.
                let err = updater::relaunch(&exe);
                tracing::error!("relaunch after self-update failed: {err}");
                self.self_updating = false;
                self.release =
                    ReleaseStatus::Error(format!("Updated, but relaunch failed: {err}"));
                Task::none()
            }
            Message::SelfUpdated(Err(e)) => {
                self.self_updating = false;
                self.release = ReleaseStatus::Error(e);
                Task::none()
            }
            Message::Token(u) => {
                match u {
                    TokenUpdate::Init(tx) => self.token_tx = Some(tx),
                    TokenUpdate::Finished => self.token_tx = None,
                    TokenUpdate::ActivationToken { token, exec, .. } => {
                        let mut cmd = std::process::Command::new("sh");
                        cmd.arg("-c").arg(&exec);
                        if let Some(token) = token {
                            cmd.env("XDG_ACTIVATION_TOKEN", &token);
                            cmd.env("DESKTOP_STARTUP_ID", &token);
                        }
                        tokio::spawn(cosmic::process::spawn(cmd));
                    }
                }
                Task::none()
            }
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let horizontal = matches!(
            self.core.applet.anchor,
            PanelAnchor::Top | PanelAnchor::Bottom
        );

        let total = self.total_updates();
        let suggested = self.core.applet.suggested_size(true);
        let icon = self.status_icon(suggested.0);

        let content: Element<'_, Message> = if total > 0 {
            let count = self.core.applet.text(total.to_string());
            if horizontal {
                row![icon, count]
                    .spacing(2)
                    .align_y(Alignment::Center)
                    .into()
            } else {
                column![icon, count]
                    .spacing(2)
                    .align_x(Alignment::Center)
                    .into()
            }
        } else {
            icon.into()
        };

        // Match stock applets: give the button a fixed cross-axis size and
        // centre the content, so the hover highlight covers the full panel
        // height (rather than just the icon). Use the regular — not the larger
        // "shrinkable" — padding on the long axis to keep the sides compact.
        let (_pad_shrinkable, pad_regular) = self.core.applet.suggested_padding(true);
        let button = if horizontal {
            button::custom(container(content).center_y(Length::Fill))
                .height(Length::Fixed((suggested.1 + 2 * pad_regular) as f32))
                .padding([0, pad_regular])
        } else {
            button::custom(container(content).center_x(Length::Fill))
                .width(Length::Fixed((suggested.0 + 2 * pad_regular) as f32))
                .padding([pad_regular, 0])
        }
        .on_press_down(Message::TogglePopup)
        .class(cosmic::theme::Button::AppletIcon);

        autosize::autosize(button, AUTOSIZE_MAIN_ID.clone()).into()
    }

    fn view_window(&self, _id: window::Id) -> Element<'_, Message> {
        let Spacing {
            space_xxs,
            space_s,
            space_m,
            ..
        } = theme::active().cosmic().spacing;

        if self.show_settings {
            return self.settings_view();
        }

        let total = self.total_updates();

        // Header / summary line.
        let summary = if !self.conn.is_configured() {
            text::body("Not connected to a TrueNAS server")
        } else if !self.ever_succeeded && self.check_error.is_none() {
            // First contact after launch hasn't landed yet (quiet retries
            // may be running while the network comes up) — stay neutral.
            text::body("Connecting to TrueNAS…")
        } else if self.checking || self.checking_containers {
            text::body("Checking for updates…")
        } else if total == 0 {
            text::body(match (self.report.total_apps, self.containers.total_containers) {
                (0, 0) => "No apps found".to_string(),
                (apps, 0) => format!("All {apps} apps are up to date"),
                (apps, containers) => {
                    format!("{apps} apps & {containers} containers up to date")
                }
            })
        } else {
            text::body(format!(
                "{total} update{} available",
                if total == 1 { "" } else { "s" }
            ))
        };

        let header = padded_control(
            row![
                self.status_icon(28),
                column![text::title4("TrueNAS Apps Watcher"), summary]
                    .spacing(2)
                    .width(Length::Fill),
                button::icon(icon::from_name("emblem-system-symbolic").symbolic(true))
                    .on_press(Message::ToggleSettings),
            ]
            .spacing(space_s)
            .align_y(Alignment::Center),
        );

        let mut content = column![header].spacing(space_xxs);

        if !self.conn.is_configured() {
            content = content.push(
                padded_control(text::caption(
                    "Open Settings (⚙) and enter your TrueNAS address and API key.",
                ))
                .padding([space_xxs, space_m]),
            );
            return self
                .core
                .applet
                .popup_container(
                    container(content.spacing(space_xxs).padding([space_s, 0]))
                        .width(Length::Fixed(360.0)),
                )
                .into();
        }

        // Check button. A manual check also asks TrueNAS to re-sync its app
        // catalog and re-checks unmanaged containers against their registries,
        // so brand-new releases show up immediately.
        let busy_checking = self.checking || self.checking_containers;
        let check_label = if busy_checking {
            "Checking…"
        } else {
            "Check for updates"
        };
        let check_button = button::standard(check_label)
            .leading_icon(icon::from_name("view-refresh-symbolic").symbolic(true))
            .on_press_maybe((!busy_checking && !self.installing).then_some(Message::CheckAll))
            .width(Length::Fill);
        content = content.push(padded_control(check_button));

        // Errors, if any.
        let mut error_lines: Vec<String> = self.report.errors.clone();
        error_lines.extend(self.containers.errors.iter().cloned());
        if let Some(e) = &self.check_error {
            error_lines.push(e.clone());
        }
        if let Some(e) = &self.install_error {
            error_lines.push(e.clone());
        }
        for err in &error_lines {
            content = content.push(
                padded_control(
                    text::caption(err.clone()).class(cosmic::theme::Text::Color(
                        theme::active().cosmic().destructive_color().into(),
                    )),
                )
                .padding([space_xxs, space_m]),
            );
        }

        // Update sections.
        let mut sections = column![].spacing(space_s);
        let mut any = false;
        if let Some(s) = self.section("App updates", &self.report.upgrades) {
            sections = sections.push(s);
            any = true;
        }
        if let Some(s) = self.section("Image updates", &self.report.images) {
            sections = sections.push(s);
            any = true;
        }
        if let Some(s) = self.section("Containers (Portainer)", &self.containers.updates) {
            sections = sections.push(s);
            any = true;
        }

        if any {
            content = content.push(padded_control(divider::horizontal::default()));
            content = content.push(
                container(scrollable(padded_control(sections)).height(Length::Shrink))
                    .max_height(320.0),
            );
            content = content.push(padded_control(divider::horizontal::default()));

            if self.installing {
                // Show progress in place of the action buttons while updating.
                let bar: Element<'_, Message> = match self.install_progress {
                    Some(p) => cosmic::widget::progress_bar::determinate_linear(p)
                        .width(Length::Fill)
                        .into(),
                    None => cosmic::widget::progress_bar::indeterminate_linear()
                        .width(Length::Fill)
                        .into(),
                };
                let label = match self.install_progress {
                    Some(p) => format!("Updating apps… {:.0}%", p * 100.0),
                    None => "Updating apps…".to_string(),
                };
                content = content.push(padded_control(
                    column![text::body(label), bar].spacing(space_xxs),
                ));
            } else {
                // Primary action: apply everything on the server.
                let install_button = button::suggested(format!(
                    "Apply {total} update{}",
                    if total == 1 { "" } else { "s" }
                ))
                .leading_icon(
                    icon::from_name("system-software-install-symbolic").symbolic(true),
                )
                .on_press(Message::Install)
                .width(Length::Fill);
                content = content.push(padded_control(install_button));
            }
        }

        // Secondary: review in the TrueNAS web UI.
        content = content.push(
            menu_button(row![
                icon::from_name("web-browser-symbolic").symbolic(true).size(16),
                text::body("Open apps in TrueNAS"),
            ]
            .spacing(space_s)
            .align_y(Alignment::Center))
            .on_press(Message::OpenWebUi),
        );

        if let Some(checked) = &self.last_checked {
            content = content.push(
                padded_control(text::caption(format!("Last checked at {checked}")))
                    .padding([space_xxs, space_m]),
            );
        }

        self.core
            .applet
            .popup_container(
                container(content.spacing(space_xxs).padding([space_s, 0]))
                    .width(Length::Fixed(360.0)),
            )
            .into()
    }

    fn on_close_requested(&self, id: window::Id) -> Option<Message> {
        Some(Message::CloseRequested(id))
    }
}

impl Window {
    /// The settings panel: TrueNAS connection details, plus the applet's own
    /// version / self-update controls.
    fn settings_view(&self) -> Element<'_, Message> {
        let Spacing {
            space_xxs,
            space_s,
            space_m,
            ..
        } = theme::active().cosmic().spacing;

        // Header with a back button.
        let header = padded_control(
            row![
                button::icon(icon::from_name("go-previous-symbolic").symbolic(true))
                    .on_press(Message::ToggleSettings),
                text::title4("Settings"),
            ]
            .spacing(space_s)
            .align_y(Alignment::Center),
        );

        // --- TrueNAS connection ---
        let url_input = text_input("truenas.local or 192.168.1.100", &self.edit_url)
            .on_input(Message::EditUrl)
            .width(Length::Fill);
        let key_input = secure_input(
            "API key (Settings → API Keys in TrueNAS)",
            &self.edit_key,
            Some(Message::ToggleShowTruenasKey),
            !self.show_truenas_key,
        )
        .on_input(Message::EditApiKey)
        .width(Length::Fill);
        let insecure_row = settings::item(
            "Accept self-signed certificate",
            toggler(self.edit_insecure).on_toggle(Message::EditInsecure),
        );
        // --- Portainer (optional, for containers outside TrueNAS apps) ---
        let portainer_url_input =
            text_input("https://truenas.local:31015 (optional)", &self.edit_portainer_url)
                .on_input(Message::EditPortainerUrl)
                .width(Length::Fill);
        let portainer_key_input = secure_input(
            "Access token (user menu → Access tokens)",
            &self.edit_portainer_key,
            Some(Message::ToggleShowPortainerKey),
            !self.show_portainer_key,
        )
        .on_input(Message::EditPortainerKey)
        .width(Length::Fill);

        let dirty = self.edit_url.trim() != self.conn.base_url
            || self.edit_key.trim() != self.conn.api_key
            || self.edit_insecure != self.conn.accept_invalid_certs
            || self.edit_portainer_url.trim() != self.portainer.base_url
            || self.edit_portainer_key.trim() != self.portainer.api_key;
        let can_save = !self.edit_url.trim().is_empty() && !self.edit_key.trim().is_empty();
        let save_button = button::suggested(if dirty { "Save & connect" } else { "Saved" })
            .on_press_maybe((dirty && can_save).then_some(Message::SaveConnection))
            .width(Length::Fill);

        // --- Applet self-update ---
        let version_row = settings::item("Version", text::body(updater::CURRENT_VERSION));

        // Manual "check GitHub" button (disabled mid-check / mid-update).
        // Labelled distinctly from the main popup's app-update check so the
        // two aren't mistaken for each other.
        let busy = matches!(self.release, ReleaseStatus::Checking) || self.self_updating;
        let check_button = button::standard("Check for new version")
            .leading_icon(icon::from_name("software-update-available-symbolic").symbolic(true))
            .on_press_maybe((!busy).then_some(Message::CheckRelease))
            .width(Length::Fill);

        // Status line + (when an update is available) an "Update now" action.
        let (status, update_now): (String, Option<Element<'_, Message>>) = match &self.release {
            ReleaseStatus::Unknown => ("Not checked yet".to_string(), None),
            ReleaseStatus::Checking => ("Checking GitHub…".to_string(), None),
            ReleaseStatus::UpToDate => {
                (format!("Up to date (v{})", updater::CURRENT_VERSION), None)
            }
            ReleaseStatus::Available(tag) => (
                format!("{tag} is available"),
                (!self.self_updating).then(|| {
                    button::suggested("Update now")
                        .on_press(Message::SelfUpdate(tag.clone()))
                        .into()
                }),
            ),
            ReleaseStatus::Error(e) => (format!("Check failed: {e}"), None),
        };

        let status_class = if matches!(self.release, ReleaseStatus::Error(_)) {
            cosmic::theme::Text::Color(theme::active().cosmic().destructive_color().into())
        } else {
            cosmic::theme::Text::Default
        };
        let mut status_col = column![text::caption(status).class(status_class)].spacing(space_xxs);
        if self.self_updating {
            status_col = status_col.push(text::caption("Downloading and installing…"));
        }
        if let Some(action) = update_now {
            status_col = status_col.push(action);
        }

        // Auto-update toggle.
        let auto_row = settings::item(
            "Automatically update the applet",
            toggler(self.auto_update).on_toggle(Message::SetAutoUpdate),
        );

        let content = column![
            header,
            padded_control(text::heading("TrueNAS server")),
            padded_control(text::caption(
                "The API key is stored in your cosmic-config directory. Use a \
                 read-limited key if you only want to watch for updates.",
            ))
            .padding([0, space_m]),
            padded_control(url_input),
            padded_control(key_input),
            padded_control(insecure_row),
            padded_control(divider::horizontal::default()),
            padded_control(text::heading("Portainer (optional)")),
            padded_control(text::caption(
                "Watches running containers that TrueNAS doesn't manage \
                 (compose stacks, Dockge, …) for newer images.",
            ))
            .padding([0, space_m]),
            padded_control(portainer_url_input),
            padded_control(portainer_key_input),
            padded_control(save_button),
            padded_control(divider::horizontal::default()),
            padded_control(text::heading("Applet")),
            padded_control(text::caption(
                "Updates for the applet itself — separate from the TrueNAS app \
                 updates it watches.",
            ))
            .padding([0, space_m]),
            padded_control(version_row),
            padded_control(check_button),
            padded_control(status_col).padding([space_xxs, space_m]),
            padded_control(auto_row),
        ]
        .spacing(space_xxs);

        self.core
            .applet
            .popup_container(
                container(content.spacing(space_xxs).padding([space_s, 0]))
                    .width(Length::Fixed(360.0)),
            )
            .into()
    }
}
