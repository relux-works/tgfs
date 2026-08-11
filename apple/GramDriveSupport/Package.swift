// swift-tools-version:6.0
//
// GramDriveSupport — the Apple provider-support package (TASK-260715-gnsa2s;
// `.spec/architecture.md`): App Group container resolution, shared-state
// access per process role, and the cross-process change doorbell. The app,
// the companion agent, and the File Provider extension all link this
// package so every process derives identical container paths and follows
// the same multi-process rules.
//
// The core dependency is a *built artifact*, never committed: `make package`
// stages the GramDriveCore SwiftPM package (XCFramework + generated
// bindings) at .temp/packaging/GramDriveCore, and this manifest resolves it
// by path. Building here without the artifact fails at resolution — run
// `make package` first, or point GRAMDRIVE_CORE_PACKAGE at a staged
// artifact. The path dependency's identity comes from its directory name
// (GramDriveCore), which the packaging pipeline guarantees.

import Foundation
import PackageDescription

let corePackagePath =
    ProcessInfo.processInfo.environment["GRAMDRIVE_CORE_PACKAGE"]
    ?? "../../.temp/packaging/GramDriveCore"

// A tdjson-linked core (staged with GRAMDRIVE_TDLIB_ARTIFACT_DIR set;
// BUG-260720-3i74u1) declares `-ltdjson`, and the runtime library sits in
// the artifact's `lib/`. Every link-producing target gets that directory as
// a search path so `swift build`/`swift test` work against either staging;
// with a hermetic core the directory simply does not exist and the flag is
// inert. Root-package-only linker flags are fine here: this package is
// always built as the root (the app packaging pipeline included).
let coreLinkerSettings: [LinkerSetting] = [
    .unsafeFlags(["-L\(corePackagePath)/lib"])
]

// App extensions are entered by the system at Foundation's NSExtensionMain,
// exactly like Xcode's App Extension product type. Calling NSExtensionMain
// from a normal Swift executable main recursively re-enters the extension
// runtime on macOS and prevents the principal class from receiving callbacks.
let fileProviderExtensionLinkerSettings: [LinkerSetting] = coreLinkerSettings + [
    .unsafeFlags(
        ["-Xlinker", "-e", "-Xlinker", "_NSExtensionMain"],
        .when(platforms: [.macOS]))
]

let package = Package(
    name: "GramDriveSupport",
    // POL-5 / DEC-017: macOS 14+ arm64 is the v1 support matrix.
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "GramDriveSupport", targets: ["GramDriveSupport"]),
        // The companion-agent lifecycle library (TASK-260715-1yx9ly):
        // single-instance guard, launch-at-login policy, transfer drain,
        // health snapshot and its bounded local IPC channel. The agent
        // executable and the app shell both link it.
        .library(name: "GramDriveAgentCore", targets: ["GramDriveAgentCore"]),
        // The companion shell (TASK-260715-13pxnu): SwiftUI menu-bar UX —
        // authorization flow, account/provider status, cache/Archive settings,
        // diagnostics, repair, removal — over the CompanionBackend seam.
        .library(name: "GramDriveCompanion", targets: ["GramDriveCompanion"]),
        // The File Provider domain layer (TASK-260715-3s44pc): stable
        // per-account domain identity, the idempotent domain reconciler,
        // and the thin NSFileProviderReplicatedExtension skeleton over
        // shared state (no TDLib — DEC-006). The containing app links this
        // to register domains; the extension target links it as its
        // principal-class implementation.
        .library(name: "GramDriveFileProvider", targets: ["GramDriveFileProvider"]),
        // The companion background agent (launch agent) binary itself.
        .executable(name: "gramdrive-agent", targets: ["GramDriveAgentMain"]),
        // The companion shell app (menu-bar).
        .executable(name: "gramdrive-companion", targets: ["GramDriveCompanionMain"]),
        // The File Provider extension's appex binary (TASK-260715-1dk9ik):
        // an NSExtensionMain entry point over the GramDriveFileProvider
        // principal class. SwiftPM cannot emit an `.appex`, so packaging
        // (.scripts/apple-app/build_app_bundle.py) wraps this executable in the
        // bundle and Info.plist the system loads.
        .executable(
            name: "gramdrive-fileprovider", targets: ["GramDriveFileProviderExtensionApp"]),
        // Used by .scripts/smoke/run_shared_state_smoke.py: reader, watcher
        // and doorbell-poster processes for the two-process smoke.
        .executable(name: "gramdrive-shared-state-smoke", targets: ["SharedStateSmoke"]),
    ],
    dependencies: [
        .package(name: "GramDriveCore", path: corePackagePath),
        .package(url: "https://github.com/sparkle-project/Sparkle.git", exact: "2.9.5"),
    ],
    targets: [
        .target(
            name: "GramDriveSupport",
            dependencies: [
                .product(name: "GramDriveCore", package: "GramDriveCore")
            ]
        ),
        .target(
            name: "GramDriveAgentCore",
            dependencies: [
                "GramDriveSupport",
                .product(name: "GramDriveCore", package: "GramDriveCore"),
            ]
        ),
        .target(
            name: "GramDriveCompanion",
            dependencies: [
                "GramDriveAgentCore",
                "GramDriveSupport",
                .product(name: "GramDriveCore", package: "GramDriveCore"),
            ]
        ),
        .executableTarget(
            name: "GramDriveAgentMain",
            dependencies: [
                "GramDriveAgentCore",
                "GramDriveSupport",
                .product(name: "GramDriveCore", package: "GramDriveCore"),
            ],
            linkerSettings: coreLinkerSettings
        ),
        .target(
            name: "GramDriveFileProvider",
            dependencies: [
                "GramDriveSupport",
                .product(name: "GramDriveCore", package: "GramDriveCore"),
            ]
        ),
        .executableTarget(
            name: "GramDriveCompanionMain",
            dependencies: [
                "GramDriveCompanion",
                .product(name: "Sparkle", package: "Sparkle"),
                "GramDriveAgentCore",
                "GramDriveFileProvider",
                "GramDriveSupport",
                .product(name: "GramDriveCore", package: "GramDriveCore"),
            ],
            linkerSettings: coreLinkerSettings
        ),
        .executableTarget(
            name: "GramDriveFileProviderExtensionApp",
            dependencies: [
                "GramDriveFileProvider",
                "GramDriveSupport",
                .product(name: "GramDriveCore", package: "GramDriveCore"),
            ],
            linkerSettings: fileProviderExtensionLinkerSettings
        ),
        .executableTarget(
            name: "SharedStateSmoke",
            dependencies: [
                "GramDriveFileProvider",
                "GramDriveSupport",
                .product(name: "GramDriveCore", package: "GramDriveCore"),
            ],
            linkerSettings: coreLinkerSettings
        ),
        .testTarget(
            name: "GramDriveSupportTests",
            dependencies: [
                "GramDriveSupport",
                .product(name: "GramDriveCore", package: "GramDriveCore"),
            ],
            linkerSettings: coreLinkerSettings
        ),
        .testTarget(
            name: "GramDriveAgentCoreTests",
            dependencies: [
                "GramDriveAgentCore",
                "GramDriveSupport",
                .product(name: "GramDriveCore", package: "GramDriveCore"),
            ],
            linkerSettings: coreLinkerSettings
        ),
        .testTarget(
            name: "GramDriveCompanionTests",
            dependencies: [
                "GramDriveCompanion",
                "GramDriveAgentCore",
                "GramDriveSupport",
                .product(name: "GramDriveCore", package: "GramDriveCore"),
            ],
            linkerSettings: coreLinkerSettings
        ),
        .testTarget(
            name: "GramDriveFileProviderTests",
            dependencies: [
                "GramDriveFileProvider",
                "GramDriveSupport",
                .product(name: "GramDriveCore", package: "GramDriveCore"),
            ],
            linkerSettings: coreLinkerSettings
        ),
    ]
)
