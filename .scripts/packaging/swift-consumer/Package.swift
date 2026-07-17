// swift-tools-version:5.9
//
// Minimal Swift package that consumes the packaged GramDrive core exactly the
// way a native host will: as a path dependency on the shipped artifact, with
// no knowledge of the Rust workspace, cargo, or the build that produced it.
//
// `.scripts/packaging/build_core_artifacts.py` copies this package next to the
// staged artifact and resolves `../GramDriveCore` to it, so the dependency edge
// below is the real integration path (SwiftPM -> package -> binaryTarget ->
// XCFramework -> Rust staticlib), not a compiler invocation arranged to
// succeed. If the artifact is missing a header, a modulemap, a slice for this
// architecture, or the Info.plist SwiftPM needs, resolution fails here rather
// than in a native host months later.
//
// The dependency is by path, and SwiftPM takes a path dependency's identity
// from its directory name -- which is why the pipeline stages the artifact in a
// directory named GramDriveCore rather than something generic.
//
// Owned by TASK-260715-3akqs8. The deep contract assertions (async, progress,
// structured errors, cancellation) belong to the bindings smoke gate
// (TASK-260715-265gqq); this package proves the *packaging*.

import PackageDescription

let package = Package(
    name: "GramDriveCoreVerify",
    // POL-5: macOS 14+ arm64 is the v1 support matrix. Stated so SwiftPM
    // rejects a platform the artifact does not claim to support.
    platforms: [.macOS(.v14)],
    dependencies: [
        .package(path: "../GramDriveCore")
    ],
    targets: [
        .executableTarget(
            name: "GramDriveVerify",
            dependencies: [
                .product(name: "GramDriveCore", package: "GramDriveCore")
            ]
        )
    ]
)
