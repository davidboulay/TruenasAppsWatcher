// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "TruenasAppsWatcher",
    platforms: [.macOS(.v13)],
    targets: [
        .executableTarget(
            name: "TruenasAppsWatcher",
            path: "Sources/TruenasAppsWatcher"
        ),
        .testTarget(
            name: "TrueNASClientTests",
            dependencies: ["TruenasAppsWatcher"],
            path: "Tests",
            exclude: ["Fixtures"]
        )
    ]
)
