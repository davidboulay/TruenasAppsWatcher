// SPDX-License-Identifier: GPL-3.0-only
//
// The menu bar popover: summary, pending-update sections, apply-all with
// progress, and a link to the TrueNAS web UI. Settings opens in a proper
// macOS Settings window (the gear / ⌘,).
//
// MenuBarExtra's .window style doesn't track content-height changes (it
// keeps the tallest size it has shown), so this view measures itself and
// resizes the hosting panel explicitly, anchored to its top edge.

import SwiftUI

struct PopoverView: View {
    @EnvironmentObject var state: AppState
    @State private var panel: NSWindow?

    var body: some View {
        mainView
            .padding(14)
            .frame(width: 340)
            .fixedSize(horizontal: false, vertical: true)
            .background(WindowAccessor(window: $panel))
            .background(
                GeometryReader { geo in
                    Color.clear.preference(key: ContentSizeKey.self, value: geo.size)
                }
            )
            .onPreferenceChange(ContentSizeKey.self) { size in
                resizePanel(to: size)
            }
    }

    /// Keep the borderless MenuBarExtra panel exactly content-sized, keeping
    /// its top edge (under the status item) in place.
    private func resizePanel(to size: CGSize) {
        guard let panel, size.height > 10 else { return }
        let frame = panel.frame
        guard abs(frame.height - size.height) > 1 else { return }
        panel.setFrame(
            NSRect(x: frame.origin.x, y: frame.maxY - size.height,
                   width: frame.width, height: size.height),
            display: true)
    }

    private var mainView: some View {
        VStack(alignment: .leading, spacing: 10) {
            header

            if !state.trueNAS.isConfigured {
                Text("Open Settings (⚙) and enter your TrueNAS address and API key.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Button {
                state.checkAll()
            } label: {
                Label(
                    state.checking || state.checkingContainers
                        ? "Checking…" : "Check for updates",
                    systemImage: "arrow.clockwise")
                .frame(maxWidth: .infinity)
            }
            .disabled(state.checking || state.checkingContainers || state.installing
                      || !state.trueNAS.isConfigured)

            if state.offline {
                // Off the home network (or the NAS is down) — a normal
                // state, not an error. Background retries keep running.
                Label(
                    "TrueNAS not reachable — retrying automatically",
                    systemImage: "wifi.slash")
                .font(.caption)
                .foregroundStyle(.secondary)
            }

            errorLines

            if state.totalUpdates > 0 {
                Divider()
                ScrollView {
                    VStack(alignment: .leading, spacing: 10) {
                        section("App updates", items: state.report.upgrades)
                        section("Image updates", items: state.report.images)
                        section("Containers (Portainer)", items: state.containers.updates)
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
                .frame(maxHeight: 280)
                Divider()

                if state.installing {
                    VStack(alignment: .leading, spacing: 4) {
                        if let p = state.installProgress {
                            Text("Updating… \(Int(p * 100))%")
                            ProgressView(value: p)
                        } else {
                            Text("Updating…")
                            ProgressView()
                                .progressViewStyle(.linear)
                        }
                    }
                } else {
                    Button {
                        state.applyAll()
                    } label: {
                        Label(
                            "Apply \(state.totalUpdates) update\(state.totalUpdates == 1 ? "" : "s")",
                            systemImage: "square.and.arrow.down")
                        .frame(maxWidth: .infinity)
                    }
                    .keyboardShortcut(.defaultAction)
                    // Stale list while unreachable: applying would just
                    // time out item by item.
                    .disabled(state.offline)
                }
            }

            Button {
                state.openTrueNAS()
            } label: {
                Label("Open apps in TrueNAS", systemImage: "safari")
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .buttonStyle(.plain)
            .disabled(!state.trueNAS.isConfigured)

            HStack {
                if let checked = state.lastChecked {
                    Text("Last checked at \(checked)")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                Button("Quit") {
                    NSApplication.shared.terminate(nil)
                }
                .buttonStyle(.plain)
                .font(.caption)
                .foregroundStyle(.secondary)
            }
        }
    }

    private var header: some View {
        HStack(spacing: 10) {
            Image(nsImage: StatusIcon.image(
                count: state.totalUpdates, configured: state.trueNAS.isConfigured))
                .resizable()
                .aspectRatio(contentMode: .fit)
                .frame(height: 26)
            VStack(alignment: .leading, spacing: 2) {
                Text("TrueNAS Apps Watcher").font(.headline)
                Text(state.summary).font(.subheadline).foregroundStyle(.secondary)
            }
            Spacer()
            gearButton
        }
    }

    @ViewBuilder
    private var gearButton: some View {
        if #available(macOS 14.0, *) {
            SettingsLink {
                Image(systemName: "gearshape")
            }
            .buttonStyle(.plain)
            // The Settings window otherwise opens behind other apps, since a
            // menu bar app is never the active application.
            .simultaneousGesture(TapGesture().onEnded {
                NSApp.activate(ignoringOtherApps: true)
            })
        } else {
            Button {
                NSApp.sendAction(Selector(("showSettingsWindow:")), to: nil, from: nil)
                NSApp.activate(ignoringOtherApps: true)
            } label: {
                Image(systemName: "gearshape")
            }
            .buttonStyle(.plain)
        }
    }

    @ViewBuilder
    private var errorLines: some View {
        let lines = state.report.errors + state.containers.errors
            + (state.installError.map { [$0] } ?? [])
        ForEach(lines, id: \.self) { line in
            Text(line)
                .font(.caption)
                .foregroundStyle(.red)
        }
    }

    @ViewBuilder
    private func section(_ title: String, items: [UpdateItem]) -> some View {
        if !items.isEmpty {
            VStack(alignment: .leading, spacing: 6) {
                Text("\(title) (\(items.count))").font(.subheadline).bold()
                ForEach(items) { item in
                    VStack(alignment: .leading, spacing: 1) {
                        Text(item.title)
                        Text(item.subtitle)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
            }
        }
    }
}

private struct ContentSizeKey: PreferenceKey {
    static var defaultValue: CGSize = .zero
    static func reduce(value: inout CGSize, nextValue: () -> CGSize) {
        value = nextValue()
    }
}

/// Surfaces the NSWindow hosting a SwiftUI hierarchy.
private struct WindowAccessor: NSViewRepresentable {
    @Binding var window: NSWindow?

    func makeNSView(context: Context) -> NSView {
        let view = NSView()
        DispatchQueue.main.async { window = view.window }
        return view
    }

    func updateNSView(_ view: NSView, context: Context) {
        DispatchQueue.main.async { window = view.window }
    }
}
