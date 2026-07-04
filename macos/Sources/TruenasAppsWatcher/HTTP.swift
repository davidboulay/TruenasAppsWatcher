// SPDX-License-Identifier: GPL-3.0-only
//
// Shared URLSession helpers, including the trust override for the
// self-signed certificate TrueNAS ships with.

import Foundation

/// Accepts any server certificate. Only installed when the user's
/// "accept self-signed certificate" setting is on (the default, matching
/// how TrueNAS ships).
final class InsecureTrustDelegate: NSObject, URLSessionDelegate {
    func urlSession(
        _ session: URLSession,
        didReceive challenge: URLAuthenticationChallenge,
        completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void
    ) {
        if challenge.protectionSpace.authenticationMethod == NSURLAuthenticationMethodServerTrust,
           let trust = challenge.protectionSpace.serverTrust {
            completionHandler(.useCredential, URLCredential(trust: trust))
        } else {
            completionHandler(.performDefaultHandling, nil)
        }
    }
}

enum HTTP {
    static func session(insecure: Bool) -> URLSession {
        let config = URLSessionConfiguration.ephemeral
        config.timeoutIntervalForRequest = 30
        // Job polling and image pulls can hold a request open for a while.
        config.timeoutIntervalForResource = 30 * 60
        if insecure {
            return URLSession(configuration: config, delegate: InsecureTrustDelegate(), delegateQueue: nil)
        }
        return URLSession(configuration: config)
    }

    /// Perform a request and return the body, mapping non-2xx to a thrown error.
    static func data(
        _ session: URLSession,
        _ request: URLRequest,
        label: String
    ) async throws -> Data {
        let (data, response) = try await session.data(for: request)
        guard let http = response as? HTTPURLResponse else {
            throw AppError("\(label): not an HTTP response")
        }
        guard (200..<300).contains(http.statusCode) else {
            if http.statusCode == 401 || http.statusCode == 403 {
                throw AppError("\(label): authentication failed — check the key")
            }
            let snippet = String(data: data.prefix(160), encoding: .utf8) ?? ""
            throw AppError("\(label): HTTP \(http.statusCode) \(snippet)")
        }
        return data
    }
}

/// A plain human-readable error string.
struct AppError: LocalizedError {
    let message: String
    init(_ message: String) { self.message = message }
    var errorDescription: String? { message }
}
