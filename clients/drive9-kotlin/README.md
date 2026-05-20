# drive9-kotlin

Kotlin (JVM / Android) consumer of the `drive9-mobile-core` UniFFI bindings.

## Layout

- `lib/src/main/kotlin/com/drive9/mobile/Drive9.kt` — hand-written idiomatic
  facade. Exposes `Drive9Client` with `suspend` methods dispatched on
  `Dispatchers.IO`. Consumers should depend on this facade, not the raw
  generated bindings.
- `lib/src/main/kotlin/uniffi/drive9_mobile_core/` — UniFFI-generated bindings.
  Regenerate with `clients/drive9-mobile-core/scripts/regenerate-bindings.sh`;
  do not edit by hand.
- `lib/src/main/resources/linux-x86-64/libdrive9_mobile_core.so` — native
  library JNA loads at runtime for JVM smoke tests. **This file is gitignored
  (the repo-root `.gitignore` excludes `*.so`); a clean checkout will not have
  it.** Run the regeneration script described below before running
  `gradle test`. Production Android builds ship the per-ABI `.so` produced by
  your `cargo-ndk` pipeline; replace this resource layout with the standard
  Android `jniLibs/` directories there.

## Smoke test

A clean checkout has neither the generated Kotlin binding nor the native
library checked in, so the first run needs:

```
(cd ../drive9-mobile-core && ./scripts/regenerate-bindings.sh)
gradle test
```

The script rebuilds `libdrive9_mobile_core` in release mode, regenerates the
Kotlin binding into `lib/src/main/kotlin/uniffi/`, and drops the Linux
x86-64 `.so` into `lib/src/main/resources/linux-x86-64/`. Subsequent runs
just need `gradle test`.

Runs `Drive9Test` end-to-end through JNA → `libdrive9_mobile_core.so` →
`drive9-rs` against an in-process `HttpServer`. The same suite covers
roundtrip I/O, error mapping, and conditional-write conflict revisions.
