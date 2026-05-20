//! Tests for the lower-level `StreamWriter` API — specifically the
//! state-machine race between a `write_part` waiting for a semaphore
//! permit and a concurrent `abort()` / `complete()`.
//!
//! The mobile FFI surface (Phase 4A `Drive9StreamUpload`) sits on top
//! of this and assumes the underlying writer honours close requests
//! even when some `write_part` calls are still queued at the
//! concurrency limit.

use std::sync::Arc;
use std::time::Duration;

use drive9::Client;
use mockito::Matcher;

const UPLOAD_MAX_CONCURRENCY: i32 = 16;
const PART_SIZE: i64 = 100;

/// StreamWriter.write_part calls `presign_one_part` (singular) — one
/// POST per part to /v2/uploads/{id}/presign with body `{"part_number":N}`.
/// This handler reads N from the body and returns a `PresignedPart`
/// pointing at /upload/N on the same mockito server.
fn build_presign_one_handler(
    base_url: String,
) -> impl Fn(&mockito::Request) -> Vec<u8> + Send + Sync + 'static {
    move |req: &mockito::Request| {
        let body: serde_json::Value =
            serde_json::from_slice(req.body().expect("request body")).expect("json");
        let n = body["part_number"]
            .as_i64()
            .expect("part_number in presign body") as i32;
        serde_json::to_vec(&serde_json::json!({
            "number": n,
            "url": format!("{}/upload/{}", base_url, n),
            "size": PART_SIZE,
        }))
        .unwrap()
    }
}

#[tokio::test]
async fn write_part_queued_at_permit_aborts_without_uploading() {
    // 17 part writes against a stream with UPLOAD_MAX_CONCURRENCY=16.
    // The first 16 PUT handlers each sleep so their tasks hold their
    // semaphore permits long enough for write_part #17 to queue. We
    // then call abort() before any of the 16 PUTs return. By the time
    // the permits free up, write_part #17 must observe state.aborted
    // / state.closing on its re-check and skip its PUT entirely.
    let mut server = mockito::Server::new_async().await;
    let total_parts = UPLOAD_MAX_CONCURRENCY + 1;
    let upload_id = "u-race";
    let total_size = PART_SIZE * total_parts as i64;

    let _init = server
        .mock("POST", "/v2/uploads/initiate")
        .with_status(200)
        .with_body(format!(
            r#"{{"upload_id":"{u}","key":"k","part_size":{ps},"total_parts":{tp}}}"#,
            u = upload_id,
            ps = PART_SIZE,
            tp = total_parts,
        ))
        .create_async()
        .await;

    let presign_handler = build_presign_one_handler(server.url());
    let _presign = server
        .mock(
            "POST",
            format!("/v2/uploads/{}/presign", upload_id).as_str(),
        )
        .with_status(200)
        .with_body_from_request(presign_handler)
        .create_async()
        .await;

    // All allowed PUTs go through `/upload/N`. We use a regex matcher
    // to count cumulative PUT hits regardless of part number.
    let put_mock = server
        .mock("PUT", Matcher::Regex(r"^/upload/\d+$".to_string()))
        .with_status(200)
        .with_header("etag", "etag")
        .with_body_from_request(|_| {
            // Hold the permit long enough for write_part #17 to queue
            // and for the test driver to call abort().
            std::thread::sleep(Duration::from_millis(200));
            Vec::new()
        })
        .expect_at_most(UPLOAD_MAX_CONCURRENCY as usize)
        .create_async()
        .await;

    let abort_mock = server
        .mock(
            "POST",
            format!("/v2/uploads/{}/abort", upload_id).as_str(),
        )
        .with_status(200)
        .expect(1)
        .create_async()
        .await;

    let complete_mock = server
        .mock(
            "POST",
            format!("/v2/uploads/{}/complete", upload_id).as_str(),
        )
        .expect(0)
        .create_async()
        .await;

    let client = Client::new(server.url(), "k");
    let writer = Arc::new(client.new_stream_writer("/race.bin", total_size));

    // Fire the first 16 write_parts; each returns once the semaphore
    // hands out its permit. They'll race to dispatch but stay in flight
    // because the PUT handlers sleep.
    for part_num in 1..=UPLOAD_MAX_CONCURRENCY {
        writer
            .write_part(part_num, vec![b'a'; PART_SIZE as usize])
            .await
            .unwrap();
    }

    // write_part #17 should now block on Semaphore::acquire_owned().
    // Drive it from a separate task so we can abort from the main task.
    let writer_for_queued = Arc::clone(&writer);
    let queued = tokio::spawn(async move {
        writer_for_queued
            .write_part(UPLOAD_MAX_CONCURRENCY + 1, vec![b'a'; PART_SIZE as usize])
            .await
    });

    // Give it a beat to register at the semaphore, then fire abort.
    tokio::time::sleep(Duration::from_millis(50)).await;
    writer.abort().await.unwrap();

    let queued_result = queued.await.unwrap();
    let err = queued_result.expect_err("queued write_part should fail after abort");
    let detail = format!("{}", err);
    assert!(
        detail.contains("aborted") || detail.contains("closing"),
        "want aborted/closing in error detail, got: {}",
        detail
    );

    // Subsequent operations must reject (state-machine, no PUT issued).
    let later = writer
        .write_part(UPLOAD_MAX_CONCURRENCY + 2, vec![b'a'; PART_SIZE as usize])
        .await
        .expect_err("write after abort must reject");
    assert!(
        format!("{}", later).contains("aborted"),
        "want aborted in error: {}",
        later
    );
    let complete_err = writer
        .complete(UPLOAD_MAX_CONCURRENCY + 3, Vec::new())
        .await
        .expect_err("complete after abort must reject");
    assert!(format!("{}", complete_err).contains("aborted"));

    put_mock.assert_async().await;
    abort_mock.assert_async().await;
    complete_mock.assert_async().await;
}

#[tokio::test]
async fn abort_is_idempotent() {
    let mut server = mockito::Server::new_async().await;
    let upload_id = "u-idempotent";
    let _init = server
        .mock("POST", "/v2/uploads/initiate")
        .with_status(200)
        .with_body(format!(
            r#"{{"upload_id":"{u}","key":"k","part_size":{ps},"total_parts":1}}"#,
            u = upload_id,
            ps = PART_SIZE
        ))
        .create_async()
        .await;
    let presign_handler = build_presign_one_handler(server.url());
    let _presign = server
        .mock(
            "POST",
            format!("/v2/uploads/{}/presign", upload_id).as_str(),
        )
        .with_status(200)
        .with_body_from_request(presign_handler)
        .create_async()
        .await;
    let _put = server
        .mock("PUT", "/upload/1")
        .with_status(200)
        .with_header("etag", "etag")
        .create_async()
        .await;
    // abort_upload_v2 should be called at most once across both
    // abort() invocations; the second call is a no-op.
    let abort_mock = server
        .mock(
            "POST",
            format!("/v2/uploads/{}/abort", upload_id).as_str(),
        )
        .with_status(200)
        .expect(1)
        .create_async()
        .await;

    let client = Client::new(server.url(), "k");
    let writer = client.new_stream_writer("/idem.bin", PART_SIZE);
    writer
        .write_part(1, vec![b'a'; PART_SIZE as usize])
        .await
        .unwrap();
    writer.abort().await.unwrap();
    writer.abort().await.unwrap();
    abort_mock.assert_async().await;
}
