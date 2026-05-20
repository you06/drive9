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
  library bundled for JVM smoke tests. Production Android builds ship the
  per-ABI `.so` produced by your `cargo-ndk` pipeline; replace this resource
  layout with the standard Android `jniLibs/` directories there.

## Smoke test

```
gradle test
```

Runs `Drive9Test` end-to-end through JNA → `libdrive9_mobile_core.so` →
`drive9-rs` against an in-process `HttpServer`. The same suite covers
roundtrip I/O, error mapping, and conditional-write conflict revisions.
