// SPDX-License-Identifier: GPL-3.0-only
//
// TrueNAS SCALE JSON-RPC 2.0 client over /api/current (25.04+).

import Foundation

struct TrueNASClient {
    let conn: TrueNASConnection
    private let rpc: TrueNASRPC

    init(_ conn: TrueNASConnection) {
        self.conn = conn
        self.rpc = TrueNASRPC(conn)
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
            do {
                let jobId: Int = try await rpc.call("catalog.sync", [])
                try await waitJob(jobId) { _ in }
            } catch {
                report.errors.append("Catalog refresh failed: \(error.localizedDescription)")
            }
        }

        do {
            let apps: [RawApp] = try await rpc.call("app.query", [])
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
            // Transport failures are transient; auth/protocol faults are not.
            report.unreachable = TrueNASRPC.isUnreachable(error)
            report.errors.append(error.localizedDescription)
        }
        return report
    }

    // MARK: Jobs

    /// Start the middleware job that applies one app/image update; returns the job id.
    func startUpdateJob(_ item: UpdateItem) async throws -> Int {
        switch item.kind {
        case .app:
            return try await rpc.call("app.upgrade", [item.name, ["app_version": "latest"]])
        case .image:
            return try await rpc.call("app.pull_images", [item.name, ["redeploy": true]])
        case .container:
            throw AppError("container updates go through Portainer")
        }
    }

    /// Poll a job until it finishes, reporting its own 0...1 progress.
    func waitJob(_ jobId: Int, onProgress: (Double) -> Void) async throws {
        let deadline = Date().addingTimeInterval(30 * 60)
        var missing = 0
        while Date() < deadline {
            try Task.checkCancellation()
            let jobs: [Job]
            do {
                jobs = try await rpc.call("core.get_jobs", [[["id", "=", jobId]]])
            } catch where TrueNASRPC.isUnreachable(error) {
                // Re-authenticate and watch the same job. Never replay an upgrade.
                try await Task.sleep(nanoseconds: 5_000_000_000)
                continue
            }
            guard let job = jobs.first else {
                missing += 1
                if missing >= 3 { throw AppError("job \(jobId) not found") }
                try await Task.sleep(nanoseconds: 2_000_000_000)
                continue
            }
            missing = 0
            switch job.state {
            case "SUCCESS":
                onProgress(1)
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
