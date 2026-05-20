# drive9-swift

Swift consumer of the `drive9-mobile-core` UniFFI bindings as a SwiftPM
package.

## Layout

- `Sources/Drive9Mobile/Drive9.swift` — hand-written idiomatic facade. Exposes
  `Drive9Client` with `async throws` methods dispatched on detached tasks.
  Consumers should depend on this module, not the raw generated bindings.
- `Sources/Drive9Mobile/Drive9MobileGenerated.swift` — UniFFI-generated Swift.
  Regenerate with `clients/drive9-mobile-core/scripts/regenerate-bindings.sh`.
- `Sources/drive9_mobile_coreFFI/` — generated C header + `module.modulemap`
  that the Swift target imports from. The modulemap is rewritten by the
  regeneration script so the package builds on Linux as well as Darwin.

## Native library

SwiftPM does not invoke cargo. Before running `swift build` or `swift test`,
build the native library and put it on the library search path:

```
(cd ../drive9-mobile-core && cargo build --release)
export LD_LIBRARY_PATH=$(pwd)/../drive9-mobile-core/target/release:$LD_LIBRARY_PATH   # Linux
export DYLD_LIBRARY_PATH=$(pwd)/../drive9-mobile-core/target/release:$DYLD_LIBRARY_PATH # macOS
```

iOS / macOS shipping builds should bundle the matching universal binary
through your normal Xcode pipeline rather than relying on `LD_LIBRARY_PATH`.

## Smoke test

```
swift test
```

Runs `Drive9Tests` end-to-end through the C interop → `libdrive9_mobile_core`
→ `drive9-rs` against an in-process `MockHTTPServer`.
