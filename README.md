# TrueNAS Apps Watcher

A panel applet for the [**COSMIC**](https://system76.com/cosmic/) desktop that
watches a **TrueNAS SCALE** server for pending app (Docker) updates — and,
optionally, any other containers you run beside them — and lets you apply
everything without opening a browser. It runs on any system with the COSMIC
desktop: Pop!_OS, and the COSMIC spins/packages of Fedora, Arch, NixOS,
openSUSE, and friends. It can be added to the COSMIC panel or dock like any
built-in applet.

<img src="screenshots/popup.png" width="420" alt="Applet popup showing all apps and containers up to date">

## What it does

- **Panel button** shows a NAS-shaped badge in TrueNAS blue with the number
  of pending updates, or a muted ✓ variant when everything is up to date.
- **Popup** (click the button) lists the pending updates, split into:
  - **App updates** — TrueNAS catalog version bumps, shown as
    `current → latest`.
  - **Image updates** — TrueNAS apps whose Docker image has a newer build
    available (typical for custom apps tracking a `latest` tag).
  - **Containers (Portainer)** — optional: running containers *outside*
    TrueNAS's apps (compose stacks, Dockge, hand-run containers…), watched
    through a Portainer instance. The applet lists them via Portainer's
    Docker API proxy (skipping the `ix-*` projects TrueNAS manages) and
    flags any whose image tag has a newer digest at its registry — the same
    check Watchtower does. Works anonymously with Docker Hub, ghcr.io,
    lscr.io, and other standard registries.
- **"Check for updates"** asks TrueNAS to re-sync its app catalog (the same
  thing its daily cron does), re-queries, and re-checks unmanaged containers
  against their registries, so brand-new releases show up immediately.
- **"Apply N updates"** applies everything server-side: catalog upgrades go
  through the `app.upgrade` job, image updates through `app.pull_images`,
  and unmanaged containers are pulled with live per-layer progress and then
  recreated through Portainer (same as its *Recreate* button). Items are
  updated one at a time and a progress bar tracks them; the badge clears
  when they finish. Failures for one item are reported without stopping the
  rest.
- **"Open apps in TrueNAS"** opens the web UI's installed-apps page in your
  browser for anything you'd rather review by hand.
- **Settings** (the ⚙ button in the popup header):
  - **TrueNAS server** — address, API key, and whether to accept the
    self-signed certificate TrueNAS ships with (on by default). Key fields
    are masked once saved, with an eye button to reveal.
  - **Portainer (optional)** — address and access token for the unmanaged-
    container watch.
  - **Applet self-update** — shows the applet's version, checks the latest
    [GitHub release](https://github.com/davidboulay/TruenasAppsWatcher/releases),
    and can keep the applet up to date automatically (checked on startup and
    every few hours).

The applet re-queries the server every 30 minutes, so updates that appear (or
are installed elsewhere, e.g. from the TrueNAS UI) are reflected without any
interaction. On startup it checks immediately. Unmanaged containers are
re-checked every 6 hours instead (plus on any manual check): each image lookup
hits its registry, and Docker Hub rate-limits anonymous requests.

## Requirements

- A desktop running **COSMIC** (Pop!_OS 24.04+, or COSMIC packages on Fedora,
  Arch, NixOS, openSUSE, …).
- **TrueNAS SCALE** with the Docker-based apps system (24.10 "Electric Eel"
  or newer; tested on 25.10). The applet talks to the `/api/v2.0` REST
  endpoints with a Bearer API key.
- A TrueNAS **API key**: TrueNAS web UI → click the ⚙ icon (top right) →
  **API Keys** → **Add**. Checking for updates only reads; applying updates
  calls `app.upgrade`, so use a full-access key (or a key limited to the app
  service) if you want the update button to work.
- *(Optional)* **Portainer** 2.19+ with an **access token** (Portainer →
  click your user name → **Access tokens** → **Add**) to also watch
  containers TrueNAS doesn't manage. Tested with Portainer CE 2.43.

## Install

One command — no checkout required:

```sh
curl -fsSL https://raw.githubusercontent.com/davidboulay/TruenasAppsWatcher/main/install.sh | bash
```

The installer downloads a **prebuilt binary** from the latest
[release](https://github.com/davidboulay/TruenasAppsWatcher/releases)
(x86_64 glibc — Pop!_OS, Fedora, Arch, …; no Rust needed). On other
architectures or distros (e.g. NixOS), or if no release is available, it
automatically **builds from source** instead (requires a Rust toolchain and
the usual COSMIC build dependencies: `libxkbcommon-dev libwayland-dev
pkg-config`).

Then add it to the panel:
**Settings → Desktop → Panel (or Dock) → Add Applet → "TrueNAS Apps Watcher"**,
click the new applet, open its Settings (⚙) and enter the server address and
API key.

<img src="screenshots/settings.png" width="360" alt="Settings panel with TrueNAS and Portainer connections and applet self-update">

## Security notes

- The API key and Portainer token are stored **in plain text** in your
  cosmic-config directory
  (`~/.config/cosmic/com.github.davidboulay.CosmicAppletTruenasApps/`),
  the same way COSMIC stores other applet settings. Don't use this on a
  shared account, and prefer keys scoped to what you need.
- "Accept self-signed certificate" (default on) disables TLS certificate
  verification for the TrueNAS and Portainer connections — fine on a trusted
  LAN, turn it off if your server has a proper certificate.

## Build from a checkout

```sh
cargo build --release
./install.sh          # installs the freshly built binary
```

## License

GPL-3.0-only
