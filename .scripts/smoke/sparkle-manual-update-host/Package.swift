// swift-tools-version:6.0

import PackageDescription

let package = Package(
  name: "SparkleManualUpdateSmokeHost",
  platforms: [.macOS(.v14)],
  products: [
    .executable(
      name: "sparkle-manual-update-smoke-host",
      targets: ["SparkleManualUpdateSmokeHost"])
  ],
  dependencies: [
    .package(path: "../../../apple/GramDriveSupport"),
    .package(url: "https://github.com/sparkle-project/Sparkle.git", exact: "2.9.5"),
  ],
  targets: [
    .executableTarget(
      name: "SparkleManualUpdateSmokeHost",
      dependencies: [
        .product(name: "GramDriveCompanion", package: "GramDriveSupport"),
        .product(name: "Sparkle", package: "Sparkle"),
      ])
  ])
