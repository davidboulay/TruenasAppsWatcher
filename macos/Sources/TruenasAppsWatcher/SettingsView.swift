// SPDX-License-Identifier: GPL-3.0-only
//
// The macOS Settings window (⌘, / the popover's gear): TrueNAS + optional
// Portainer connections (keys masked once saved, with an eye toggle),
// launch-at-login, and the app's own version / release check.

import ServiceManagement
import SwiftUI

struct SettingsView: View {
    @EnvironmentObject var state: AppState

    @State private var serverURL = ""
    @State private var apiKey = ""
    @State private var acceptInvalidCerts = true
    @State private var portainerURL = ""
    @State private var portainerKey = ""
    @State private var showTrueNASKey = false
    @State private var showPortainerKey = false
    @State private var launchAtLogin = SMAppService.mainApp.status == .enabled

    private var dirty: Bool {
        serverURL.trimmingCharacters(in: .whitespaces) != state.trueNAS.baseURL
            || apiKey.trimmingCharacters(in: .whitespaces) != state.trueNAS.apiKey
            || acceptInvalidCerts != state.trueNAS.acceptInvalidCerts
            || portainerURL.trimmingCharacters(in: .whitespaces) != state.portainer.baseURL
            || portainerKey.trimmingCharacters(in: .whitespaces) != state.portainer.apiKey
    }

    private var canSave: Bool {
        !serverURL.trimmingCharacters(in: .whitespaces).isEmpty
            && !apiKey.trimmingCharacters(in: .whitespaces).isEmpty
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("TrueNAS server").font(.subheadline).bold()
            Text("The API key is stored in this app's preferences. Use a read-limited key if you only want to watch for updates.")
                .font(.caption)
                .foregroundStyle(.secondary)
            TextField("truenas.local or 192.168.1.100", text: $serverURL)
                .textFieldStyle(.roundedBorder)
            keyField(
                "API key (Settings → API Keys in TrueNAS)",
                text: $apiKey, visible: $showTrueNASKey)
            Toggle("Accept self-signed certificate", isOn: $acceptInvalidCerts)

            Divider()

            Text("Portainer (optional)").font(.subheadline).bold()
            Text("Watches running containers that TrueNAS doesn't manage (compose stacks, Dockge, …) for newer images.")
                .font(.caption)
                .foregroundStyle(.secondary)
            TextField("https://truenas.local:31015 (optional)", text: $portainerURL)
                .textFieldStyle(.roundedBorder)
            keyField(
                "Access token (user menu → Access tokens)",
                text: $portainerKey, visible: $showPortainerKey)

            Button {
                state.saveSettings(
                    serverURL: serverURL, apiKey: apiKey,
                    acceptInvalidCerts: acceptInvalidCerts,
                    portainerURL: portainerURL, portainerKey: portainerKey)
            } label: {
                Text(dirty ? "Save & connect" : "Saved")
                    .frame(maxWidth: .infinity)
            }
            .disabled(!dirty || !canSave)

            Divider()

            Text("App").font(.subheadline).bold()
            Toggle("Start at login", isOn: $launchAtLogin)
                .onChange(of: launchAtLogin) { on in
                    do {
                        if on {
                            try SMAppService.mainApp.register()
                        } else {
                            try SMAppService.mainApp.unregister()
                        }
                    } catch {
                        launchAtLogin = SMAppService.mainApp.status == .enabled
                    }
                }
            HStack {
                Text("Version")
                Spacer()
                Text(ReleaseCheck.currentVersion).foregroundStyle(.secondary)
            }
            releaseRow
        }
        .padding(20)
        .frame(width: 400)
        .fixedSize(horizontal: false, vertical: true)
        .onAppear {
            serverURL = state.trueNAS.baseURL
            apiKey = state.trueNAS.apiKey
            acceptInvalidCerts = state.trueNAS.acceptInvalidCerts
            portainerURL = state.portainer.baseURL
            portainerKey = state.portainer.apiKey
            // Only show key text while none is saved yet (first setup).
            showTrueNASKey = state.trueNAS.apiKey.isEmpty
            showPortainerKey = state.portainer.apiKey.isEmpty
            state.checkRelease()
        }
    }

    /// A masked key field with an eye button to reveal it.
    private func keyField(
        _ placeholder: String, text: Binding<String>, visible: Binding<Bool>
    ) -> some View {
        HStack(spacing: 6) {
            Group {
                if visible.wrappedValue {
                    TextField(placeholder, text: text)
                } else {
                    SecureField(placeholder, text: text)
                }
            }
            .textFieldStyle(.roundedBorder)
            Button {
                visible.wrappedValue.toggle()
            } label: {
                Image(systemName: visible.wrappedValue ? "eye.slash" : "eye")
            }
            .buttonStyle(.plain)
        }
    }

    @ViewBuilder
    private var releaseRow: some View {
        switch state.releaseStatus {
        case .unknown:
            Text("Release check pending").font(.caption).foregroundStyle(.secondary)
        case .checking:
            Text("Checking GitHub…").font(.caption).foregroundStyle(.secondary)
        case .upToDate:
            Text("Up to date (v\(ReleaseCheck.currentVersion))")
                .font(.caption)
                .foregroundStyle(.secondary)
        case .available(let tag):
            HStack {
                Text("\(tag) is available").font(.caption)
                Spacer()
                Button("Get update") {
                    NSWorkspace.shared.open(ReleaseCheck.releasesURL)
                }
            }
        case .error(let message):
            Text("Check failed: \(message)").font(.caption).foregroundStyle(.red)
        }
    }
}
