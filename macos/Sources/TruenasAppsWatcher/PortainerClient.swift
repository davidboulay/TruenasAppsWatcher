// SPDX-License-Identifier: GPL-3.0-only
//
// Unmanaged-container backend, mirroring the Linux applet: containers are
// listed via Portainer's Docker API proxy (skipping the `ix-*` compose
// projects TrueNAS manages), "update available" means the image tag's digest
// at the registry no longer matches a local RepoDigest, and applying an
// update pulls the image (with streamed per-layer progress) then recreates
// the container through Portainer.

import Foundation

struct PortainerClient {
    let conn: PortainerConnection
    private let session: URLSession

    init(_ conn: PortainerConnection) {
        self.conn = conn
        self.session = HTTP.session(insecure: conn.acceptInvalidCerts)
    }

    private func request(_ method: String, _ path: String, body: Data? = nil) -> URLRequest {
        var req = URLRequest(url: URL(string: conn.normalizedBase + path)!)
        req.httpMethod = method
        req.setValue(conn.apiKey.trimmingCharacters(in: .whitespaces), forHTTPHeaderField: "X-API-Key")
        if let body {
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
            req.httpBody = body
        }
        return req
    }

    // MARK: Wire types

    private struct Endpoint: Decodable {
        let id: Int
        let type: Int?
        enum CodingKeys: String, CodingKey {
            case id = "Id"
            case type = "Type"
        }
    }

    private struct ApiContainer: Decodable {
        let id: String
        let names: [String]?
        let image: String?
        let imageID: String?
        let labels: [String: String]?
        enum CodingKeys: String, CodingKey {
            case id = "Id"
            case names = "Names"
            case image = "Image"
            case imageID = "ImageID"
            case labels = "Labels"
        }

        var displayName: String {
            if let n = names?.first?.trimmingCharacters(in: CharacterSet(charactersIn: "/")),
               !n.isEmpty {
                return n
            }
            return String(id.prefix(12))
        }

        /// Containers of a TrueNAS app run in an `ix-<app>` compose project;
        /// those are already covered by the apps check.
        var isTrueNASManaged: Bool {
            (labels?["com.docker.compose.project"] ?? "").hasPrefix("ix-")
        }
    }

    private struct ImageInspect: Decodable {
        let repoDigests: [String]?
        enum CodingKeys: String, CodingKey {
            case repoDigests = "RepoDigests"
        }
    }

    // MARK: Check

    /// Check all Docker environments known to Portainer for containers whose
    /// image tag has a newer build at the registry. Never throws as a whole.
    func checkContainers() async -> ContainerReport {
        var report = ContainerReport()
        guard conn.isConfigured else { return report }

        let endpoints: [Endpoint]
        do {
            let data = try await HTTP.data(
                session, request("GET", "/api/endpoints?limit=100"), label: "Portainer endpoints")
            endpoints = try JSONDecoder().decode([Endpoint].self, from: data)
        } catch {
            // URLError = transport-level (offline, refused, DNS, timeout).
            report.unreachable = error is URLError
            report.errors.append(error.localizedDescription)
            return report
        }

        // Containers often share an image; ask the registry once per unique ref.
        var digestCache: [String: Result<String, Error>] = [:]

        // Types 1 and 2 are Docker environments (local socket / agent).
        for ep in endpoints where ep.type == 1 || ep.type == 2 {
            let containers: [ApiContainer]
            do {
                let data = try await HTTP.data(
                    session,
                    request("GET", "/api/endpoints/\(ep.id)/docker/containers/json"),
                    label: "containers")
                containers = try JSONDecoder().decode([ApiContainer].self, from: data)
            } catch {
                report.errors.append(error.localizedDescription)
                continue
            }

            for c in containers {
                guard !c.isTrueNASManaged,
                      let image = c.image, !image.isEmpty,
                      // Images pinned by digest or referenced by id can't drift.
                      !image.contains("@"), !image.hasPrefix("sha256:")
                else { continue }
                report.totalContainers += 1

                let local: [String]
                do {
                    local = try await repoDigests(endpointId: ep.id, imageId: c.imageID ?? image)
                } catch {
                    report.errors.append("\(c.displayName): \(error.localizedDescription)")
                    continue
                }
                // Locally built image — nothing at a registry to compare with.
                if local.isEmpty { continue }

                let remote: Result<String, Error>
                if let cached = digestCache[image] {
                    remote = cached
                } else {
                    do {
                        remote = .success(try await Registry.remoteDigest(
                            session: session, image: image))
                    } catch {
                        remote = .failure(error)
                    }
                    digestCache[image] = remote
                }

                switch remote {
                case .success(let digest):
                    if !local.contains(digest) {
                        report.updates.append(UpdateItem(
                            name: c.displayName, title: c.displayName,
                            current: image, latest: "",
                            kind: .container(endpointId: ep.id, containerId: c.id, image: image)))
                    }
                case .failure(let error):
                    report.errors.append("\(c.displayName) (\(image)): \(error.localizedDescription)")
                }
            }
        }

        report.updates.sort { $0.title.lowercased() < $1.title.lowercased() }
        return report
    }

    /// The `sha256:…` parts of an image's RepoDigests.
    private func repoDigests(endpointId: Int, imageId: String) async throws -> [String] {
        let data = try await HTTP.data(
            session,
            request("GET", "/api/endpoints/\(endpointId)/docker/images/\(imageId)/json"),
            label: "image inspect")
        let inspect = try JSONDecoder().decode(ImageInspect.self, from: data)
        return (inspect.repoDigests ?? []).compactMap { entry in
            guard let at = entry.firstIndex(of: "@") else { return nil }
            return String(entry[entry.index(after: at)...])
        }
    }

    // MARK: Apply

    /// Pull an image via the Docker API proxy, reporting aggregate progress
    /// from the per-layer events the daemon streams back (download counts as
    /// 70% of a layer, extraction as 30%).
    func pullImage(endpointId: Int, image: String, onProgress: (Double) -> Void) async throws {
        let (name, tag) = Registry.splitNameTag(image)
        var components = URLComponents(
            string: conn.normalizedBase + "/api/endpoints/\(endpointId)/docker/images/create")!
        components.queryItems = [
            URLQueryItem(name: "fromImage", value: name),
            URLQueryItem(name: "tag", value: tag),
        ]
        var req = URLRequest(url: components.url!)
        req.httpMethod = "POST"
        req.setValue(conn.apiKey.trimmingCharacters(in: .whitespaces), forHTTPHeaderField: "X-API-Key")

        let (bytes, response) = try await session.bytes(for: req)
        guard let http = response as? HTTPURLResponse, (200..<300).contains(http.statusCode) else {
            let code = (response as? HTTPURLResponse)?.statusCode ?? 0
            throw AppError("pull failed: HTTP \(code)")
        }

        var layers: [String: Double] = [:]
        for try await line in bytes.lines {
            guard let data = line.data(using: .utf8),
                  let event = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
            else { continue }
            if let err = event["error"] as? String {
                throw AppError("pull: \(err)")
            }
            guard let id = event["id"] as? String else { continue }
            let detail = event["progressDetail"] as? [String: Any]
            let current = (detail?["current"] as? Double) ?? 0
            let total = (detail?["total"] as? Double) ?? 0
            let fraction: Double? = total > 0 ? min(current / total, 1) : nil

            let layerValue: Double?
            switch event["status"] as? String ?? "" {
            case "Pulling fs layer", "Waiting":
                layerValue = 0
            case "Downloading":
                layerValue = fraction.map { $0 * 0.7 }
            case "Verifying Checksum", "Download complete":
                layerValue = 0.7
            case "Extracting":
                layerValue = fraction.map { 0.7 + $0 * 0.3 }
            case "Pull complete", "Already exists":
                layerValue = 1
            default:
                layerValue = nil
            }
            if let v = layerValue {
                layers[id] = v
                onProgress(layers.values.reduce(0, +) / Double(layers.count))
            }
        }
    }

    /// Recreate a container with its existing configuration. With `pull`, the
    /// image is re-pulled inside this (blocking, progress-less) request.
    func recreateContainer(endpointId: Int, containerId: String, pull: Bool) async throws {
        let payload = try JSONSerialization.data(withJSONObject: ["PullImage": pull])
        _ = try await HTTP.data(
            session,
            request("POST", "/api/docker/\(endpointId)/containers/\(containerId)/recreate",
                    body: payload),
            label: "recreate")
    }
}
