// SPDX-License-Identifier: GPL-3.0-only
//
// TrueNAS SCALE REST API client (`/api/v2.0`, Bearer API key): queries
// installed apps for pending upgrades and drives the `app.upgrade` /
// `app.pull_images` middleware jobs (started via POST, polled via
// `core.get_jobs`).

import Foundation

struct TrueNASClient {
    let conn: TrueNASConnection
    private let session: URLSession

    init(_ conn: TrueNASConnection) {
        self.conn = conn
        self.session = HTTP.session(insecure: conn.acceptInvalidCerts)
    }

    private func request(_ method: String, _ path: String, body: Data? = nil) -> URLRequest {
        var req = URLRequest(url: URL(string: conn.normalizedBase + path)!)
        req.httpMethod = method
        req.setValue("Bearer \(conn.apiKey.trimmingCharacters(in: .whitespaces))",
                     forHTTPHeaderField: "Authorization")
        if let body {
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
            req.httpBody = body
        }
        return req
    }

    // MARK: Wire types

    private struct RawApp: Decodable {
        let name: String
        let upgrade_available: Bool?
        let image_updates_available: Bool?
        let human_version: String?
        let latest_version: String?
        let metadata: RawMetadata?
    }

    private struct RawMetadata: Decodable {
        let title: String?
    }

    private struct Job: Decodable {
        let state: String
        let progress: JobProgress?
        let error: String?
    }

    private struct JobProgress: Decodable {
        let percent: Double?
    }

    // MARK: Checks

    /// Check for pending app updates. With `refreshCatalog`, first ask TrueNAS
    /// to re-sync its app catalog (what its own daily cron does). Never throws
    /// as a whole — errors are collected in the report.
    func checkApps(refreshCatalog: Bool) async -> AppsReport {
        var report = AppsReport()
        guard conn.isConfigured else {
            report.errors.append("Not configured — set the server address and API key in Settings")
            return report
        }

        if refreshCatalog {
            // `catalog.sync` takes no arguments, which the REST layer maps to
            // GET. A sync failure shouldn't hide what we can still read.
            do {
                let data = try await HTTP.data(
                    session, request("GET", "/api/v2.0/catalog/sync"), label: "catalog sync")
                if let jobId = try? JSONDecoder().decode(Int.self, from: data) {
                    _ = try? await waitJob(jobId) { _ in }
                }
            } catch {
                report.errors.append("Catalog refresh failed: \(error.localizedDescription)")
            }
        }

        do {
            let data = try await HTTP.data(
                session, request("GET", "/api/v2.0/app"), label: "app query")
            let apps = try JSONDecoder().decode([RawApp].self, from: data)
            report.totalApps = apps.count
            for app in apps {
                let title = app.metadata?.title ?? app.name
                let current = app.human_version ?? ""
                if app.upgrade_available == true {
                    report.upgrades.append(UpdateItem(
                        name: app.name, title: title, current: current,
                        latest: app.latest_version ?? "", kind: .app))
                } else if app.image_updates_available == true {
                    report.images.append(UpdateItem(
                        name: app.name, title: title, current: current,
                        latest: "", kind: .image))
                }
            }
            report.upgrades.sort { $0.title.lowercased() < $1.title.lowercased() }
            report.images.sort { $0.title.lowercased() < $1.title.lowercased() }
        } catch {
            report.errors.append(error.localizedDescription)
        }
        return report
    }

    // MARK: Jobs

    /// Start the middleware job that applies one app/image update; returns the job id.
    func startUpdateJob(_ item: UpdateItem) async throws -> Int {
        let path: String
        let body: [String: Any]
        switch item.kind {
        case .app:
            path = "/api/v2.0/app/upgrade"
            body = ["app_name": item.name, "options": ["app_version": "latest"]]
        case .image:
            path = "/api/v2.0/app/pull_images"
            body = ["app_name": item.name]
        case .container:
            throw AppError("container updates go through Portainer")
        }
        let payload = try JSONSerialization.data(withJSONObject: body)
        let data = try await HTTP.data(
            session, request("POST", path, body: payload), label: item.title)
        guard let jobId = try? JSONDecoder().decode(Int.self, from: data) else {
            throw AppError("\(item.title): unexpected job response")
        }
        return jobId
    }

    /// Poll a job until it finishes, reporting its own 0...1 progress.
    func waitJob(_ jobId: Int, onProgress: (Double) -> Void) async throws {
        let deadline = Date().addingTimeInterval(30 * 60)
        while Date() < deadline {
            let data = try await HTTP.data(
                session, request("GET", "/api/v2.0/core/get_jobs?id=\(jobId)"),
                label: "job status")
            guard let job = try JSONDecoder().decode([Job].self, from: data).first else {
                throw AppError("job \(jobId) not found")
            }
            switch job.state {
            case "SUCCESS":
                return
            case "FAILED", "ABORTED", "ERROR":
                let detail = job.error ?? job.state
                throw AppError(String(detail.split(separator: "\n").first ?? "failed"))
            default:
                if let pct = job.progress?.percent, pct >= 0, pct <= 100 {
                    onProgress(pct / 100)
                }
                try await Task.sleep(nanoseconds: 2_000_000_000)
            }
        }
        throw AppError("job \(jobId) timed out")
    }
}
