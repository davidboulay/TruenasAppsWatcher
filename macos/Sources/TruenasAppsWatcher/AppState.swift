// SPDX-License-Identifier: GPL-3.0-only
//
// Central state: connections, check results, timers, and the update
// orchestration. Checks run on the same cadence as the Linux applet:
// TrueNAS apps every 30 minutes, unmanaged containers every 6 hours
// (registry lookups are rate-limited), the app's own release every 6 hours.

import AppKit
import SwiftUI

@MainActor
final class AppState: ObservableObject {
    // MARK: Check results

    @Published var report = AppsReport()
    @Published var containers = ContainerReport()
    @Published var checking = false
    @Published var checkingContainers = false
    @Published var lastChecked: String?
    /// A persistent connectivity error worth showing (manual check failed,
    /// or the quiet retries ran out).
    @Published var checkError: String?
    /// Whether any apps check has succeeded since launch.
    @Published var everSucceeded = false

    /// Consecutive quiet retries after transient "server unreachable"
    /// failures of automatic checks (e.g. Wi-Fi not up yet after login).
    private var silentRetries = 0
    private var silentContainerRetries = 0
    /// Quiet 20-second retries before an unreachable server is reported —
    /// roughly five minutes of grace after login.
    private let maxSilentRetries = 15

    // MARK: Applying updates

    @Published var installing = false
    @Published var installProgress: Double?
    @Published var installError: String?

    // MARK: Self-update

    @Published var releaseStatus: ReleaseStatus = .unknown

    // MARK: Settings (persisted in UserDefaults)

    @Published var trueNAS = TrueNASConnection()
    @Published var portainer = PortainerConnection()

    private var timers: [Timer] = []

    var totalUpdates: Int { report.total + containers.updates.count }

    var menuBarImage: NSImage {
        StatusIcon.image(count: totalUpdates, configured: trueNAS.isConfigured)
    }

    var summary: String {
        if !trueNAS.isConfigured {
            return "Not connected to a TrueNAS server"
        }
        if !everSucceeded && checkError == nil {
            // First contact after launch hasn't landed yet (quiet retries
            // may be running while the network comes up) — stay neutral.
            return "Connecting to TrueNAS…"
        }
        if checking || checkingContainers {
            return "Checking for updates…"
        }
        if totalUpdates == 0 {
            switch (report.totalApps, containers.totalContainers) {
            case (0, 0): return "No apps found"
            case (let apps, 0): return "All \(apps) apps are up to date"
            case (let apps, let n): return "\(apps) apps & \(n) containers up to date"
            }
        }
        return "\(totalUpdates) update\(totalUpdates == 1 ? "" : "s") available"
    }

    init() {
        let defaults = UserDefaults.standard
        trueNAS = TrueNASConnection(
            baseURL: defaults.string(forKey: "server-url") ?? "",
            apiKey: defaults.string(forKey: "api-key") ?? "",
            acceptInvalidCerts: defaults.object(forKey: "accept-invalid-certs") as? Bool ?? true)
        portainer = PortainerConnection(
            baseURL: defaults.string(forKey: "portainer-url") ?? "",
            apiKey: defaults.string(forKey: "portainer-api-key") ?? "",
            acceptInvalidCerts: trueNAS.acceptInvalidCerts)

        checkApps()
        checkContainers()
        checkRelease()

        // Timer callbacks arrive on the main run loop; hop into the actor.
        // (`guard let` rebinds self immutably so the @Sendable Task can capture it.)
        timers.append(Timer.scheduledTimer(withTimeInterval: 30 * 60, repeats: true) { [weak self] _ in
            guard let self else { return }
            Task { @MainActor in self.checkApps() }
        })
        timers.append(Timer.scheduledTimer(withTimeInterval: 6 * 60 * 60, repeats: true) { [weak self] _ in
            guard let self else { return }
            Task { @MainActor in
                self.checkContainers()
                self.checkRelease()
            }
        })
    }

    // MARK: Actions

    func checkApps(refreshCatalog: Bool = false, manual: Bool = false) {
        guard !checking, trueNAS.isConfigured else { return }
        checking = true
        let client = TrueNASClient(trueNAS)
        Task {
            let result = await client.checkApps(refreshCatalog: refreshCatalog)
            self.checking = false
            if result.unreachable {
                // Keep whatever we last knew instead of clobbering it.
                // Automatic checks retry quietly first — right after login
                // the network is often not up yet, and a red "no internet"
                // flash helps nobody.
                if !manual, self.silentRetries < self.maxSilentRetries {
                    self.silentRetries += 1
                    try? await Task.sleep(nanoseconds: 20_000_000_000)
                    self.checkApps()
                } else {
                    self.checkError = result.errors.first
                }
                return
            }
            self.everSucceeded = true
            self.silentRetries = 0
            self.checkError = nil
            self.report = result
            let fmt = DateFormatter()
            fmt.dateFormat = "HH:mm"
            self.lastChecked = fmt.string(from: Date())
        }
    }

    func checkContainers(manual: Bool = false) {
        guard !checkingContainers, portainer.isConfigured else { return }
        checkingContainers = true
        let client = PortainerClient(portainer)
        Task {
            let result = await client.checkContainers()
            self.checkingContainers = false
            if result.unreachable {
                if !manual, self.silentContainerRetries < self.maxSilentRetries {
                    self.silentContainerRetries += 1
                    try? await Task.sleep(nanoseconds: 20_000_000_000)
                    self.checkContainers()
                } else {
                    // Persistent or manual: show it with the report errors.
                    self.containers.errors = result.errors
                }
                return
            }
            self.silentContainerRetries = 0
            self.containers = result
        }
    }

    func checkAll() {
        checkApps(refreshCatalog: true, manual: true)
        checkContainers(manual: true)
    }

    /// Apply every pending update, one at a time, streaming overall progress.
    func applyAll() {
        guard !installing, !checking, totalUpdates > 0 else { return }
        installing = true
        installProgress = nil
        installError = nil
        let items = report.upgrades + report.images + containers.updates
        let tnClient = TrueNASClient(trueNAS)
        let ptClient = PortainerClient(portainer)

        Task {
            let n = Double(max(items.count, 1))
            var errors: [String] = []

            for (i, item) in items.enumerated() {
                let base = Double(i) / n
                self.installProgress = base
                do {
                    switch item.kind {
                    case .container(let endpointId, let containerId, let image):
                        // Pull with streamed layer progress (nearly all the
                        // wall time), then recreate without re-pulling. Fall
                        // back to pull-inside-recreate if streaming fails.
                        do {
                            try await ptClient.pullImage(
                                endpointId: endpointId, image: image
                            ) { f in
                                Task { @MainActor in
                                    self.installProgress = base + f * 0.9 / n
                                }
                            }
                            self.installProgress = base + 0.9 / n
                            try await ptClient.recreateContainer(
                                endpointId: endpointId, containerId: containerId, pull: false)
                        } catch {
                            try await ptClient.recreateContainer(
                                endpointId: endpointId, containerId: containerId, pull: true)
                        }
                    case .app, .image:
                        let jobId = try await tnClient.startUpdateJob(item)
                        try await tnClient.waitJob(jobId) { f in
                            Task { @MainActor in
                                self.installProgress = base + f / n
                            }
                        }
                    }
                } catch {
                    errors.append("\(item.title): \(error.localizedDescription)")
                }
            }

            self.installing = false
            self.installProgress = nil
            self.installError = errors.isEmpty ? nil : errors.joined(separator: "\n")
            self.checkApps()
            self.checkContainers()
        }
    }

    func openTrueNAS() {
        guard trueNAS.isConfigured,
              let url = URL(string: trueNAS.webUIBase + "/ui/apps/installed")
        else { return }
        NSWorkspace.shared.open(url)
    }

    /// Persist edited connection settings and re-check against them.
    func saveSettings(
        serverURL: String, apiKey: String, acceptInvalidCerts: Bool,
        portainerURL: String, portainerKey: String
    ) {
        trueNAS = TrueNASConnection(
            baseURL: serverURL.trimmingCharacters(in: .whitespaces),
            apiKey: apiKey.trimmingCharacters(in: .whitespaces),
            acceptInvalidCerts: acceptInvalidCerts)
        portainer = PortainerConnection(
            baseURL: portainerURL.trimmingCharacters(in: .whitespaces),
            apiKey: portainerKey.trimmingCharacters(in: .whitespaces),
            acceptInvalidCerts: acceptInvalidCerts)

        let defaults = UserDefaults.standard
        defaults.set(trueNAS.baseURL, forKey: "server-url")
        defaults.set(trueNAS.apiKey, forKey: "api-key")
        defaults.set(trueNAS.acceptInvalidCerts, forKey: "accept-invalid-certs")
        defaults.set(portainer.baseURL, forKey: "portainer-url")
        defaults.set(portainer.apiKey, forKey: "portainer-api-key")

        report = AppsReport()
        containers = ContainerReport()
        everSucceeded = false
        silentRetries = 0
        silentContainerRetries = 0
        checkError = nil
        checkApps()
        checkContainers()
    }

    func checkRelease() {
        if case .checking = releaseStatus { return }
        releaseStatus = .checking
        Task {
            self.releaseStatus = await ReleaseCheck.latest()
        }
    }
}
