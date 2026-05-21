//! End-to-end smoke tests for the UniFFI wrapper using a mock HTTP server.
//! These tests exercise the same FFI-exposed methods that Kotlin and Swift
//! callers see, so they catch regressions in error mapping and runtime usage
//! without needing the Kotlin / Swift toolchain.

use std::collections::HashMap;
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
        .patch_file_parts(local_str.clone(), "/r".into(), vec![1], -1, 100, None)
        .unwrap_err();
    let Drive9Exception::Drive9 { code, detail, .. } = err;
    assert_eq!(code, "other");
    assert!(detail.contains("new_size"), "want new_size error: {}", detail);

    // part_size <= 0
    let err = client
        .patch_file_parts(local_str.clone(), "/r".into(), vec![1], 100, 0, None)
        .unwrap_err();
    let Drive9Exception::Drive9 { code, detail, .. } = err;
    assert_eq!(code, "other");
    assert!(detail.contains("part_size"), "want part_size error: {}", detail);

    // dirty_parts contains 0
    let err = client
        .patch_file_parts(local_str.clone(), "/r".into(), vec![0, 1], 100, 100, None)
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
            100,
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

#[test]
fn patch_file_parts_uses_global_part_size_for_offset() {
    // local file is 150 bytes: bytes 0..100 are 'a', 100..150 are 'b'.
    // Caller picked part_size=100 → part 1 covers offset 0..100, part 2
    // covers offset 100..150 (size 50).
    //
    // Server plan returns upload_part {number:2,size:50}; the closure
    // MUST seek to offset 100 (using global part_size) and read 50 bytes
    // of 'b', not offset 50 (which would be 'a' bytes).
    let mut server = mockito::Server::new();
    let upload_url = format!("{}/upload/part-2", server.url());
    let _patch = server
        .mock("PATCH", "/v1/fs/r.bin")
        .with_status(200)
        .with_body(format!(
            r#"{{"upload_id":"u1","part_size":100,"upload_parts":[{{"number":2,"url":"{}","size":50}}],"copied_parts":[1]}}"#,
            upload_url
        ))
        .create();

    let received_body = Arc::new(Mutex::new(Vec::<u8>::new()));
    let received_body_clone = Arc::clone(&received_body);
    let _put = server
        .mock("PUT", "/upload/part-2")
        .with_status(200)
        .with_header("ETag", "etag-2")
        .match_request(move |req| {
            if let Ok(body) = req.body() {
                *received_body_clone.lock().unwrap() = body.to_vec();
            }
            true
        })
        .create();
    let _complete = server
        .mock("POST", "/v1/uploads/u1/complete")
        .with_status(200)
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let local = writable_tempfile_path("patch_offset");
    let mut body = vec![b'a'; 100];
    body.extend(std::iter::repeat(b'b').take(50));
    std::fs::write(&local, &body).unwrap();

    client
        .patch_file_parts(
            local.to_string_lossy().to_string(),
            "/r.bin".into(),
            vec![2],
            150,
            100,
            None,
        )
        .unwrap();

    let received = received_body.lock().unwrap().clone();
    assert_eq!(received.len(), 50, "expected 50 bytes, got {}", received.len());
    assert!(
        received.iter().all(|&b| b == b'b'),
        "expected all 'b' bytes (offset 100..150), got: {:?}",
        &received[..received.len().min(10)]
    );
    std::fs::remove_file(&local).ok();
}

#[test]
fn download_file_preserves_preexisting_destination_on_failure() {
    // The destination already has user content. A failed/cancelled download
    // must not touch it: write to a sibling temp file, rename on success,
    // remove temp on failure.
    let mut server = mockito::Server::new();
    let _head = server
        .mock("HEAD", "/v1/fs/cancel.bin")
        .with_status(200)
        .with_header("Content-Length", "10000")
        .create();
    let _get = server
        .mock("GET", "/v1/fs/cancel.bin")
        .with_status(200)
        .with_body(vec![b'b'; 10_000])
        .create();

    let dest = writable_tempfile_path("download_preserve");
    let original = b"do not overwrite me".to_vec();
    std::fs::write(&dest, &original).unwrap();

    let client = Drive9MobileClient::new(server.url(), "k".into());
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
    let after = std::fs::read(&dest).unwrap();
    assert_eq!(
        after, original,
        "pre-existing destination must be unchanged on failure"
    );

    // Sanity: no leftover temp file in the parent directory.
    let parent = dest.parent().unwrap();
    let file_name = dest.file_name().unwrap().to_string_lossy().to_string();
    let leftover = std::fs::read_dir(parent)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .find(|n| n.starts_with(&format!(".{}.drive9-tmp-", file_name)));
    assert!(
        leftover.is_none(),
        "leftover temp file after cancel: {:?}",
        leftover
    );
    std::fs::remove_file(&dest).ok();
}

#[test]
fn upload_file_small_roundtrip_with_progress() {
    let mut server = mockito::Server::new();
    let _put = server
        .mock("PUT", "/v1/fs/up.bin")
        .with_status(200)
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let local = writable_tempfile_path("upload_small");
    std::fs::write(&local, vec![b'x'; 100]).unwrap();

    let updates = Arc::new(Mutex::new(Vec::<(u64, u64)>::new()));
    let listener: Arc<dyn Drive9ProgressListener> = Arc::new(RecordingListener {
        updates: Arc::clone(&updates),
    });
    client
        .upload_file(
            local.to_string_lossy().to_string(),
            "/up.bin".into(),
            None,
            Some(listener),
            None,
        )
        .unwrap();

    let updates = updates.lock().unwrap().clone();
    assert_eq!(updates, vec![(0, 100), (100, 100)]);
    std::fs::remove_file(&local).ok();
}

#[test]
fn upload_file_cancel_before_returns_cancelled_without_request() {
    let mut server = mockito::Server::new();
    let put_mock = server
        .mock("PUT", "/v1/fs/up-cancel.bin")
        .with_status(200)
        .expect(0)
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let local = writable_tempfile_path("upload_cancel_before");
    std::fs::write(&local, vec![b'x'; 100]).unwrap();

    let token = Drive9CancelToken::new();
    token.cancel();
    let updates = Arc::new(Mutex::new(Vec::<(u64, u64)>::new()));
    let listener: Arc<dyn Drive9ProgressListener> = Arc::new(RecordingListener {
        updates: Arc::clone(&updates),
    });
    let err = client
        .upload_file(
            local.to_string_lossy().to_string(),
            "/up-cancel.bin".into(),
            None,
            Some(listener),
            Some(token),
        )
        .unwrap_err();
    let Drive9Exception::Drive9 { code, detail, .. } = err;
    assert_eq!(code, "cancelled");
    assert!(detail.contains("cancel"), "want cancel in detail: {}", detail);

    let updates = updates.lock().unwrap().clone();
    assert_eq!(updates, vec![], "no progress should fire when cancel is pre-set");
    put_mock.assert();
    std::fs::remove_file(&local).ok();
}

#[test]
fn upload_file_conditional_conflict_preserves_server_revision() {
    let mut server = mockito::Server::new();
    let _put = server
        .mock("PUT", "/v1/fs/up-rev.bin")
        .match_header("X-Dat9-Expected-Revision", "5")
        .with_status(409)
        .with_body(r#"{"error":"revision mismatch","server_revision":12}"#)
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let local = writable_tempfile_path("upload_conflict");
    std::fs::write(&local, vec![b'a'; 50]).unwrap();

    let err = client
        .upload_file(
            local.to_string_lossy().to_string(),
            "/up-rev.bin".into(),
            Some(5),
            None,
            None,
        )
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
    std::fs::remove_file(&local).ok();
}

#[test]
fn vault_list_readable_secrets_happy_path() {
    let mut server = mockito::Server::new();
    let _m = server
        .mock("GET", "/v1/vault/read")
        .with_status(200)
        .with_body(r#"{"secrets":["alpha","beta"]}"#)
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let names = client.vault_list_readable_secrets().unwrap();
    assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
}

#[test]
fn vault_read_secret_field_returns_value_verbatim() {
    let mut server = mockito::Server::new();
    let _m = server
        .mock("GET", "/v1/vault/read/api-keys/openai")
        .with_status(200)
        .with_body("sk-very-secret-value")
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let value = client
        .vault_read_secret_field("api-keys".into(), "openai".into())
        .unwrap();
    assert_eq!(value, "sk-very-secret-value");
}

#[test]
fn vault_read_secret_field_passes_json_looking_string_through_untouched() {
    let mut server = mockito::Server::new();
    // The server stored a JSON-encoded string in the field. The wrapper
    // must not parse, re-encode, or strip whitespace — return verbatim.
    let raw = r#"{"k":1,"nested":{"flag":true}}"#;
    let _m = server
        .mock("GET", "/v1/vault/read/dest-config/payload")
        .with_status(200)
        .with_body(raw)
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let value = client
        .vault_read_secret_field("dest-config".into(), "payload".into())
        .unwrap();
    assert_eq!(value, raw, "wrapper must not interpret JSON-looking strings");
}

#[test]
fn vault_list_unauthorized_surfaces_as_http_status() {
    let mut server = mockito::Server::new();
    let _m = server
        .mock("GET", "/v1/vault/read")
        .with_status(401)
        .with_body(r#"{"error":"token expired"}"#)
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let err = client.vault_list_readable_secrets().unwrap_err();
    let Drive9Exception::Drive9 {
        code,
        status_code,
        detail,
        ..
    } = err;
    assert_eq!(code, "http_status");
    assert_eq!(status_code, Some(401));
    assert_eq!(detail, "token expired");
}

#[test]
fn vault_read_field_forbidden_surfaces_as_http_status() {
    let mut server = mockito::Server::new();
    let _m = server
        .mock("GET", "/v1/vault/read/private/value")
        .with_status(403)
        .with_body(r#"{"error":"out of scope"}"#)
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let err = client
        .vault_read_secret_field("private".into(), "value".into())
        .unwrap_err();
    let Drive9Exception::Drive9 {
        code, status_code, ..
    } = err;
    assert_eq!(code, "http_status");
    assert_eq!(status_code, Some(403));
}

#[test]
fn vault_read_missing_secret_surfaces_as_http_status_404() {
    let mut server = mockito::Server::new();
    let _m = server
        .mock("GET", "/v1/vault/read/no-such/field")
        .with_status(404)
        .with_body(r#"{"error":"secret not found"}"#)
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let err = client
        .vault_read_secret_field("no-such".into(), "field".into())
        .unwrap_err();
    let Drive9Exception::Drive9 {
        code, status_code, ..
    } = err;
    assert_eq!(code, "http_status");
    assert_eq!(status_code, Some(404));
}

#[test]
fn vault_read_secret_field_url_encodes_special_chars_via_drive9_rs() {
    // Confirm drive9-rs's existing urlencoding is exercised end-to-end:
    // a name with a slash gets percent-encoded as %2F. The wrapper does
    // not re-encode.
    let mut server = mockito::Server::new();
    let _m = server
        .mock("GET", "/v1/vault/read/team%2Fbackend/api_key")
        .with_status(200)
        .with_body("encoded-value")
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let value = client
        .vault_read_secret_field("team/backend".into(), "api_key".into())
        .unwrap();
    assert_eq!(value, "encoded-value");
}

#[test]
fn stream_upload_happy_path_two_parts() {
    let mut server = mockito::Server::new();
    let upload_id = "u-stream";
    let part_size: i64 = 100;
    let _init = server
        .mock("POST", "/v2/uploads/initiate")
        .with_status(200)
        .with_body(format!(
            r#"{{"upload_id":"{}","key":"k","part_size":{},"total_parts":2}}"#,
            upload_id, part_size
        ))
        .create();

    let base = server.url();
    let _presign = server
        .mock(
            "POST",
            format!("/v2/uploads/{}/presign", upload_id).as_str(),
        )
        .with_status(200)
        .with_body_from_request(move |req| {
            let body: serde_json::Value =
                serde_json::from_slice(req.body().unwrap()).unwrap();
            let n = body["part_number"].as_i64().unwrap() as i32;
            serde_json::to_vec(&serde_json::json!({
                "number": n,
                "url": format!("{}/upload/{}", base, n),
                "size": part_size,
            }))
            .unwrap()
        })
        .expect_at_least(1)
        .create();

    let _put = server
        .mock("PUT", mockito::Matcher::Regex(r"^/upload/\d+$".to_string()))
        .with_status(200)
        .with_header("etag", "e")
        .expect(2)
        .create();

    let complete = server
        .mock(
            "POST",
            format!("/v2/uploads/{}/complete", upload_id).as_str(),
        )
        .with_status(200)
        .expect(1)
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let upload = client.new_stream_upload("/big.bin".into(), part_size * 2, None);
    upload
        .write_part(1, vec![b'a'; part_size as usize])
        .unwrap();
    upload
        .complete(2, vec![b'a'; part_size as usize])
        .unwrap();
    complete.assert();

    // After complete, the object is terminal.
    let err = upload
        .write_part(3, vec![b'a'; part_size as usize])
        .unwrap_err();
    let Drive9Exception::Drive9 { code, detail, .. } = err;
    assert_eq!(code, "other");
    assert!(detail.contains("completed"), "want completed reason: {}", detail);
}

#[test]
fn stream_upload_abort_after_write_calls_server_abort() {
    let mut server = mockito::Server::new();
    let upload_id = "u-stream-abort";
    let part_size: i64 = 100;
    let _init = server
        .mock("POST", "/v2/uploads/initiate")
        .with_status(200)
        .with_body(format!(
            r#"{{"upload_id":"{}","key":"k","part_size":{},"total_parts":1}}"#,
            upload_id, part_size
        ))
        .create();
    let base = server.url();
    let _presign = server
        .mock(
            "POST",
            format!("/v2/uploads/{}/presign", upload_id).as_str(),
        )
        .with_status(200)
        .with_body_from_request(move |req| {
            let body: serde_json::Value =
                serde_json::from_slice(req.body().unwrap()).unwrap();
            let n = body["part_number"].as_i64().unwrap() as i32;
            serde_json::to_vec(&serde_json::json!({
                "number": n,
                "url": format!("{}/upload/{}", base, n),
                "size": part_size,
            }))
            .unwrap()
        })
        .create();
    let _put = server
        .mock("PUT", "/upload/1")
        .with_status(200)
        .with_header("etag", "e")
        .create();
    let abort_mock = server
        .mock(
            "POST",
            format!("/v2/uploads/{}/abort", upload_id).as_str(),
        )
        .with_status(200)
        .expect(1)
        .create();
    let complete_mock = server
        .mock(
            "POST",
            format!("/v2/uploads/{}/complete", upload_id).as_str(),
        )
        .expect(0)
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let upload = client.new_stream_upload("/abrt.bin".into(), part_size, None);
    upload
        .write_part(1, vec![b'a'; part_size as usize])
        .unwrap();
    upload.abort().unwrap();
    // Idempotent: second call returns Ok without re-hitting the server.
    upload.abort().unwrap();
    abort_mock.assert();
    complete_mock.assert();

    let err = upload
        .write_part(2, vec![b'a'; part_size as usize])
        .unwrap_err();
    let Drive9Exception::Drive9 { code, detail, .. } = err;
    assert_eq!(code, "other");
    assert!(detail.contains("aborted"), "want aborted reason: {}", detail);
}

#[test]
fn stream_upload_part_size_total_parts_initiate_once() {
    // Phase 4C: part_size() / total_parts() lazily call /v2/uploads/initiate
    // on first use and cache the result; repeated calls hit the cache.
    let mut server = mockito::Server::new();
    let upload_id = "u-accessors";
    let part_size: i64 = 12_345;
    let total_parts: i32 = 7;
    let init_mock = server
        .mock("POST", "/v2/uploads/initiate")
        .with_status(200)
        .with_body(format!(
            r#"{{"upload_id":"{}","key":"k","part_size":{},"total_parts":{}}}"#,
            upload_id, part_size, total_parts
        ))
        .expect(1)
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let upload = client.new_stream_upload(
        "/info.bin".into(),
        part_size * total_parts as i64,
        None,
    );
    assert_eq!(upload.part_size().unwrap(), part_size);
    assert_eq!(upload.total_parts().unwrap(), total_parts);
    assert_eq!(upload.part_size().unwrap(), part_size);
    assert_eq!(upload.total_parts().unwrap(), total_parts);
    init_mock.assert();
}

#[test]
fn stream_upload_parameter_error_keeps_upload_active() {
    // Phase 4A review (Kaltsit): an invalid part_num (e.g. 0) is a
    // caller bug, not a stream failure. The wrapper must NOT poison
    // the upload to Errored after a parameter-error write_part —
    // subsequent legitimate write_part + complete must still succeed.
    let mut server = mockito::Server::new();
    let upload_id = "u-stream-param";
    let part_size: i64 = 100;
    let _init = server
        .mock("POST", "/v2/uploads/initiate")
        .with_status(200)
        .with_body(format!(
            r#"{{"upload_id":"{}","key":"k","part_size":{},"total_parts":1}}"#,
            upload_id, part_size
        ))
        .create();
    let base = server.url();
    let _presign = server
        .mock(
            "POST",
            format!("/v2/uploads/{}/presign", upload_id).as_str(),
        )
        .with_status(200)
        .with_body_from_request(move |req| {
            let body: serde_json::Value =
                serde_json::from_slice(req.body().unwrap()).unwrap();
            let n = body["part_number"].as_i64().unwrap() as i32;
            serde_json::to_vec(&serde_json::json!({
                "number": n,
                "url": format!("{}/upload/{}", base, n),
                "size": part_size,
            }))
            .unwrap()
        })
        .create();
    let put_mock = server
        .mock("PUT", "/upload/1")
        .with_status(200)
        .with_header("etag", "e")
        .expect(1)
        .create();
    let complete_mock = server
        .mock(
            "POST",
            format!("/v2/uploads/{}/complete", upload_id).as_str(),
        )
        .with_status(200)
        .expect(1)
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let upload = client.new_stream_upload("/param.bin".into(), part_size, None);

    // Parameter error: part_num must be >= 1.
    let err = upload
        .write_part(0, vec![b'a'; part_size as usize])
        .unwrap_err();
    let Drive9Exception::Drive9 { detail, .. } = err;
    assert!(
        detail.contains("part number must be"),
        "want parameter error detail, got: {}",
        detail
    );

    // Upload must NOT be poisoned — legitimate write + complete still
    // succeed.
    upload
        .write_part(1, vec![b'a'; part_size as usize])
        .unwrap();
    upload.complete(1, Vec::new()).unwrap();
    put_mock.assert();
    complete_mock.assert();
}

#[test]
fn stream_upload_complete_alone_works_for_one_part_stream() {
    // Phase 4A review: a single-part upload should be expressible as
    // new_stream_upload + complete(1, data), without a prior
    // write_part. drive9-rs::StreamWriter::complete initiates the v2
    // upload internally in this case.
    let mut server = mockito::Server::new();
    let upload_id = "u-stream-one-shot";
    let part_size: i64 = 100;
    let _init = server
        .mock("POST", "/v2/uploads/initiate")
        .with_status(200)
        .with_body(format!(
            r#"{{"upload_id":"{}","key":"k","part_size":{},"total_parts":1}}"#,
            upload_id, part_size
        ))
        .expect(1)
        .create();
    let base = server.url();
    let _presign = server
        .mock(
            "POST",
            format!("/v2/uploads/{}/presign", upload_id).as_str(),
        )
        .with_status(200)
        .with_body_from_request(move |req| {
            let body: serde_json::Value =
                serde_json::from_slice(req.body().unwrap()).unwrap();
            let n = body["part_number"].as_i64().unwrap() as i32;
            serde_json::to_vec(&serde_json::json!({
                "number": n,
                "url": format!("{}/upload/{}", base, n),
                "size": part_size,
            }))
            .unwrap()
        })
        .create();
    let put_mock = server
        .mock("PUT", "/upload/1")
        .with_status(200)
        .with_header("etag", "e")
        .expect(1)
        .create();
    let complete_mock = server
        .mock(
            "POST",
            format!("/v2/uploads/{}/complete", upload_id).as_str(),
        )
        .with_status(200)
        .expect(1)
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let upload = client.new_stream_upload("/one-shot.bin".into(), part_size, None);
    upload
        .complete(1, vec![b'a'; part_size as usize])
        .unwrap();
    put_mock.assert();
    complete_mock.assert();
}

#[test]
fn stream_upload_part_error_transitions_to_errored() {
    let mut server = mockito::Server::new();
    let upload_id = "u-stream-err";
    let part_size: i64 = 100;
    let _init = server
        .mock("POST", "/v2/uploads/initiate")
        .with_status(200)
        .with_body(format!(
            r#"{{"upload_id":"{}","key":"k","part_size":{},"total_parts":1}}"#,
            upload_id, part_size
        ))
        .create();
    let base = server.url();
    let _presign = server
        .mock(
            "POST",
            format!("/v2/uploads/{}/presign", upload_id).as_str(),
        )
        .with_status(200)
        .with_body_from_request(move |req| {
            let body: serde_json::Value =
                serde_json::from_slice(req.body().unwrap()).unwrap();
            let n = body["part_number"].as_i64().unwrap() as i32;
            serde_json::to_vec(&serde_json::json!({
                "number": n,
                "url": format!("{}/upload/{}", base, n),
                "size": part_size,
            }))
            .unwrap()
        })
        .create();
    // First part PUT fails 500.
    let _put = server
        .mock("PUT", "/upload/1")
        .with_status(500)
        .with_body(r#"{"error":"boom"}"#)
        .create();
    // Abort must still be callable for cleanup.
    let abort_mock = server
        .mock(
            "POST",
            format!("/v2/uploads/{}/abort", upload_id).as_str(),
        )
        .with_status(200)
        .expect(1)
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let upload = client.new_stream_upload("/err.bin".into(), part_size, None);
    upload
        .write_part(1, vec![b'a'; part_size as usize])
        .unwrap();
    // Give the spawned PUT a chance to fail and set state.err. The
    // next write_part observes the background error and rejects.
    std::thread::sleep(std::time::Duration::from_millis(100));
    let err = upload
        .write_part(2, vec![b'a'; part_size as usize])
        .unwrap_err();
    let Drive9Exception::Drive9 { code, .. } = err;
    assert_eq!(code, "other");

    // complete() must also reject in Errored state.
    let complete_err = upload
        .complete(2, vec![b'a'; part_size as usize])
        .unwrap_err();
    let Drive9Exception::Drive9 { code, detail, .. } = complete_err;
    assert_eq!(code, "other");
    assert!(
        detail.contains("errored") || detail.contains("background"),
        "want errored / background reason: {}",
        detail
    );

    // abort() is still allowed for server-side cleanup.
    upload.abort().unwrap();
    abort_mock.assert();
}

#[test]
fn stream_download_happy_path_multiple_chunks() {
    let mut server = mockito::Server::new();
    let body: Vec<u8> = (0..200_000).map(|i| (i % 251) as u8).collect();
    let _get = server
        .mock("GET", "/v1/fs/big.bin")
        .with_status(200)
        .with_body(body.clone())
        .create();

    let client = Drive9MobileClient::new(server.url(), "k".into());
    let dl = client
        .new_stream_download("/big.bin".into(), None)
        .unwrap();
    let mut out = Vec::new();
    loop {
        match dl.read_chunk().unwrap() {
            Some(chunk) => out.extend_from_slice(&chunk),
            None => break,
        }
    }
    assert_eq!(out, body);
    // After EOF, subsequent read returns None too.
    assert!(dl.read_chunk().unwrap().is_none());
    dl.close_stream();
    // close after EOF is a no-op; further read_chunk should still
    // return None (terminal state stays EndOfStream).
}

#[test]
fn stream_download_close_blocks_further_reads() {
    let mut server = mockito::Server::new();
    let body = vec![b'a'; 200_000];
    let _get = server
        .mock("GET", "/v1/fs/closeme.bin")
        .with_status(200)
        .with_body(body)
        .create();
    let client = Drive9MobileClient::new(server.url(), "k".into());
    let dl = client
        .new_stream_download("/closeme.bin".into(), None)
        .unwrap();
    // First chunk lands fine.
    assert!(dl.read_chunk().unwrap().is_some());
    dl.close_stream();
    // close is idempotent.
    dl.close_stream();
    let err = dl.read_chunk().unwrap_err();
    let Drive9Exception::Drive9 { code, detail, .. } = err;
    assert_eq!(code, "other");
    assert!(
        detail.contains("closed"),
        "want closed reason: {}",
        detail
    );
}

#[test]
fn stream_download_replays_error() {
    let mut server = mockito::Server::new();
    let _get = server
        .mock("GET", "/v1/fs/nope.bin")
        .with_status(404)
        .with_body(r#"{"error":"not found"}"#)
        .create();
    let client = Drive9MobileClient::new(server.url(), "k".into());
    // The 404 surfaces at new_stream_download (read_stream issues the
    // GET and check_error returns Err). Verify the constructor path.
    let err = match client.new_stream_download("/nope.bin".into(), None) {
        Err(e) => e,
        Ok(_) => panic!("expected error from new_stream_download"),
    };
    let Drive9Exception::Drive9 {
        code, status_code, ..
    } = err;
    assert_eq!(code, "http_status");
    assert_eq!(status_code, Some(404));
}

#[test]
fn stream_download_concurrent_read_chunk_marks_terminal_error() {
    // Two threads racing on read_chunk: one takes the reader and parks
    // on the socket (TcpListener hangs after headers); the other
    // observes a None reader and must transition the object to a
    // terminal Errored state — subsequent calls then replay the same
    // "concurrent read_chunk is not supported" error rather than
    // succeeding the moment the first reader puts it back.
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server_done = Arc::new(AtomicBool::new(false));
    let server_done_t = Arc::clone(&server_done);
    let server_thread = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 8192];
        let mut total = Vec::new();
        loop {
            let n = stream.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            total.extend_from_slice(&buf[..n]);
            if total.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let _ = stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\nConnection: close\r\n\r\n",
        );
        let _ = stream.flush();
        while !server_done_t.load(AtomicOrdering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });

    let base_url = format!("http://127.0.0.1:{}", port);
    let client = Drive9MobileClient::new(base_url, "k".into());
    let token = Drive9CancelToken::new();
    let dl = client
        .new_stream_download("/race.bin".into(), Some(token.clone()))
        .unwrap();

    // Thread A: starts read_chunk and parks on the socket.
    let dl_a = dl.clone();
    let a_thread = std::thread::spawn(move || dl_a.read_chunk());

    // Let A enter read_chunk (so it has the reader taken out).
    std::thread::sleep(std::time::Duration::from_millis(80));

    // Thread B: from the main thread, fire read_chunk. Should see
    // reader=None with state still Open -> terminal Errored.
    let err_b = match dl.read_chunk() {
        Ok(v) => panic!("expected concurrent error; got Ok({:?})", v.as_ref().map(|d| d.len())),
        Err(e) => e,
    };
    let Drive9Exception::Drive9 { code, detail, .. } = err_b;
    assert_eq!(code, "other");
    assert!(
        detail.contains("concurrent"),
        "want concurrent reason: {}",
        detail
    );

    // Subsequent read_chunk replays the same terminal error.
    let err_c = match dl.read_chunk() {
        Ok(v) => panic!("expected replayed concurrent error; got Ok({:?})", v.as_ref().map(|d| d.len())),
        Err(e) => e,
    };
    let Drive9Exception::Drive9 { code, detail, .. } = err_c;
    assert_eq!(code, "other");
    assert!(
        detail.contains("concurrent"),
        "want concurrent reason on replay: {}",
        detail
    );

    // Unblock A so the test exits cleanly.
    token.cancel();
    let _ = a_thread.join();
    server_done.store(true, AtomicOrdering::SeqCst);
    let _ = server_thread.join();
}

#[test]
fn stream_download_cancel_during_read_returns_cancelled() {
    // We need the SAME blocking read_chunk to be woken by token.cancel(),
    // not just for "next read returns cancelled". Mockito buffers the
    // whole response body before sending, so it can't reliably park
    // read_chunk mid-stream. Use a raw TcpListener that sends 200 OK
    // headers plus a single small chunk, then hangs — that lands one
    // chunk at the client and parks subsequent reads waiting on more
    // body bytes.
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server_done = Arc::new(AtomicBool::new(false));
    let server_done_t = Arc::clone(&server_done);

    let server_thread = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 8192];
        // Drain the HTTP request headers (until \r\n\r\n).
        let mut total = Vec::new();
        loop {
            let n = stream.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            total.extend_from_slice(&buf[..n]);
            if total.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        // Send 200 OK with a Content-Length larger than what we'll
        // actually deliver, then send one small chunk and hang. This
        // forces read_chunk to park waiting for the rest of the body.
        let _ = stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\nConnection: close\r\n\r\n",
        );
        let _ = stream.write_all(&vec![b'b'; 1024]);
        let _ = stream.flush();
        // Hang until the test marks us done (after the cancel).
        while !server_done_t.load(AtomicOrdering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });

    let base_url = format!("http://127.0.0.1:{}", port);
    let client = Drive9MobileClient::new(base_url, "k".into());
    let token = Drive9CancelToken::new();
    let dl = client
        .new_stream_download("/slow.bin".into(), Some(token.clone()))
        .unwrap();

    // First chunk should arrive fine (server already wrote 1024 bytes).
    let first = dl.read_chunk().unwrap();
    assert!(first.is_some(), "first chunk should arrive before cancel");

    // Spawn a second read_chunk; it will park waiting for more body.
    let dl_for_thread = dl.clone();
    let read_thread = std::thread::spawn(move || {
        let start = std::time::Instant::now();
        let result = dl_for_thread.read_chunk();
        (start.elapsed(), result)
    });

    // Give read_chunk a moment to park, then cancel.
    std::thread::sleep(std::time::Duration::from_millis(100));
    token.cancel();

    let (elapsed, result) = read_thread.join().unwrap();
    let err = match result {
        Err(e) => e,
        Ok(v) => panic!(
            "expected cancelled error from mid-read cancel; got Ok({:?})",
            v.as_ref().map(|d| d.len())
        ),
    };
    let Drive9Exception::Drive9 { code, .. } = err;
    assert_eq!(code, "cancelled");
    // The cancel must wake the SAME read call, not just the next one.
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "expected mid-read cancel within 500ms; got {:?}",
        elapsed
    );

    // Subsequent read_chunk replays cancelled.
    let err2 = match dl.read_chunk() {
        Err(e) => e,
        Ok(v) => panic!("expected cancelled replay; got Ok({:?})", v),
    };
    let Drive9Exception::Drive9 { code, .. } = err2;
    assert_eq!(code, "cancelled");

    // Let the server thread finish cleanly.
    server_done.store(true, AtomicOrdering::SeqCst);
    let _ = server_thread.join();
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
