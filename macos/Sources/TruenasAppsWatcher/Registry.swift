// SPDX-License-Identifier: GPL-3.0-only
//
// Registry digest lookup: ask an image's registry for the current digest of
// its tag, handling the anonymous Bearer-token dance used by Docker Hub,
// ghcr.io, lscr.io, and friends.

import Foundation

enum Registry {
    /// Accept headers covering Docker and OCI manifests (and their multi-arch
    /// list/index forms, which is what a tag's top-level digest usually is).
    private static let manifestAccept = [
        "application/vnd.docker.distribution.manifest.list.v2+json",
        "application/vnd.oci.image.index.v1+json",
        "application/vnd.docker.distribution.manifest.v2+json",
        "application/vnd.oci.image.manifest.v1+json",
    ].joined(separator: ", ")

    struct ImageRef: Equatable {
        let registry: String
        let repo: String
        let tag: String
    }

    /// Split an image reference into name and tag (default "latest"). A ':'
    /// only counts as a tag separator after the last '/', otherwise it's a
    /// registry port.
    static func splitNameTag(_ image: String) -> (name: String, tag: String) {
        if let colon = image.lastIndex(of: ":") {
            let tag = String(image[image.index(after: colon)...])
            if !tag.contains("/") {
                return (String(image[..<colon]), tag)
            }
        }
        return (image, "latest")
    }

    /// Apply Docker's defaulting rules (Docker Hub, `library/`, `latest`).
    static func parseImageRef(_ image: String) -> ImageRef {
        let (name, tag) = splitNameTag(image)
        if let slash = name.firstIndex(of: "/") {
            let host = String(name[..<slash])
            let rest = String(name[name.index(after: slash)...])
            // First segment is a host only if it looks like one — that's how
            // Docker itself disambiguates.
            if host.contains(".") || host.contains(":") || host == "localhost" {
                return ImageRef(registry: host, repo: rest, tag: tag)
            }
        }
        let repo = name.contains("/") ? name : "library/\(name)"
        return ImageRef(registry: "registry-1.docker.io", repo: repo, tag: tag)
    }

    /// The current digest of the image's tag at its registry.
    static func remoteDigest(session: URLSession, image: String) async throws -> String {
        let ref = parseImageRef(image)
        let url = URL(string: "https://\(ref.registry)/v2/\(ref.repo)/manifests/\(ref.tag)")!

        func head(token: String?) async throws -> HTTPURLResponse {
            var req = URLRequest(url: url)
            req.httpMethod = "HEAD"
            req.setValue(manifestAccept, forHTTPHeaderField: "Accept")
            if let token {
                req.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
            }
            let (_, response) = try await session.data(for: req)
            guard let http = response as? HTTPURLResponse else {
                throw AppError("registry: not an HTTP response")
            }
            return http
        }

        var response = try await head(token: nil)
        if response.statusCode == 401 {
            let challenge = response.value(forHTTPHeaderField: "Www-Authenticate") ?? ""
            let token = try await fetchToken(session: session, challenge: challenge, repo: ref.repo)
            response = try await head(token: token)
        }
        guard (200..<300).contains(response.statusCode) else {
            throw AppError("registry: HTTP \(response.statusCode) for \(ref.tag)")
        }
        guard let digest = response.value(forHTTPHeaderField: "Docker-Content-Digest") else {
            throw AppError("registry did not return a digest")
        }
        return digest
    }

    /// Fetch an anonymous pull token from the auth service named in a
    /// `WWW-Authenticate: Bearer realm="…",service="…",scope="…"` challenge.
    private static func fetchToken(
        session: URLSession, challenge: String, repo: String
    ) async throws -> String {
        let params = parseChallenge(challenge)
        guard let realm = params["realm"], var components = URLComponents(string: realm) else {
            throw AppError("registry auth: no realm in challenge")
        }
        var query: [URLQueryItem] = []
        if let service = params["service"] {
            query.append(URLQueryItem(name: "service", value: service))
        }
        query.append(URLQueryItem(
            name: "scope", value: params["scope"] ?? "repository:\(repo):pull"))
        components.queryItems = query
        guard let url = components.url else {
            throw AppError("registry auth: bad realm URL")
        }

        let data = try await HTTP.data(session, URLRequest(url: url), label: "registry auth")
        guard let obj = try JSONSerialization.jsonObject(with: data) as? [String: Any],
              let token = (obj["token"] ?? obj["access_token"]) as? String
        else {
            throw AppError("registry auth: no token in response")
        }
        return token
    }

    /// Parse the `k="v"` pairs of a Bearer challenge.
    static func parseChallenge(_ challenge: String) -> [String: String] {
        var out: [String: String] = [:]
        let stripped = challenge.hasPrefix("Bearer")
            ? String(challenge.dropFirst("Bearer".count)) : challenge
        for pair in stripped.split(separator: ",") {
            guard let eq = pair.firstIndex(of: "=") else { continue }
            let key = pair[..<eq].trimmingCharacters(in: .whitespaces)
            let value = pair[pair.index(after: eq)...]
                .trimmingCharacters(in: .whitespaces)
                .trimmingCharacters(in: CharacterSet(charactersIn: "\""))
            out[key] = value
        }
        return out
    }
}
