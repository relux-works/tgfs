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
        // The companion background agent (launch agent) binary itself.
        .executable(name: "gramdrive-agent", targets: ["GramDriveAgentMain"]),
        // The companion shell app (menu-bar).
        .executable(name: "gramdrive-companion", targets: ["GramDriveCompanionMain"]),
        // Used by .scripts/smoke/run_shared_state_smoke.py: reader, watcher
        // and doorbell-poster processes for the two-process smoke.
        .executable(name: "gramdrive-shared-state-smoke", targets: ["SharedStateSmoke"]),
    ],
    dependencies: [
        .package(name: "GramDriveCore", path: corePackagePath)
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
            ]
        ),
        .executableTarget(
            name: "GramDriveCompanionMain",
            dependencies: [
                "GramDriveCompanion",
                "GramDriveAgentCore",
                "GramDriveSupport",
                .product(name: "GramDriveCore", package: "GramDriveCore"),
            ]
        ),
        .executableTarget(
            name: "SharedStateSmoke",
            dependencies: [
                "GramDriveSupport",
                .product(name: "GramDriveCore", package: "GramDriveCore"),
            ]
        ),
        .testTarget(
            name: "GramDriveSupportTests",
            dependencies: [
                "GramDriveSupport",
                .product(name: "GramDriveCore", package: "GramDriveCore"),
            ]
        ),
        .testTarget(
            name: "GramDriveAgentCoreTests",
            dependencies: [
                "GramDriveAgentCore",
                "GramDriveSupport",
                .product(name: "GramDriveCore", package: "GramDriveCore"),
            ]
        ),
        .testTarget(
            name: "GramDriveCompanionTests",
            dependencies: [
                "GramDriveCompanion",
                "GramDriveAgentCore",
                "GramDriveSupport",
                .product(name: "GramDriveCore", package: "GramDriveCore"),
            ]
        ),
    ]
)
