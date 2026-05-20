# drive9-mobile-core

UniFFI wrapper around `drive9-rs` that exposes a small, mobile-friendly subset
of the Drive9 FS API to Kotlin and Swift consumers.

## Scope (phase 1)

The wrapper deliberately stays small so the FFI surface stays auditable:

| Method | Purpose |
| --- | --- |
| `Drive9MobileClient::new(base_url, api_key)` | Build a client. Each instance owns its own multi-thread Tokio runtime. |
| `write(path, data, expected_revision?)` | Upload bytes; passing `expected_revision` makes the write conditional. |
| `read(path) -> Vec<u8>` | Download bytes. |
| `list(path) -> Vec<Drive9FileInfo>` | List a directory. |
| `stat(path) -> Drive9StatResult` | Stat a path. |
| `delete(path)` | Delete a path. |

Errors arrive as a single `Drive9Exception::Drive9` variant carrying
`code`, `status_code`, `detail`, and `server_revision`. The `code` field is one
of `http_status`, `conflict`, `request`, `json`, `io`, `other`.

Stream uploads, vault, search, copy/rename/mkdir, and SQL are intentionally
out of scope until the basic FFI shape ships and bakes.

## TLS

The crate depends on `drive9` with `rustls-tls` so Android NDK / iOS builds do
not need a system OpenSSL. Desktop builds of `drive9-rs` keep `native-tls`
through its default feature.

## Building bindings locally

```
./scripts/regenerate-bindings.sh
```

This rebuilds the release `cdylib` and refreshes the generated Kotlin / Swift
files under `clients/drive9-kotlin` and `clients/drive9-swift`. It also copies
the Linux x86-64 `libdrive9_mobile_core.so` into the Kotlin resources tree so
the JVM smoke tests can load it via JNA without extra setup.

For iOS / Android shipping builds the host project's build pipeline must
produce the appropriate static / dynamic artifacts (e.g. via `cargo-ndk` for
Android, `xcodebuild` + `lipo` for iOS XCFramework). That packaging is out of
scope for this crate.

## Tests

```
cargo test
```

Runs FFI smoke tests against an in-process mock HTTP server, exercising the
same exported types that Kotlin and Swift see.
