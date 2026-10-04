// SPDX-License-Identifier: GPL-3.0-only
import Foundation
import CoreFoundation
import Darwin

/// One sequential caller owns this session. Jobs reconnect to their existing id;
/// mutating calls are never retried because the server may have accepted them.
final class TrueNASRPC {
    private let conn: TrueNASConnection
    private let session: URLSession
    private var socket: URLSessionWebSocketTask?
    private var nextID = 0

    init(_ conn: TrueNASConnection) {
        self.conn = conn
        session = HTTP.session(insecure: conn.acceptInvalidCerts)
    }

    deinit {
        socket?.cancel(with: .goingAway, reason: nil)
        session.invalidateAndCancel()
    }

    static func isUnreachable(_ error: Error) -> Bool {
        (error as? AppError)?.message.hasPrefix("Could not reach TrueNAS") == true
    }

    static func endpoint(_ base: String) throws -> URL {
        guard var parts = URLComponents(string: base),
              let host = parts.host, !host.isEmpty,
              parts.user == nil, parts.password == nil,
              parts.scheme == "https" || parts.scheme == "http" else {
            throw AppError("Invalid TrueNAS server address")
        }
        parts.scheme = parts.scheme == "https" ? "wss" : "ws"
        parts.path = "/api/current"
        parts.query = nil
        parts.fragment = nil
        guard let url = parts.url else { throw AppError("Invalid TrueNAS server address") }
        return url
    }

    func call<T: Decodable>(_ method: String, _ params: [Any]) async throws -> T {
        try Task.checkCancellation()
        if socket == nil {
            let ws = session.webSocketTask(with: try Self.endpoint(conn.normalizedBase))
            ws.maximumMessageSize = 4 * 1024 * 1024
            socket = ws
            ws.resume()
            do {
                let authenticated: Bool = try await exchange(
                    "auth.login_with_api_key", [conn.apiKey.trimmingCharacters(in: .whitespacesAndNewlines)], ws)
                guard authenticated else { throw AppError("Authentication failed — check the API key") }
            } catch {
                disconnect()
                throw error
            }
        }
        guard let ws = socket else { throw AppError("Could not reach TrueNAS (no session)") }
        do {
            return try await exchange(method, params, ws)
        } catch {
            // Any fault ends the conversation; a later operation can log in anew.
            disconnect()
            throw error
        }
    }

    private func disconnect() {
        socket?.cancel(with: .goingAway, reason: nil)
        socket = nil
    }

    private func exchange<T: Decodable>(
        _ method: String, _ params: [Any], _ ws: URLSessionWebSocketTask
    ) async throws -> T {
        nextID += 1
        let id = nextID
        let payload = try JSONSerialization.data(withJSONObject: [
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        ])
        // URLSession's request timeout does not bound reads on an open socket.
        // One deadline covers send, notifications and receive together.
        let timer = DispatchWorkItem { ws.cancel(with: .goingAway, reason: nil) }
        DispatchQueue.global().asyncAfter(deadline: .now() + 30, execute: timer)
        defer { timer.cancel() }
        return try await withTaskCancellationHandler {
            do {
                try await ws.send(.string(String(decoding: payload, as: UTF8.self)))
                while true {
                    try Task.checkCancellation()
                    let message = try await ws.receive()
                    let data: Data
                    switch message {
                    case .string(let text): data = Data(text.utf8)
                    case .data(let bytes): data = bytes
                    @unknown default: throw AppError("\(method): unexpected WebSocket message")
                    }
                    guard let reply = try JSONSerialization.jsonObject(with: data) as? [String: Any],
                          reply["jsonrpc"] as? String == "2.0" else {
                        throw AppError("\(method): invalid JSON-RPC reply")
                    }
                    guard let replyID = reply["id"] as? NSNumber,
                          CFGetTypeID(replyID) != CFBooleanGetTypeID(),
                          replyID == NSNumber(value: id) else { continue }
                    if let error = reply["error"] as? [String: Any] {
                        throw Self.rpcError(method, error)
                    }
                    guard let result = reply["result"] else {
                        throw AppError("\(method): missing result")
                    }
                    let bytes = try JSONSerialization.data(withJSONObject: result, options: .fragmentsAllowed)
                    return try JSONDecoder().decode(T.self, from: bytes)
                }
            } catch {
                if Task.isCancelled { throw CancellationError() }
                if error is AppError { throw error }
                let nsError = error as NSError
                if nsError.domain == NSPOSIXErrorDomain {
                    if nsError.code == Int(EMSGSIZE) {
                        throw AppError("\(method): TrueNAS reply exceeded 4 MB")
                    }
                    throw AppError("Could not reach TrueNAS (\(nsError.localizedDescription))")
                }
                if nsError.domain == NSURLErrorDomain {
                    if let response = ws.response as? HTTPURLResponse {
                        if response.statusCode == 401 || response.statusCode == 403 {
                            throw AppError("Authentication failed — check the API key")
                        }
                        if response.statusCode == 404 || response.statusCode == 405 {
                            throw AppError("TrueNAS JSON-RPC API unavailable — requires SCALE 25.04 or newer")
                        }
                    }
                    if nsError.code == NSURLErrorDataLengthExceedsMaximum {
                        throw AppError("\(method): TrueNAS reply exceeded 4 MB")
                    }
                    throw AppError("Could not reach TrueNAS (\(nsError.localizedDescription))")
                }
                throw AppError("\(method): invalid response")
            }
        } onCancel: {
            ws.cancel(with: .goingAway, reason: nil)
        }
    }

    static func rpcError(_ method: String, _ error: [String: Any]) -> AppError {
        if error["code"] as? Int == -32601 {
            return AppError("\(method): no such method on this TrueNAS version")
        }
        let data = error["data"] as? [String: Any] ?? [:]
        let reason = data["reason"] as? String ?? error["message"] as? String ?? "error"
        let firstLine = String(reason.split(separator: "\n").first ?? "error").prefix(300)
        return AppError("\(method): \(firstLine)")
    }
}
