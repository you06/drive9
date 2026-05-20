//! End-to-end smoke tests for the UniFFI wrapper using a mock HTTP server.
//! These tests exercise the same FFI-exposed methods that Kotlin and Swift
//! callers see, so they catch regressions in error mapping and runtime usage
//! without needing the Kotlin / Swift toolchain.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use drive9_mobile_core::{
    Drive9CancelToken, Drive9Exception, Drive9MobileClient, Drive9ProgressListener,
};

/// Test-only listener that records each progress update. Inspect via the
/// shared `Arc<Mutex<Vec<...>>>` after the call returns.
struct RecordingListener {
    updates: Arc<Mutex<Vec<(u64, u64)>>>,
}

impl Drive9ProgressListener for RecordingListener {
    fn on_progress(&self, transferred: u64, total: u64) {
        self.updates.lock().unwrap().push((transferred, total));
    }
}

fn writable_tempfile_path(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    p.push(format!("drive9-mobile-core-test-{}-{}.bin", name, nonce));
    p
}

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
fn download_file_roundtrip_with_progress() {
    let mut server = mockito::Server::new();
    let body = vec![b'a'; 200_000];
    let _head = server
        .mock("HEAD", "/v1/fs/big.bin")
        .with_status(200)
        .with_header("Content-Length", "200000")
        .with_header("X-Dat9-Revision", "1")
        .create();
    let _get = server
        .mock("GET", "/v1/fs/big.bin")
        .with_status(200)
        .with_body(body.clone())
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let dest = writable_tempfile_path("download_file_progress");
    let updates = Arc::new(Mutex::new(Vec::<(u64, u64)>::new()));
    let listener: Arc<dyn Drive9ProgressListener> = Arc::new(RecordingListener {
        updates: Arc::clone(&updates),
    });
    client
        .download_file(
            "/big.bin".into(),
            dest.to_string_lossy().to_string(),
            Some(listener),
            None,
        )
        .unwrap();

    let written = std::fs::read(&dest).unwrap();
    assert_eq!(written, body);

    let updates = updates.lock().unwrap().clone();
    assert!(updates.len() >= 2, "want at least start + finish: {:?}", updates);
    assert_eq!(updates.first().copied(), Some((0, 200_000)));
    assert_eq!(updates.last().copied(), Some((200_000, 200_000)));
    // Monotonic non-decreasing transferred bytes.
    for w in updates.windows(2) {
        assert!(
            w[0].0 <= w[1].0,
            "progress not monotonic: {} then {}",
            w[0].0,
            w[1].0
        );
    }
    std::fs::remove_file(&dest).ok();
}

#[test]
fn download_file_unknown_total_when_stat_fails() {
    let mut server = mockito::Server::new();
    let _head = server
        .mock("HEAD", "/v1/fs/no-head.bin")
        .with_status(404)
        .create();
    let _get = server
        .mock("GET", "/v1/fs/no-head.bin")
        .with_status(200)
        .with_body("hi")
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let dest = writable_tempfile_path("download_file_unknown_total");
    let updates = Arc::new(Mutex::new(Vec::<(u64, u64)>::new()));
    let listener: Arc<dyn Drive9ProgressListener> = Arc::new(RecordingListener {
        updates: Arc::clone(&updates),
    });
    client
        .download_file(
            "/no-head.bin".into(),
            dest.to_string_lossy().to_string(),
            Some(listener),
            None,
        )
        .unwrap();

    let updates = updates.lock().unwrap().clone();
    for u in &updates {
        assert_eq!(u.1, 0, "want total=0 when stat fails: {:?}", u);
    }
    assert_eq!(std::fs::read(&dest).unwrap(), b"hi");
    std::fs::remove_file(&dest).ok();
}

#[test]
fn download_file_cancellation_deletes_partial_file() {
    // Long body so we have time to cancel mid-transfer; we cancel before
    // the read_stream future resolves by pre-cancelling the token.
    let mut server = mockito::Server::new();
    let body = vec![b'b'; 10_000];
    let _head = server
        .mock("HEAD", "/v1/fs/cancel.bin")
        .with_status(200)
        .with_header("Content-Length", "10000")
        .create();
    let _get = server
        .mock("GET", "/v1/fs/cancel.bin")
        .with_status(200)
        .with_body(body)
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let dest = writable_tempfile_path("download_file_cancel");
    let token = Drive9CancelToken::new();
    token.cancel();

    let err = client
        .download_file(
            "/cancel.bin".into(),
            dest.to_string_lossy().to_string(),
            None,
            Some(token),
        )
        .unwrap_err();
    let Drive9Exception::Drive9 { code, .. } = err;
    assert_eq!(code, "cancelled");
    assert!(
        !dest.exists(),
        "partial file should be cleaned up on cancellation"
    );
}

#[test]
fn download_file_cancel_idempotent() {
    let token = Drive9CancelToken::new();
    assert!(!token.is_cancelled());
    token.cancel();
    token.cancel();
    token.cancel();
    assert!(token.is_cancelled());
}

#[test]
fn download_file_no_cancel_token_works() {
    let mut server = mockito::Server::new();
    let _head = server
        .mock("HEAD", "/v1/fs/plain.bin")
        .with_status(200)
        .with_header("Content-Length", "5")
        .create();
    let _get = server
        .mock("GET", "/v1/fs/plain.bin")
        .with_status(200)
        .with_body("plain")
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let dest = writable_tempfile_path("download_file_no_cancel");
    client
        .download_file("/plain.bin".into(), dest.to_string_lossy().to_string(), None, None)
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), b"plain");
    std::fs::remove_file(&dest).ok();
}

#[test]
fn patch_file_parts_validates_inputs() {
    let server = mockito::Server::new();
    let client = Drive9MobileClient::new(server.url(), "k".into());
    let local = writable_tempfile_path("patch_validate");
    std::fs::write(&local, b"xx").unwrap();
    let local_str = local.to_string_lossy().to_string();

    // new_size < 0
    let err = client
        .patch_file_parts(local_str.clone(), "/r".into(), vec![1], -1, None, None)
        .unwrap_err();
    let Drive9Exception::Drive9 { code, detail, .. } = err;
    assert_eq!(code, "other");
    assert!(detail.contains("new_size"), "want new_size error: {}", detail);

    // part_size <= 0
    let err = client
        .patch_file_parts(local_str.clone(), "/r".into(), vec![1], 100, Some(0), None)
        .unwrap_err();
    let Drive9Exception::Drive9 { code, detail, .. } = err;
    assert_eq!(code, "other");
    assert!(detail.contains("part_size"), "want part_size error: {}", detail);

    // dirty_parts contains 0
    let err = client
        .patch_file_parts(local_str.clone(), "/r".into(), vec![0, 1], 100, None, None)
        .unwrap_err();
    let Drive9Exception::Drive9 { code, detail, .. } = err;
    assert_eq!(code, "other");
    assert!(detail.contains("dirty_parts"), "want dirty_parts error: {}", detail);

    std::fs::remove_file(&local).ok();
}

#[test]
fn patch_file_parts_short_read_errors() {
    // Set up a server that responds to PATCH with a multipart plan asking
    // for a part of size 100; the local file only has 50 bytes. Closure
    // must error instead of uploading short.
    let mut server = mockito::Server::new();
    let _patch = server
        .mock("PATCH", "/v1/fs/r.bin")
        .with_status(200)
        .with_body(
            r#"{"upload_id":"u1","part_size":100,"upload_parts":[{"number":1,"url":"http://placeholder/part1","size":100}],"copied_parts":[]}"#,
        )
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let local = writable_tempfile_path("patch_short_read");
    std::fs::write(&local, vec![b'x'; 50]).unwrap();

    let err = client
        .patch_file_parts(
            local.to_string_lossy().to_string(),
            "/r.bin".into(),
            vec![1],
            100,
            Some(100),
            None,
        )
        .unwrap_err();
    let Drive9Exception::Drive9 { code, detail, .. } = err;
    // Drive9Error::Other for "short read for part ..." surfaces as code="other".
    // drive9-rs's task join may surface this as Other too.
    assert!(
        detail.contains("short read") || detail.contains("part 1"),
        "want short-read message, got: {}",
        detail
    );
    assert_eq!(code, "other", "expected other code, got: {}", code);
    std::fs::remove_file(&local).ok();
}

// Avoid the AtomicU64-unused warning when this module is imported elsewhere.
#[allow(dead_code)]
fn _atomic_u64_witness() -> AtomicU64 {
    AtomicU64::new(0)
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
