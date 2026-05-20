//! Mobile-friendly UniFFI wrapper for the drive9 Rust SDK.
//!
//! Exposes a minimal FS API (`write`, `read`, `list`, `stat`, `delete`) plus a
//! flat `Drive9Exception` shape that preserves HTTP status and 409 conflict
//! revisions. Each `Drive9MobileClient` owns its own multi-thread Tokio
//! runtime; all calls block the calling thread on that runtime. Higher-level
//! Kotlin / Swift facades wrap these calls in their idiomatic async APIs.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use drive9::{
    transfer::{CancelSignal, SeekableReader, UploadProgress},
    Client, Drive9Error, FileInfo, SearchResult, StatResult, StreamWriter,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::Notify;

uniffi::setup_scaffolding!();

/// Flat error surface for Kotlin / Swift. We keep a single variant so callers
/// always match the same shape; the `code` field distinguishes underlying
/// causes without leaking Rust enum layout across FFI.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum Drive9Exception {
    #[error("{detail}")]
    Drive9 {
        /// Stable error code: "http_status", "conflict", "request", "json",
        /// "io", or "other".
        code: String,
        /// HTTP status code when the error originated from a server response.
        status_code: Option<i32>,
        /// Human-readable message. Named `detail` because Kotlin's
        /// `Throwable.message` is already provided by UniFFI as a debug-style
        /// summary of the variant.
        detail: String,
        /// Server-reported revision for 409 conflicts; preserved so conditional
        /// writes can retry against the latest revision.
        server_revision: Option<i64>,
    },
}

impl From<Drive9Error> for Drive9Exception {
    fn from(e: Drive9Error) -> Self {
        match e {
            Drive9Error::Status {
                status_code,
                message,
            } => Drive9Exception::Drive9 {
                code: "http_status".into(),
                status_code: Some(status_code as i32),
                detail: message,
                server_revision: None,
            },
            Drive9Error::Conflict {
                status_code,
                message,
                server_revision,
            } => Drive9Exception::Drive9 {
                code: "conflict".into(),
                status_code: Some(status_code as i32),
                detail: message,
                server_revision,
            },
            Drive9Error::Request(err) => Drive9Exception::Drive9 {
                code: "request".into(),
                status_code: None,
                detail: err.to_string(),
                server_revision: None,
            },
            Drive9Error::Json(err) => Drive9Exception::Drive9 {
                code: "json".into(),
                status_code: None,
                detail: err.to_string(),
                server_revision: None,
            },
            Drive9Error::Io(err) => Drive9Exception::Drive9 {
                code: "io".into(),
                status_code: None,
                detail: err.to_string(),
                server_revision: None,
            },
            Drive9Error::Other(message) => Drive9Exception::Drive9 {
                code: "other".into(),
                status_code: None,
                detail: message,
                server_revision: None,
            },
            Drive9Error::Cancelled => Drive9Exception::Drive9 {
                code: "cancelled".into(),
                status_code: None,
                detail: "operation cancelled".into(),
                server_revision: None,
            },
        }
    }
}

#[derive(uniffi::Record, Debug, Clone)]
pub struct Drive9FileInfo {
    pub name: String,
    pub size: i64,
    pub is_dir: bool,
    /// Unix timestamp in seconds, if the server reported it.
    pub mtime_unix: Option<i64>,
}

impl From<FileInfo> for Drive9FileInfo {
    fn from(f: FileInfo) -> Self {
        Self {
            name: f.name,
            size: f.size,
            is_dir: f.is_dir,
            mtime_unix: f.mtime,
        }
    }
}

#[derive(uniffi::Record, Debug, Clone)]
pub struct Drive9StatResult {
    pub size: i64,
    pub is_dir: bool,
    pub revision: i64,
    /// Unix timestamp in seconds, if the server reported it.
    pub mtime_unix: Option<i64>,
}

impl From<StatResult> for Drive9StatResult {
    fn from(s: StatResult) -> Self {
        Self {
            size: s.size,
            is_dir: s.is_dir,
            revision: s.revision,
            mtime_unix: s.mtime.map(|d| d.timestamp()),
        }
    }
}

#[derive(uniffi::Record, Debug, Clone)]
pub struct Drive9SearchResult {
    pub path: String,
    pub name: String,
    pub size_bytes: i64,
    /// Optional search relevance score, when the server returns one.
    pub score: Option<f64>,
}

impl From<SearchResult> for Drive9SearchResult {
    fn from(r: SearchResult) -> Self {
        Self {
            path: r.path,
            name: r.name,
            size_bytes: r.size_bytes,
            score: r.score,
        }
    }
}

/// Cooperative cancellation handle. Construct one on the caller side, pass it
/// into `upload_file` / `download_file` / `patch_file_parts`, and call
/// `cancel()` from another task / thread to abort the transfer.
///
/// Cancellation is cooperative: in-flight HTTP requests are dropped when the
/// transfer future is racing against `cancel()`, which terminates the
/// underlying connection. Partial uploads on the server are NOT explicitly
/// aborted (drive9-rs handles its own cleanup on internal errors, but a
/// drop-on-cancel from outside cannot reach those abort paths). Server-side
/// upload sessions typically expire.
#[derive(uniffi::Object)]
pub struct Drive9CancelToken {
    flag: AtomicBool,
    notify: Notify,
}

#[uniffi::export]
impl Drive9CancelToken {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            flag: AtomicBool::new(false),
            notify: Notify::new(),
        })
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

impl CancelSignal for Drive9CancelToken {
    fn is_cancelled(&self) -> bool {
        // Delegate to the inherent method; both read the same AtomicBool.
        Drive9CancelToken::is_cancelled(self)
    }
}

impl Drive9CancelToken {
    /// Resolve when cancellation has been triggered. Safe to call repeatedly.
    async fn wait(&self) {
        loop {
            if self.flag.load(Ordering::SeqCst) {
                return;
            }
            // Register interest BEFORE re-checking the flag to close the race
            // between a `cancel()` call and arrival here.
            let notified = self.notify.notified();
            if self.flag.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
            if self.flag.load(Ordering::SeqCst) {
                return;
            }
        }
    }
}

/// Foreign-implemented progress callback. UniFFI marshals this into a
/// Kotlin interface and a Swift protocol; consumers implement
/// `on_progress(transferred, total)`.
///
/// `total` is 0 when the total size is unknown (e.g. download without a
/// Content-Length). Implementations must be cheap; this is called per chunk
/// during transfer.
#[uniffi::export(with_foreign)]
pub trait Drive9ProgressListener: Send + Sync {
    fn on_progress(&self, transferred: u64, total: u64);
}

/// Adapter that lets a foreign-implemented `Drive9ProgressListener` be used
/// where `drive9-rs` expects a `transfer::UploadProgress`. Kept private so
/// the foreign-only trait does not leak into the lower SDK's signature.
struct ProgressAdapter {
    inner: Arc<dyn Drive9ProgressListener>,
}

impl UploadProgress for ProgressAdapter {
    fn on_progress(&self, transferred: u64, total: u64) {
        self.inner.on_progress(transferred, total);
    }
}

#[derive(uniffi::Object)]
pub struct Drive9MobileClient {
    inner: Client,
    rt: Arc<Runtime>,
}

type Drive9Result<T> = Result<T, Drive9Exception>;

#[uniffi::export]
impl Drive9MobileClient {
    /// Build a client bound to `base_url` and authenticated with `api_key`.
    /// Each client owns a multi-thread Tokio runtime; share one client across
    /// the app rather than constructing per-call.
    #[uniffi::constructor]
    pub fn new(base_url: String, api_key: String) -> Arc<Self> {
        let rt = Builder::new_multi_thread()
            .enable_all()
            .thread_name("drive9-mobile")
            .build()
            .expect("failed to build tokio runtime for drive9-mobile-core");
        Arc::new(Self {
            inner: Client::new(base_url, api_key),
            rt: Arc::new(rt),
        })
    }

    /// Write `data` to `path`. When `expected_revision` is provided, the write
    /// is conditional: a 409 Conflict surfaces as `Drive9Exception` with
    /// `code = "conflict"` and `server_revision` set.
    pub fn write(
        &self,
        path: String,
        data: Vec<u8>,
        expected_revision: Option<i64>,
    ) -> Drive9Result<()> {
        self.rt.block_on(async {
            match expected_revision {
                Some(rev) => self.inner.write_with_revision(&path, &data, rev).await,
                None => self.inner.write(&path, &data).await,
            }
        })?;
        Ok(())
    }

    pub fn read(&self, path: String) -> Drive9Result<Vec<u8>> {
        let bytes = self.rt.block_on(self.inner.read(&path))?;
        Ok(bytes)
    }

    pub fn list(&self, path: String) -> Drive9Result<Vec<Drive9FileInfo>> {
        let entries = self.rt.block_on(self.inner.list(&path))?;
        Ok(entries.into_iter().map(Into::into).collect())
    }

    pub fn stat(&self, path: String) -> Drive9Result<Drive9StatResult> {
        let s = self.rt.block_on(self.inner.stat(&path))?;
        Ok(s.into())
    }

    pub fn delete(&self, path: String) -> Drive9Result<()> {
        self.rt.block_on(self.inner.delete(&path))?;
        Ok(())
    }

    pub fn copy(&self, src_path: String, dst_path: String) -> Drive9Result<()> {
        self.rt
            .block_on(self.inner.copy(&src_path, &dst_path))?;
        Ok(())
    }

    pub fn rename(&self, old_path: String, new_path: String) -> Drive9Result<()> {
        self.rt
            .block_on(self.inner.rename(&old_path, &new_path))?;
        Ok(())
    }

    pub fn mkdir(&self, path: String) -> Drive9Result<()> {
        self.rt.block_on(self.inner.mkdir(&path))?;
        Ok(())
    }

    /// Search by content. `limit` of 0 (or negative) lets the server pick.
    pub fn grep(
        &self,
        query: String,
        path_prefix: String,
        limit: i32,
    ) -> Drive9Result<Vec<Drive9SearchResult>> {
        let results = self
            .rt
            .block_on(self.inner.grep(&query, &path_prefix, limit))?;
        Ok(results.into_iter().map(Into::into).collect())
    }

    /// Search by metadata. `params` is forwarded verbatim to `drive9-rs` so
    /// URL encoding and server-side semantics stay in one place; the wrapper
    /// does not interpret keys.
    pub fn find(
        &self,
        path_prefix: String,
        params: HashMap<String, String>,
    ) -> Drive9Result<Vec<Drive9SearchResult>> {
        let results = self
            .rt
            .block_on(self.inner.find(&path_prefix, &params))?;
        Ok(results.into_iter().map(Into::into).collect())
    }

    /// Run a SQL query. Each result row is returned as a JSON-encoded string
    /// so the FFI surface stays free of arbitrary JSON values; consumers
    /// parse with their preferred JSON library. The wrapper does not
    /// interpret column names or types.
    pub fn sql(&self, query: String) -> Drive9Result<Vec<String>> {
        let rows = self.rt.block_on(self.inner.sql(&query))?;
        Ok(rows.into_iter().map(|v| v.to_string()).collect())
    }

    /// Stream `remote_path` to disk at `local_path`. Progress is emitted
    /// per chunk written (precise: bytes transferred always equal bytes
    /// already on disk). Cancellation is cooperative — checked between
    /// chunks and via `tokio::select!` so a cancel while a chunk read is
    /// in flight terminates the underlying connection. A cancelled
    /// transfer surfaces as `Drive9Exception` with `code = "cancelled"`.
    ///
    /// Download writes to a sibling temp file (`.{name}.drive9-tmp-{nonce}`)
    /// inside the parent directory of `local_path`. On success the temp
    /// file is renamed onto `local_path` (atomic on Unix when both paths
    /// share a filesystem). On any failure — network error, cancel, write
    /// error — only the temp file is removed; any pre-existing file at
    /// `local_path` is left untouched.
    pub fn download_file(
        &self,
        remote_path: String,
        local_path: String,
        progress: Option<Arc<dyn Drive9ProgressListener>>,
        cancel: Option<Arc<Drive9CancelToken>>,
    ) -> Drive9Result<()> {
        let target = Path::new(&local_path);
        let parent = target.parent().filter(|p| !p.as_os_str().is_empty());
        let file_name = target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("download");
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let temp_name = format!(".{}.drive9-tmp-{}", file_name, nonce);
        let temp_path = match parent {
            Some(dir) => dir.join(&temp_name),
            None => std::path::PathBuf::from(&temp_name),
        };
        let temp_path_str = temp_path.to_string_lossy().to_string();

        let result = self.rt.block_on(self.download_file_inner(
            remote_path,
            temp_path_str.clone(),
            progress,
            cancel,
        ));

        match result {
            Ok(()) => {
                if let Err(e) = self.rt.block_on(tokio::fs::rename(&temp_path, target)) {
                    // Best-effort cleanup of the temp file even when rename
                    // fails; the destination file is unaffected because we
                    // never wrote to it directly.
                    self.rt.block_on(tokio::fs::remove_file(&temp_path)).ok();
                    return Err(Drive9Exception::Drive9 {
                        code: "io".into(),
                        status_code: None,
                        detail: format!(
                            "rename temp to {}: {}",
                            target.display(),
                            e
                        ),
                        server_revision: None,
                    });
                }
                Ok(())
            }
            Err(e) => {
                self.rt.block_on(tokio::fs::remove_file(&temp_path)).ok();
                Err(e)
            }
        }
    }

    /// Open a streaming multipart upload. The returned
    /// [`Drive9StreamUpload`] receives parts incrementally via
    /// `write_part`, finalizes via `complete`, or aborts via `abort`.
    /// `total_size` is the final file size in bytes; `part_size` and
    /// concurrency are chosen by the server-side upload plan.
    ///
    /// Phase 4A surfaces only this object-based API; Kotlin Flow / Swift
    /// AsyncSequence wrappers (Phase 4B) sit on top of it.
    pub fn new_stream_upload(
        &self,
        remote_path: String,
        total_size: i64,
        expected_revision: Option<i64>,
    ) -> Arc<Drive9StreamUpload> {
        let inner = match expected_revision {
            Some(rev) => self.inner.new_stream_writer_conditional(&remote_path, total_size, rev),
            None => self.inner.new_stream_writer(&remote_path, total_size),
        };
        Arc::new(Drive9StreamUpload {
            rt: Arc::clone(&self.rt),
            inner: Arc::new(inner),
            state: std::sync::Mutex::new(StreamState::Active),
        })
    }

    /// List vault secrets readable by the current api_key / token.
    ///
    /// The mobile FFI surface for vault is intentionally narrow: only
    /// read paths (this method and `vault_read_secret_field`) are
    /// exposed; admin operations (create/update/delete secrets, issue/
    /// revoke tokens, audit queries) stay off the mobile surface for
    /// now. Token issuance and rotation happen elsewhere (backend) and
    /// the resulting scoped token is what the mobile client uses as
    /// `api_key`.
    ///
    /// Authorization failures (401/403) and missing-secret errors (404)
    /// surface through the existing `code = "http_status"` channel; no
    /// dedicated vault error code is introduced so foreign callers
    /// only have one branch to write.
    pub fn vault_list_readable_secrets(&self) -> Drive9Result<Vec<String>> {
        let names = self.rt.block_on(self.inner.list_readable_vault_secrets())?;
        Ok(names)
    }

    /// Read a single field from a vault secret.
    ///
    /// The wrapper does NOT inspect or transform the returned value: it
    /// is whatever string drive9-rs received from the server, even if
    /// that string looks like JSON. Callers that store JSON-encoded
    /// values in vault fields must parse on their side.
    ///
    /// URL encoding for `name` and `field` is delegated to drive9-rs;
    /// the wrapper does not re-encode.
    pub fn vault_read_secret_field(
        &self,
        name: String,
        field: String,
    ) -> Drive9Result<String> {
        let value = self
            .rt
            .block_on(self.inner.read_vault_secret_field(&name, &field))?;
        Ok(value)
    }

    /// Stream `local_path` to `remote_path`. Progress is reported only
    /// from completed part uploads (multipart) or from the success
    /// transition of the single PUT (small file): a cancelled or failed
    /// upload never emits a `(total, total)` event.
    ///
    /// Cancellation:
    /// - If `cancel` is observed before [`Client::write_stream_with_hooks`]
    ///   issues any HTTP request, no requests are sent.
    /// - For multipart uploads, in-flight part PUTs are allowed to drain
    ///   so server-side multipart state is consistent, then
    ///   `abort_upload_v2(upload_id)` is invoked before this call
    ///   returns. The resulting error has `code = "cancelled"`.
    ///
    /// `expected_revision` makes the write conditional; a 409 surfaces
    /// as `code = "conflict"` with the server-reported `server_revision`.
    pub fn upload_file(
        &self,
        local_path: String,
        remote_path: String,
        expected_revision: Option<i64>,
        progress: Option<Arc<dyn Drive9ProgressListener>>,
        cancel: Option<Arc<Drive9CancelToken>>,
    ) -> Drive9Result<()> {
        let file = std::fs::File::open(Path::new(&local_path)).map_err(Drive9Error::Io)?;
        let size = file.metadata().map_err(Drive9Error::Io)?.len() as i64;
        let reader: Box<dyn SeekableReader> = Box::new(file);
        let progress_for_rs: Option<Arc<dyn UploadProgress>> =
            progress.map(|p| Arc::new(ProgressAdapter { inner: p }) as Arc<dyn UploadProgress>);
        let cancel_for_rs: Option<Arc<dyn CancelSignal>> =
            cancel.map(|c| c as Arc<dyn CancelSignal>);
        self.rt.block_on(self.inner.write_stream_with_hooks(
            &remote_path,
            reader,
            size,
            expected_revision.unwrap_or(-1),
            progress_for_rs,
            cancel_for_rs,
        ))?;
        Ok(())
    }

    /// Patch specific parts of a remote file using bytes read from
    /// `local_path`. `dirty_parts` are 1-based part numbers; the server
    /// keeps the unlisted parts. `part_size` is the part size the caller
    /// used to compute `dirty_parts` and is also sent to the server so
    /// the upload plan uses the same chunking. `new_size` is the total
    /// file size after patching.
    ///
    /// Input validation (rejected with `code = "other"`, before any HTTP
    /// request goes out):
    /// - every `part_num >= 1`
    /// - `part_size > 0`
    /// - `new_size >= 0`
    ///
    /// `part_size` is required (not Optional) because the closure has to
    /// compute file offsets as `(part_num - 1) * part_size`; the
    /// per-part size returned by the upload plan can be smaller than
    /// `part_size` for the final short part and is the wrong value to use
    /// for offset arithmetic.
    ///
    /// Cancellation / progress are NOT supported in this iteration;
    /// dropping the call mid-patch leaves the server-side multipart upload
    /// to expire on its own. Callers that need cancel should compose with
    /// external task cancellation and accept the same caveat.
    pub fn patch_file_parts(
        &self,
        local_path: String,
        remote_path: String,
        dirty_parts: Vec<i32>,
        new_size: i64,
        part_size: i64,
        expected_revision: Option<i64>,
    ) -> Drive9Result<()> {
        if new_size < 0 {
            return Err(Drive9Exception::Drive9 {
                code: "other".into(),
                status_code: None,
                detail: format!("new_size must be >= 0, got {}", new_size),
                server_revision: None,
            });
        }
        if part_size <= 0 {
            return Err(Drive9Exception::Drive9 {
                code: "other".into(),
                status_code: None,
                detail: format!("part_size must be > 0, got {}", part_size),
                server_revision: None,
            });
        }
        if dirty_parts.iter().any(|&p| p < 1) {
            return Err(Drive9Exception::Drive9 {
                code: "other".into(),
                status_code: None,
                detail: "dirty_parts contain a non-positive part number".into(),
                server_revision: None,
            });
        }
        let dirty = dirty_parts.clone();
        let global_part_size = part_size;
        let local_path = local_path.clone();
        let read_part = move |part_num: i32, requested_size: i64, _orig: Option<&[u8]>| -> Result<Vec<u8>, Drive9Error> {
            if requested_size <= 0 {
                return Err(Drive9Error::Other(format!(
                    "server returned requested_size {} for part {}",
                    requested_size, part_num
                )));
            }
            // Offset uses the caller-supplied global part_size; the upload
            // plan may report a smaller `requested_size` for the final
            // short part, which is correct for the read length but wrong
            // for computing where this part begins in the local file.
            let offset = (part_num as i64 - 1) * global_part_size;
            let mut file = std::fs::File::open(Path::new(&local_path))?;
            file.seek(SeekFrom::Start(offset as u64))?;
            let mut buf = vec![0u8; requested_size as usize];
            let mut filled = 0usize;
            while filled < buf.len() {
                let n = file.read(&mut buf[filled..])?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            // A short read means the local file does not extend to cover
            // the bytes the upload plan asked for; surface this instead of
            // uploading truncated data and pretending success.
            buf.truncate(filled);
            if buf.len() != requested_size as usize {
                return Err(Drive9Error::Other(format!(
                    "short read for part {}: expected {} bytes, got {}",
                    part_num,
                    requested_size,
                    buf.len()
                )));
            }
            Ok(buf)
        };
        self.rt.block_on(self.inner.patch_file(
            &remote_path,
            new_size,
            &dirty,
            read_part,
            Some(part_size),
            expected_revision,
        ))?;
        Ok(())
    }
}

const CHUNK_SIZE: usize = 64 * 1024;

/// Lifecycle state of a [`Drive9StreamUpload`]. Enforced in the wrapper
/// so foreign callers see consistent rejection messages regardless of
/// what the underlying `drive9::StreamWriter` happens to report; the
/// wrapper still consults the writer's own state for fine-grained
/// per-part errors (those surface as `Errored` here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamState {
    Active,
    Completed,
    Aborted,
    Errored,
}

/// Streaming multipart upload exposed across FFI as a UniFFI object.
///
/// State machine:
/// - `Active` → can call `write_part` / `complete` / `abort`.
/// - After `complete` returns Ok: `Completed`. All further calls reject.
/// - After `abort` returns Ok: `Aborted`. `abort` itself stays
///   idempotent; other calls reject.
/// - When `write_part` or `complete` detects a background upload error
///   surfaced by `drive9-rs`, the state transitions to `Errored`.
///   `abort` is still callable in this state so callers can clean up
///   server-side multipart bookkeeping; other calls reject.
///
/// Backpressure is the underlying `StreamWriter`'s semaphore: once 16
/// parts are in flight, the next `write_part` blocks until a permit is
/// released. The Phase 4A test
/// `write_part_queued_at_permit_aborts_without_uploading` in
/// `drive9-rs` covers the queued-vs-close race that this object
/// inherits.
#[derive(uniffi::Object)]
pub struct Drive9StreamUpload {
    rt: Arc<Runtime>,
    inner: Arc<StreamWriter>,
    state: std::sync::Mutex<StreamState>,
}

#[uniffi::export]
impl Drive9StreamUpload {
    /// Queue a part for upload. `part_num` is 1-based; parts may be
    /// written in any order subject to the server-side plan. The call
    /// returns once the part has been accepted by the underlying
    /// concurrency-limit semaphore; the actual HTTP PUT runs in a Tokio
    /// task and any failure surfaces in a subsequent `write_part` or
    /// `complete` call (the object transitions to `Errored`).
    pub fn write_part(&self, part_num: i32, data: Vec<u8>) -> Drive9Result<()> {
        self.guard_writable_or_err("write_part")?;
        let result = self.rt.block_on(self.inner.write_part(part_num, data));
        self.observe_result(&result);
        result?;
        Ok(())
    }

    /// Finalize the upload. `final_part_num` is the part number for the
    /// last chunk (which may be smaller than `part_size`); pass an
    /// empty `final_data` if the last part was already written via
    /// `write_part`.
    pub fn complete(&self, final_part_num: i32, final_data: Vec<u8>) -> Drive9Result<()> {
        self.guard_writable_or_err("complete")?;
        let result = self
            .rt
            .block_on(self.inner.complete(final_part_num, final_data));
        match result {
            Ok(()) => {
                *self.state.lock().unwrap() = StreamState::Completed;
                Ok(())
            }
            Err(e) => {
                *self.state.lock().unwrap() = StreamState::Errored;
                Err(Drive9Exception::from(e))
            }
        }
    }

    /// Explicit abort. Idempotent: calling abort on an already-aborted
    /// upload returns Ok without contacting the server again. Allowed
    /// in any non-Completed state so callers can clean up server-side
    /// multipart bookkeeping after an upload error.
    pub fn abort(&self) -> Drive9Result<()> {
        {
            let s = self.state.lock().unwrap();
            if *s == StreamState::Aborted {
                return Ok(());
            }
            if *s == StreamState::Completed {
                return Err(Drive9Exception::Drive9 {
                    code: "other".into(),
                    status_code: None,
                    detail: "stream upload already completed; cannot abort".into(),
                    server_revision: None,
                });
            }
        }
        let result = self.rt.block_on(self.inner.abort());
        match result {
            Ok(()) => {
                *self.state.lock().unwrap() = StreamState::Aborted;
                Ok(())
            }
            Err(e) => {
                // We don't transition the state when the server abort
                // itself failed — let the caller decide whether to
                // retry. Returning an error here keeps that contract.
                Err(Drive9Exception::from(e))
            }
        }
    }
}

impl Drive9StreamUpload {
    fn guard_writable_or_err(&self, op: &str) -> Drive9Result<()> {
        let s = *self.state.lock().unwrap();
        match s {
            StreamState::Active => Ok(()),
            StreamState::Completed => Err(Drive9Exception::Drive9 {
                code: "other".into(),
                status_code: None,
                detail: format!("{}: stream upload already completed", op),
                server_revision: None,
            }),
            StreamState::Aborted => Err(Drive9Exception::Drive9 {
                code: "other".into(),
                status_code: None,
                detail: format!("{}: stream upload already aborted", op),
                server_revision: None,
            }),
            StreamState::Errored => Err(Drive9Exception::Drive9 {
                code: "other".into(),
                status_code: None,
                detail: format!(
                    "{}: stream upload is in errored state; call abort() to clean up",
                    op
                ),
                server_revision: None,
            }),
        }
    }

    fn observe_result(&self, result: &Result<(), Drive9Error>) {
        if result.is_err() {
            let mut s = self.state.lock().unwrap();
            if *s == StreamState::Active {
                *s = StreamState::Errored;
            }
        }
    }
}

impl Drive9MobileClient {
    async fn download_file_inner(
        &self,
        remote_path: String,
        local_path: String,
        progress: Option<Arc<dyn Drive9ProgressListener>>,
        cancel: Option<Arc<Drive9CancelToken>>,
    ) -> Drive9Result<()> {
        let total = match self.inner.stat(&remote_path).await {
            Ok(s) => s.size.max(0) as u64,
            // Stat may fail for reasons unrelated to read (e.g. server
            // doesn't expose HEAD); fall back to 0 = unknown total.
            Err(_) => 0,
        };

        let cancelled_err = || Drive9Exception::Drive9 {
            code: "cancelled".into(),
            status_code: None,
            detail: "transfer cancelled".into(),
            server_revision: None,
        };

        if let Some(c) = &cancel {
            if c.is_cancelled() {
                return Err(cancelled_err());
            }
        }

        let stream_fut = self.inner.read_stream(&remote_path);
        let mut reader = match cancel.clone() {
            Some(c) => tokio::select! {
                r = stream_fut => r?,
                _ = c.wait() => return Err(cancelled_err()),
            },
            None => stream_fut.await?,
        };

        let mut file = tokio::fs::File::create(&local_path)
            .await
            .map_err(Drive9Error::Io)?;

        let mut buf = vec![0u8; CHUNK_SIZE];
        let mut transferred: u64 = 0;
        if let Some(p) = &progress {
            p.on_progress(0, total);
        }

        loop {
            if let Some(c) = &cancel {
                if c.is_cancelled() {
                    return Err(cancelled_err());
                }
            }
            let read_fut = reader.read(&mut buf);
            let n = match cancel.clone() {
                Some(c) => tokio::select! {
                    n = read_fut => n.map_err(Drive9Error::Io)?,
                    _ = c.wait() => return Err(cancelled_err()),
                },
                None => read_fut.await.map_err(Drive9Error::Io)?,
            };
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n]).await.map_err(Drive9Error::Io)?;
            transferred += n as u64;
            if let Some(p) = &progress {
                p.on_progress(transferred, total);
            }
        }
        file.flush().await.map_err(Drive9Error::Io)?;
        Ok(())
    }
}

/// Helper for callers who want to wrap arbitrary `Read + Seek + Send` types
/// into the `SeekableReader` trait. Currently unused (download is the only
/// streaming path exposed in Phase 2B-1) but kept for the upcoming upload
/// implementation so the trait stays visible from the public API surface.
#[allow(dead_code)]
fn _seekable_reader_witness(r: Box<dyn SeekableReader>) -> Box<dyn SeekableReader> {
    r
}
