//! End-to-end smoke tests for the UniFFI wrapper using a mock HTTP server.
//! These tests exercise the same FFI-exposed methods that Kotlin and Swift
//! callers see, so they catch regressions in error mapping and runtime usage
//! without needing the Kotlin / Swift toolchain.

use std::collections::HashMap;

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
fn copy_rename_mkdir_succeed() {
    let mut server = mockito::Server::new();
    let _copy = server
        .mock("POST", "/v1/fs/dst.txt?copy")
        .match_header("X-Dat9-Copy-Source", "/src.txt")
        .with_status(200)
        .create();
    let _rename = server
        .mock("POST", "/v1/fs/new.txt?rename")
        .match_header("X-Dat9-Rename-Source", "/old.txt")
        .with_status(200)
        .create();
    let _mkdir = server
        .mock("POST", "/v1/fs/dir/?mkdir")
        .with_status(200)
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    client.copy("/src.txt".into(), "/dst.txt".into()).unwrap();
    client.rename("/old.txt".into(), "/new.txt".into()).unwrap();
    client.mkdir("/dir/".into()).unwrap();
}

#[test]
fn grep_returns_search_results() {
    let mut server = mockito::Server::new();
    let _m = server
        .mock("GET", "/v1/fs/?grep=needle&limit=5")
        .with_status(200)
        .with_body(
            r#"[{"path":"/a.txt","name":"a.txt","size_bytes":10,"score":0.9},{"path":"/b.txt","name":"b.txt","size_bytes":20,"score":null}]"#,
        )
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let results = client.grep("needle".into(), "/".into(), 5).unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].path, "/a.txt");
    assert_eq!(results[0].size_bytes, 10);
    assert_eq!(results[0].score, Some(0.9));
    assert_eq!(results[1].score, None);
}

#[test]
fn find_forwards_params_to_drive9_rs() {
    let mut server = mockito::Server::new();
    // drive9-rs's find() URL-encodes via urlencoding and concatenates params
    // from a HashMap, so query-parameter order is non-deterministic. Match
    // each piece independently to verify the wrapper passes them through.
    let _m = server
        .mock("GET", "/v1/fs/data/")
        .match_query(mockito::Matcher::AllOf(vec![
            mockito::Matcher::Regex("find=".to_string()),
            mockito::Matcher::Regex("type=file".to_string()),
            mockito::Matcher::Regex("limit=10".to_string()),
        ]))
        .with_status(200)
        .with_body(
            r#"[{"path":"/data/x.txt","name":"x.txt","size_bytes":1,"score":null}]"#,
        )
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let mut params = HashMap::new();
    params.insert("type".into(), "file".into());
    params.insert("limit".into(), "10".into());
    let results = client.find("/data/".into(), params).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].path, "/data/x.txt");
}

#[test]
fn find_handles_empty_params() {
    let mut server = mockito::Server::new();
    let _m = server
        .mock("GET", "/v1/fs/data/?find=")
        .with_status(200)
        .with_body("[]")
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let results = client.find("/data/".into(), HashMap::new()).unwrap();
    assert!(results.is_empty());
}

#[test]
fn sql_returns_rows_as_json_strings() {
    let mut server = mockito::Server::new();
    let _m = server
        .mock("POST", "/v1/sql")
        .with_status(200)
        .with_body(
            r#"[{"path":"/a.txt","size":10,"is_dir":false},{"path":"/b","size":0,"is_dir":true}]"#,
        )
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let rows = client.sql("SELECT path, size, is_dir FROM files".into()).unwrap();
    assert_eq!(rows.len(), 2);
    // Each row is a JSON string; key order in serde_json::Value::to_string is
    // insertion-stable on serde_json's preserve_order config — drive9-rs does
    // not enable that, so we parse and compare structurally instead of by
    // string equality.
    let row0: serde_json::Value = serde_json::from_str(&rows[0]).unwrap();
    assert_eq!(row0["path"], "/a.txt");
    assert_eq!(row0["size"], 10);
    assert_eq!(row0["is_dir"], false);
    let row1: serde_json::Value = serde_json::from_str(&rows[1]).unwrap();
    assert_eq!(row1["path"], "/b");
    assert_eq!(row1["is_dir"], true);
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
