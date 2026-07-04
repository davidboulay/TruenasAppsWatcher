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
        guard let url = URL(string: "https://api.github.com/repos/\(repo)/releases/latest") else {
            return .error("bad URL")
        }
        var req = URLRequest(url: url)
        req.setValue("application/vnd.github+json", forHTTPHeaderField: "Accept")
        do {
            let (data, response) = try await URLSession.shared.data(for: req)
            guard let http = response as? HTTPURLResponse else {
                return .error("not an HTTP response")
            }
            // 404: repo up but nothing published yet — distinct from offline.
            if http.statusCode == 404 {
                return .error("No release published yet")
            }
            // 403/429: GitHub's anonymous API allows 60 requests/hour per IP.
            if http.statusCode == 403 || http.statusCode == 429 {
                return .error("GitHub rate limit reached — try again in an hour")
            }
            guard (200..<300).contains(http.statusCode) else {
                return .error("GitHub returned HTTP \(http.statusCode)")
            }
            guard let obj = try JSONSerialization.jsonObject(with: data) as? [String: Any],
                  let tag = obj["tag_name"] as? String
            else {
                return .error("No release found")
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
