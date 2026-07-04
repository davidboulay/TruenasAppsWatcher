// SPDX-License-Identifier: GPL-3.0-only
//
// TrueNAS Apps Watcher — a macOS menu bar app that watches a TrueNAS SCALE
// server (and optionally Portainer) for pending Docker app updates and can
// apply them. Lives entirely in the menu bar: the bundle sets LSUIElement so
// there is no Dock icon.

import SwiftUI

@main
struct TruenasAppsWatcherApp: App {
    @StateObject private var state = AppState()

    var body: some Scene {
        MenuBarExtra {
            PopoverView()
                .environmentObject(state)
        } label: {
            // Icon and pending-update count are drawn into one NSImage —
            // menu bar labels handle a single image more predictably than
            // composed views.
            Image(nsImage: state.menuBarImage)
        }
        .menuBarExtraStyle(.window)

        // A real macOS Settings window (⌘,) rather than a panel swapped
        // inside the popover: the popover keeps one compact layout, and the
        // MenuBarExtra window never gets stuck at the settings height.
        Settings {
            SettingsView()
                .environmentObject(state)
        }
    }
}
