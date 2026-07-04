// SPDX-License-Identifier: GPL-3.0-only
//
// Self-update check: compare this build's version against the latest GitHub
// release. On macOS the app doesn't replace itself — "Update now" opens the
// releases page.

import Foundation

enum ReleaseCheck {
    static let repo = "davidboulay/TruenasAppsWatcher"
    static let currentVersion =
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "0.0.0"

    static var releasesURL: URL {
        URL(string: "https://github.com/\(repo)/releases")!
    }

    static func latest() async -> ReleaseStatus {
        // Uses the *web* endpoint, not api.github.com: releases/latest
        // redirects to …/releases/tag/<tag>, and unlike the API it isn't
        // subject to the 60-requests/hour anonymous quota that the whole
        // LAN's public IP shares. URLSession follows the redirect; the tag
        // is read off the final URL.
        guard let url = URL(string: "https://github.com/\(repo)/releases/latest") else {
            return .error("bad URL")
        }
        var req = URLRequest(url: url)
        req.httpMethod = "HEAD"
        do {
            let (_, response) = try await URLSession.shared.data(for: req)
            guard let http = response as? HTTPURLResponse else {
                return .error("not an HTTP response")
            }
            // 404: repo up but nothing published yet — distinct from offline.
            if http.statusCode == 404 {
                return .error("No release published yet")
            }
            guard (200..<300).contains(http.statusCode) else {
                return .error("GitHub returned HTTP \(http.statusCode)")
            }
            let path = http.url?.path ?? ""
            guard let range = path.range(of: "/tag/") else {
                return .error("No release published yet")
            }
            let tag = String(path[range.upperBound...])
                .trimmingCharacters(in: CharacterSet(charactersIn: "/"))
            guard !tag.isEmpty else {
                return .error("No release published yet")
            }
            return isNewer(tag, than: currentVersion) ? .available(tag) : .upToDate
        } catch {
            return .error("Could not reach GitHub")
        }
    }

    /// `1.2.3` (optional leading `v`, trailing pre-release ignored) → (1, 2, 3).
    static func parseSemver(_ v: String) -> (Int, Int, Int)? {
        var s = v.trimmingCharacters(in: .whitespaces)
        if s.hasPrefix("v") { s.removeFirst() }
        let core = s.split(whereSeparator: { $0 == "-" || $0 == "+" }).first.map(String.init) ?? s
        let parts = core.split(separator: ".").map(String.init)
        guard let major = parts.first.flatMap(Int.init) else { return nil }
        let minor = parts.count > 1 ? Int(parts[1]) ?? 0 : 0
        let patch = parts.count > 2 ? Int(parts[2]) ?? 0 : 0
        return (major, minor, patch)
    }

    static func isNewer(_ latest: String, than current: String) -> Bool {
        guard let l = parseSemver(latest), let c = parseSemver(current) else {
            return latest.trimmingCharacters(in: CharacterSet(charactersIn: "v"))
                != current.trimmingCharacters(in: CharacterSet(charactersIn: "v"))
        }
        return l > c
    }
}
