// SPDX-License-Identifier: GPL-3.0-only
//
// Shared data model: pending updates, check reports, and connections.

import Foundation

/// How to reach the TrueNAS server.
struct TrueNASConnection: Equatable {
    var baseURL = ""
    var apiKey = ""
    var acceptInvalidCerts = true

    var isConfigured: Bool {
        !baseURL.trimmingCharacters(in: .whitespaces).isEmpty
            && !apiKey.trimmingCharacters(in: .whitespaces).isEmpty
    }

    /// Base with a scheme and no trailing slash; bare hosts default to https.
    var normalizedBase: String {
        var b = baseURL.trimmingCharacters(in: .whitespaces)
        while b.hasSuffix("/") { b.removeLast() }
        if !b.hasPrefix("http://") && !b.hasPrefix("https://") {
            b = "https://" + b
        }
        return b
    }
}

/// How to reach Portainer (optional — enables the unmanaged-container watch).
struct PortainerConnection: Equatable {
    var baseURL = ""
    var apiKey = ""
    var acceptInvalidCerts = true

    var isConfigured: Bool {
        !baseURL.trimmingCharacters(in: .whitespaces).isEmpty
            && !apiKey.trimmingCharacters(in: .whitespaces).isEmpty
    }

    var normalizedBase: String {
        var b = baseURL.trimmingCharacters(in: .whitespaces)
        while b.hasSuffix("/") { b.removeLast() }
        if !b.hasPrefix("http://") && !b.hasPrefix("https://") {
            b = "https://" + b
        }
        return b
    }
}

/// What kind of update an item has pending, and what's needed to apply it.
enum UpdateKind: Equatable {
    /// A newer TrueNAS catalog version — applied with `app.upgrade`.
    case app
    /// Same version, newer Docker image(s) — applied with `app.pull_images`.
    case image
    /// A container outside TrueNAS's apps — pulled + recreated via Portainer.
    case container(endpointId: Int, containerId: String, image: String)
}

/// A single pending update.
struct UpdateItem: Identifiable, Equatable {
    let id = UUID()
    /// The name/id used by the API (app name or container name).
    let name: String
    /// Human-readable title.
    let title: String
    /// Currently installed version or image reference.
    let current: String
    /// Latest available version. Empty for image/container updates.
    let latest: String
    let kind: UpdateKind

    /// The secondary line shown under the title.
    var subtitle: String {
        if latest.isEmpty {
            return current.isEmpty ? "New image available" : "\(current) — new image available"
        }
        return current.isEmpty ? latest : "\(current) → \(latest)"
    }
}

/// Result of a TrueNAS apps check.
struct AppsReport: Equatable {
    var upgrades: [UpdateItem] = []
    var images: [UpdateItem] = []
    var totalApps = 0
    var errors: [String] = []

    var total: Int { upgrades.count + images.count }
}

/// Result of an unmanaged-container check (separate schedule from the apps).
struct ContainerReport: Equatable {
    var updates: [UpdateItem] = []
    var totalContainers = 0
    var errors: [String] = []
}

/// Where the app's own version sits relative to the latest GitHub release.
enum ReleaseStatus: Equatable {
    case unknown
    case checking
    case upToDate
    case available(String)
    case error(String)
}
