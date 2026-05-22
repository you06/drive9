#!/usr/bin/env bash
# Rebuild drive9-mobile-core in release mode and regenerate Kotlin / Swift
# bindings under sibling crates.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CLIENTS="$(cd "$ROOT/.." && pwd)"
KOTLIN_OUT="$CLIENTS/drive9-kotlin/lib/src/main/kotlin"
KOTLIN_NATIVE="$CLIENTS/drive9-kotlin/lib/src/main/resources/linux-x86-64"
SWIFT_OUT_FFI="$CLIENTS/drive9-swift/Sources/drive9_mobile_coreFFI"
SWIFT_OUT_API="$CLIENTS/drive9-swift/Sources/Drive9Mobile"

cd "$ROOT"
cargo build --release

# Kotlin bindings (regenerate in place; the .so under resources is loaded by
# JNA when the JVM module starts).
rm -f "$KOTLIN_OUT/uniffi/drive9_mobile_core/drive9_mobile_core.kt"
# Pass --no-format to uniffi-bindgen: without it, uniffi-bindgen
# shells out to ktlint (Kotlin) / swift-format (Swift) on the
# generated file. On this host swift-format hangs on the large
# generated Swift file and pegs a CPU core for hours; passing
# --no-format skips that step entirely. The generated code already
# compiles cleanly and the cosmetic formatting doesn't matter
# because the file is regenerated, not edited by hand.
cargo run --release --bin uniffi-bindgen -- generate \
    --no-format \
    --library target/release/libdrive9_mobile_core.so \
    --language kotlin \
    --out-dir "$KOTLIN_OUT"

mkdir -p "$KOTLIN_NATIVE"
cp target/release/libdrive9_mobile_core.so "$KOTLIN_NATIVE/"

# Swift bindings. The generator writes both a C header / modulemap and a Swift
# source file; we split them between the two SwiftPM targets.
rm -f "$SWIFT_OUT_FFI"/{drive9_mobile_coreFFI.h,module.modulemap}
rm -f "$SWIFT_OUT_API/Drive9MobileGenerated.swift"
TMP_SWIFT="$(mktemp -d)"
trap 'rm -rf "$TMP_SWIFT"' EXIT
cargo run --release --bin uniffi-bindgen -- generate \
    --no-format \
    --library target/release/libdrive9_mobile_core.so \
    --language swift \
    --out-dir "$TMP_SWIFT"

mkdir -p "$SWIFT_OUT_FFI" "$SWIFT_OUT_API"
cp "$TMP_SWIFT/drive9_mobile_coreFFI.h" "$SWIFT_OUT_FFI/"
# Rewrite the modulemap so its module name matches the import in the generated
# Swift file and so it is portable to non-Darwin builds (Linux test runs).
cat > "$SWIFT_OUT_FFI/module.modulemap" <<'EOF'
module drive9_mobile_coreFFI {
    header "drive9_mobile_coreFFI.h"
    export *
}
EOF
cp "$TMP_SWIFT/drive9_mobile_core.swift" "$SWIFT_OUT_API/Drive9MobileGenerated.swift"

echo "Regenerated bindings."
