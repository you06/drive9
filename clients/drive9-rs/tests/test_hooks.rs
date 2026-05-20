//! Tests for the upload-stream hooks API
//! (`Client::write_stream_with_hooks`, `UploadProgress`, `CancelSignal`,
//! `Drive9Error::Cancelled`).

use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use drive9::transfer::{CancelSignal, SeekableReader, UploadProgress};
use drive9::{Client, Drive9Error};

#[derive(Default)]
struct RecordingProgress {
    updates: Mutex<Vec<(u64, u64)>>,
}

impl UploadProgress for RecordingProgress {
    fn on_progress(&self, transferred: u64, total: u64) {
        self.updates.lock().unwrap().push((transferred, total));
    }
}

#[derive(Default)]
struct FlagCancel {
    flag: AtomicBool,
}

impl FlagCancel {
    fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }
}

impl CancelSignal for FlagCancel {
    fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

#[tokio::test]
async fn small_file_emits_start_and_finish_progress() {
    let mut server = mockito::Server::new_async().await;
    let _put = server
        .mock("PUT", "/v1/fs/small.bin")
        .with_status(200)
        .create_async()
        .await;

    let client = Client::new(server.url(), "k");
    let progress = Arc::new(RecordingProgress::default());
    let reader: Box<dyn SeekableReader> = Box::new(Cursor::new(vec![b'a'; 100]));
    client
        .write_stream_with_hooks(
            "/small.bin",
            reader,
            100,
            -1,
            Some(progress.clone() as Arc<dyn UploadProgress>),
            None,
        )
        .await
        .unwrap();

    let updates = progress.updates.lock().unwrap().clone();
    assert_eq!(updates, vec![(0, 100), (100, 100)]);
}

#[tokio::test]
async fn small_file_cancel_before_put_returns_cancelled_without_request() {
    let mut server = mockito::Server::new_async().await;
    // If the wrapper accidentally PUTs anyway, this mock will hand back 200
    // and the test will fail because we'll observe a successful upload.
    let put_mock = server
        .mock("PUT", "/v1/fs/cancel-small.bin")
        .with_status(200)
        .expect(0)
        .create_async()
        .await;

    let client = Client::new(server.url(), "k");
    let cancel = Arc::new(FlagCancel::default());
    cancel.cancel();
    let progress = Arc::new(RecordingProgress::default());
    let reader: Box<dyn SeekableReader> = Box::new(Cursor::new(vec![b'a'; 100]));
    let err = client
        .write_stream_with_hooks(
            "/cancel-small.bin",
            reader,
            100,
            -1,
            Some(progress.clone() as Arc<dyn UploadProgress>),
            Some(cancel.clone() as Arc<dyn CancelSignal>),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Drive9Error::Cancelled), "got: {:?}", err);

    // No progress event of any kind should have fired because cancel was
    // already true on entry.
    let updates = progress.updates.lock().unwrap().clone();
    assert_eq!(updates, vec![]);
    put_mock.assert_async().await;
}

#[tokio::test]
async fn multipart_cancel_before_initiate_skips_all_requests() {
    let mut server = mockito::Server::new_async().await;
    let initiate_mock = server
        .mock("POST", "/v2/uploads/initiate")
        .expect(0)
        .create_async()
        .await;

    // Force the multipart path by setting a tiny small-file threshold.
    let client = Client::new(server.url(), "k").with_small_file_threshold(1);
    let cancel = Arc::new(FlagCancel::default());
    cancel.cancel();
    let progress = Arc::new(RecordingProgress::default());
    let reader: Box<dyn SeekableReader> = Box::new(Cursor::new(vec![b'a'; 10_000]));
    let err = client
        .write_stream_with_hooks(
            "/cancel-mp.bin",
            reader,
            10_000,
            -1,
            Some(progress.clone() as Arc<dyn UploadProgress>),
            Some(cancel.clone() as Arc<dyn CancelSignal>),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Drive9Error::Cancelled), "got: {:?}", err);
    assert_eq!(progress.updates.lock().unwrap().clone(), vec![]);
    initiate_mock.assert_async().await;
}

#[tokio::test]
async fn multipart_cancel_between_parts_calls_abort_and_returns_cancelled() {
    let mut server = mockito::Server::new_async().await;
    let upload_id = "upload-1";
    let part_size = 5_000i64;
    let total = 10_000i64;
    let total_parts = 2i32;

    let _initiate = server
        .mock("POST", "/v2/uploads/initiate")
        .with_status(200)
        .with_body(format!(
            r#"{{"upload_id":"{}","key":"k","part_size":{},"total_parts":{}}}"#,
            upload_id, part_size, total_parts
        ))
        .create_async()
        .await;

    let presign_url = format!("{}/v2/uploads/{}/part", server.url(), upload_id);
    let _presign = server
        .mock("POST", format!("/v2/uploads/{}/presign-batch", upload_id).as_str())
        .with_status(200)
        .with_body(format!(
            r#"{{"parts":[{{"number":1,"url":"{u}/1","size":{ps}}},{{"number":2,"url":"{u}/2","size":{ps}}}]}}"#,
            u = presign_url,
            ps = part_size
        ))
        .create_async()
        .await;

    // The cancel future flips the flag after the first part is uploaded.
    let cancel = Arc::new(FlagCancel::default());
    let cancel_for_handler = Arc::clone(&cancel);

    let _put1 = server
        .mock("PUT", format!("/v2/uploads/{}/part/1", upload_id).as_str())
        .with_status(200)
        .with_header("etag", "etag-1")
        .with_body_from_request(move |_req| {
            // Trigger cancel as soon as part 1 has been delivered to the
            // server; part 2 should now be short-circuited.
            cancel_for_handler.cancel();
            Vec::new()
        })
        .create_async()
        .await;

    let put2 = server
        .mock("PUT", format!("/v2/uploads/{}/part/2", upload_id).as_str())
        .with_status(200)
        .with_header("etag", "etag-2")
        .expect_at_most(1)
        .create_async()
        .await;

    let abort_mock = server
        .mock("POST", format!("/v2/uploads/{}/abort", upload_id).as_str())
        .with_status(200)
        .expect(1)
        .create_async()
        .await;

    let complete_mock = server
        .mock("POST", format!("/v2/uploads/{}/complete", upload_id).as_str())
        .expect(0)
        .create_async()
        .await;

    let client = Client::new(server.url(), "k").with_small_file_threshold(1);
    let progress = Arc::new(RecordingProgress::default());
    let reader: Box<dyn SeekableReader> = Box::new(Cursor::new(vec![b'a'; total as usize]));
    let err = client
        .write_stream_with_hooks(
            "/cancel-between.bin",
            reader,
            total,
            -1,
            Some(progress.clone() as Arc<dyn UploadProgress>),
            Some(cancel.clone() as Arc<dyn CancelSignal>),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Drive9Error::Cancelled), "got: {:?}", err);

    abort_mock.assert_async().await;
    complete_mock.assert_async().await;
    put2.assert_async().await;

    // Progress should NOT have reported `(total, total)` — at most the
    // first part's bytes (and only if it raced ahead of the cancel).
    let updates = progress.updates.lock().unwrap().clone();
    for (transferred, t) in &updates {
        assert!(
            *transferred < total as u64,
            "progress should not reach total on cancel; got {:?} (total={})",
            updates,
            t
        );
    }
}

#[tokio::test]
async fn existing_write_stream_path_unchanged() {
    // Sanity: the original write_stream_conditional still works without
    // hooks and goes through the existing v2 path.
    let mut server = mockito::Server::new_async().await;
    let upload_id = "u-existing";
    let part_size = 5_000i64;
    let total = 5_000i64;

    let _init = server
        .mock("POST", "/v2/uploads/initiate")
        .with_status(200)
        .with_body(format!(
            r#"{{"upload_id":"{}","key":"k","part_size":{},"total_parts":1}}"#,
            upload_id, part_size
        ))
        .create_async()
        .await;
    let presign_url = format!("{}/v2/uploads/{}/part/1", server.url(), upload_id);
    let _ps = server
        .mock("POST", format!("/v2/uploads/{}/presign-batch", upload_id).as_str())
        .with_status(200)
        .with_body(format!(
            r#"{{"parts":[{{"number":1,"url":"{}","size":{}}}]}}"#,
            presign_url, part_size
        ))
        .create_async()
        .await;
    let _put = server
        .mock("PUT", format!("/v2/uploads/{}/part/1", upload_id).as_str())
        .with_status(200)
        .with_header("etag", "e1")
        .create_async()
        .await;
    let _complete = server
        .mock("POST", format!("/v2/uploads/{}/complete", upload_id).as_str())
        .with_status(200)
        .create_async()
        .await;

    let client = Client::new(server.url(), "k").with_small_file_threshold(1);
    let reader: Box<dyn SeekableReader> = Box::new(Cursor::new(vec![b'a'; total as usize]));
    client
        .write_stream(/* path */ "/x.bin", reader, total)
        .await
        .unwrap();
}
