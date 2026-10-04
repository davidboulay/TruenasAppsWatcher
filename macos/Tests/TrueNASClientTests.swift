// SPDX-License-Identifier: GPL-3.0-only
import XCTest
@testable import TruenasAppsWatcher

final class TrueNASClientTests: XCTestCase {
    private var server: Process?

    private func client(_ scenario: String = "normal", key: String = "test-key") throws -> TrueNASClient {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/python3")
        let fixture = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
            .appendingPathComponent("Fixtures/rpc-server.py")
        process.arguments = [fixture.path, scenario]
        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = FileHandle.nullDevice
        try process.run()
        server = process
        let output = pipe.fileHandleForReading.availableData
        let port = try XCTUnwrap(Int(String(decoding: output, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)))
        return TrueNASClient(TrueNASConnection(baseURL: "http://127.0.0.1:\(port)", apiKey: key))
    }

    override func tearDown() {
        server?.terminate()
        server?.waitUntilExit()
        server = nil
        super.tearDown()
    }

    func testQuerySkipsNotifications() async throws {
        let report = await (try client()).checkApps(refreshCatalog: false)
        XCTAssertEqual(report.totalApps, 1)
        XCTAssertEqual(report.upgrades.first?.title, "Demo")
        XCTAssertTrue(report.errors.isEmpty)
    }

    func testBadKeyIsNotTransient() async throws {
        let report = await (try client(key: "wrong-key")).checkApps(refreshCatalog: false)
        XCTAssertFalse(report.unreachable)
        XCTAssertTrue(report.errors.first?.contains("Authentication failed") == true)
    }

    func testDroppedJobReconnectsWithoutReplayingUpgrade() async throws {
        let client = try client("drop")
        let item = UpdateItem(name: "demo", title: "Demo", current: "1", latest: "2", kind: .app)
        let id = try await client.startUpdateJob(item)
        XCTAssertEqual(id, 42)
        var progress = 0.0
        try await client.waitJob(id) { progress = $0 }
        XCTAssertEqual(progress, 1)
    }

    func testImageArguments() async throws {
        let client = try client()
        let item = UpdateItem(name: "demo", title: "Demo", current: "", latest: "", kind: .image)
        let id = try await client.startUpdateJob(item)
        try await client.waitJob(id) { _ in }
    }

    func testFailedJobIsNotTransient() async throws {
        let client = try client("failed")
        let item = UpdateItem(name: "demo", title: "Demo", current: "", latest: "", kind: .image)
        let id = try await client.startUpdateJob(item)
        do {
            try await client.waitJob(id) { _ in }
            XCTFail("Expected job failure")
        } catch {
            XCTAssertEqual(error.localizedDescription, "pull failed")
            XCTAssertFalse(TrueNASRPC.isUnreachable(error))
        }
    }

    func testCatalogSync() async throws {
        let report = await (try client()).checkApps(refreshCatalog: true)
        XCTAssertTrue(report.errors.isEmpty)
        XCTAssertEqual(report.totalApps, 1)
    }

    func testMalformedReplyIsNotTransient() async throws {
        let report = await (try client("malformed")).checkApps(refreshCatalog: false)
        XCTAssertFalse(report.unreachable)
        XCTAssertTrue(report.errors.first?.contains("invalid response") == true)
    }

    func testOversizeReplyIsNotTransient() async throws {
        let report = await (try client("oversize")).checkApps(refreshCatalog: false)
        XCTAssertFalse(report.unreachable)
        XCTAssertTrue(report.errors.first?.contains("4 MB") == true, "\(report.errors)")
    }

    func testEndpointAndErrorClassification() throws {
        XCTAssertEqual(try TrueNASRPC.endpoint("https://nas:443/old?x=1").absoluteString, "wss://nas:443/api/current")
        XCTAssertThrowsError(try TrueNASRPC.endpoint("https://user:secret@nas"))
        let error = TrueNASRPC.rpcError("app.upgrade", ["code": -32001, "data": ["reason": "Not authorized\ntrace", "errname": "EACCES"]])
        XCTAssertEqual(error.message, "app.upgrade: Not authorized")
        XCTAssertFalse(TrueNASRPC.isUnreachable(error))
    }
}
