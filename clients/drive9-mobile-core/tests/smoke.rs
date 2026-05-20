//! End-to-end smoke tests for the UniFFI wrapper using a mock HTTP server.
//! These tests exercise the same FFI-exposed methods that Kotlin and Swift
//! callers see, so they catch regressions in error mapping and runtime usage
//! without needing the Kotlin / Swift toolchain.

use drive9_mobile_core::{Drive9Exception, Drive9MobileClient};

#[test]
fn write_then_read_roundtrip() {
    let mut server = mockito::Server::new();
    let _put = server
        .mock("PUT", "/v1/fs/hello.txt")
        .with_status(200)
        .create();
    let _get = server
        .mock("GET", "/v1/fs/hello.txt")
        .with_status(200)
        .with_body("hello mobile")
        .create();

    let client = Drive9MobileClient::new(server.url(), "test-key".into());
    client
        .write("/hello.txt".into(), b"hello mobile".to_vec(), None)
        .unwrap();
    let data = client.read("/hello.txt".into()).unwrap();
    assert_eq!(data, b"hello mobile");
}

#[test]
fn list_returns_entries() {
    let mut server = mockito::Server::new();
    let _m = server
        .mock("GET", "/v1/fs/data/?list=1")
        .with_status(200)
        .with_body(
            r#"{"entries":[{"name":"a.txt","size":3,"isDir":false},{"name":"sub","size":0,"isDir":true}]}"#,
        )
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let entries = client.list("/data/".into()).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "a.txt");
    assert_eq!(entries[0].size, 3);
    assert!(!entries[0].is_dir);
    assert!(entries[1].is_dir);
}

#[test]
fn stat_reports_revision_and_size() {
    let mut server = mockito::Server::new();
    let _m = server
        .mock("HEAD", "/v1/fs/f.bin")
        .with_status(200)
        .with_header("Content-Length", "42")
        .with_header("X-Dat9-Revision", "9")
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let s = client.stat("/f.bin".into()).unwrap();
    assert_eq!(s.size, 42);
    assert_eq!(s.revision, 9);
    assert!(!s.is_dir);
}

#[test]
fn delete_succeeds() {
    let mut server = mockito::Server::new();
    let _m = server
        .mock("DELETE", "/v1/fs/gone.txt")
        .with_status(204)
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    client.delete("/gone.txt".into()).unwrap();
}

#[test]
fn conflict_preserves_server_revision() {
    let mut server = mockito::Server::new();
    let _m = server
        .mock("PUT", "/v1/fs/r.txt")
        .with_status(409)
        .with_body(r#"{"error":"revision mismatch","server_revision":12}"#)
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let err = client
        .write("/r.txt".into(), b"x".to_vec(), Some(7))
        .unwrap_err();
    let Drive9Exception::Drive9 {
        code,
        status_code,
        server_revision,
        ..
    } = err;
    assert_eq!(code, "conflict");
    assert_eq!(status_code, Some(409));
    assert_eq!(server_revision, Some(12));
}

#[test]
fn detail_field_carries_message() {
    let mut server = mockito::Server::new();
    let _m = server
        .mock("GET", "/v1/fs/x.txt")
        .with_status(500)
        .with_body(r#"{"error":"boom"}"#)
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let err = client.read("/x.txt".into()).unwrap_err();
    let Drive9Exception::Drive9 { code, detail, .. } = err;
    assert_eq!(code, "http_status");
    assert_eq!(detail, "boom");
}

#[test]
fn status_error_carries_code() {
    let mut server = mockito::Server::new();
    let _m = server
        .mock("GET", "/v1/fs/missing.txt")
        .with_status(403)
        .with_body(r#"{"error":"forbidden"}"#)
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let err = client.read("/missing.txt".into()).unwrap_err();
    let Drive9Exception::Drive9 {
        code,
        status_code,
        detail,
        ..
    } = err;
    assert_eq!(code, "http_status");
    assert_eq!(status_code, Some(403));
    assert_eq!(detail, "forbidden");
}
