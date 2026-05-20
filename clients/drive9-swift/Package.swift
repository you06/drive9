// swift-tools-version: 5.9
import PackageDescription

// The native library `libdrive9_mobile_core` is built by the `drive9-mobile-core`
// Cargo crate. SwiftPM does not run cargo for us, so the package looks for the
// library on the host's library search path. For local development point
// `DRIVE9_MOBILE_LIB_PATH` or `LD_LIBRARY_PATH` (Linux) / `DYLD_LIBRARY_PATH`
// (macOS) at `../drive9-mobile-core/target/release/`. Production builds for
// iOS / macOS / Android should bundle the corresponding native artifact via
// the host build pipeline.

let package = Package(
    name: "Drive9Mobile",
    products: [
        .library(name: "Drive9Mobile", targets: ["Drive9Mobile"]),
    ],
    targets: [
        .systemLibrary(
            name: "drive9_mobile_coreFFI",
            path: "Sources/drive9_mobile_coreFFI"
        ),
        .target(
            name: "Drive9Mobile",
            dependencies: ["drive9_mobile_coreFFI"],
            path: "Sources/Drive9Mobile",
            linkerSettings: [
                .linkedLibrary("drive9_mobile_core"),
                .unsafeFlags(["-L", "../drive9-mobile-core/target/release"]),
            ]
        ),
        .testTarget(
            name: "Drive9MobileTests",
            dependencies: ["Drive9Mobile"],
            path: "Tests/Drive9MobileTests"
        ),
    ]
)
