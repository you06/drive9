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

use drive9::{transfer::SeekableReader, Client, Drive9Error, FileInfo, SearchResult, StatResult};
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
    /// On any failure (network error, cancellation, write error) the
    /// partial local file is deleted so callers do not mistake a partial
    /// download for the full file. Successful downloads leave the file
    /// in place.
    pub fn download_file(
        &self,
        remote_path: String,
        local_path: String,
        progress: Option<Arc<dyn Drive9ProgressListener>>,
        cancel: Option<Arc<Drive9CancelToken>>,
    ) -> Drive9Result<()> {
        let local_path_for_cleanup = local_path.clone();
        let result = self.rt.block_on(self.download_file_inner(
            remote_path,
            local_path,
            progress,
            cancel,
        ));
        if result.is_err() {
            // Best-effort cleanup of the partial file. Ignored if the file
            // was never created (e.g. failure before File::create).
            self.rt
                .block_on(tokio::fs::remove_file(&local_path_for_cleanup))
                .ok();
        }
        result
    }

    /// Patch specific parts of a remote file using bytes read from
    /// `local_path`. `dirty_parts` are 1-based part numbers; the server
    /// keeps the unlisted parts. `part_size` overrides the server default
    /// when set. `new_size` is the total file size after patching.
    ///
    /// Input validation (rejected with `code = "other"`):
    /// - every `part_num >= 1`
    /// - `part_size`, if set, must be `> 0`
    /// - `new_size >= 0`
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
        part_size: Option<i64>,
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
        if let Some(ps) = part_size {
            if ps <= 0 {
                return Err(Drive9Exception::Drive9 {
                    code: "other".into(),
                    status_code: None,
                    detail: format!("part_size must be > 0, got {}", ps),
                    server_revision: None,
                });
            }
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
        let local_path = local_path.clone();
        let read_part = move |part_num: i32, part_size: i64, _orig: Option<&[u8]>| -> Result<Vec<u8>, Drive9Error> {
            if part_size <= 0 {
                return Err(Drive9Error::Other(format!(
                    "server returned part_size {} for part {}",
                    part_size, part_num
                )));
            }
            let offset = (part_num as i64 - 1) * part_size;
            let mut file = std::fs::File::open(Path::new(&local_path))?;
            file.seek(SeekFrom::Start(offset as u64))?;
            // Last part may be shorter than part_size; stop reading at EOF.
            let mut buf = vec![0u8; part_size as usize];
            let mut filled = 0usize;
            while filled < buf.len() {
                let n = file.read(&mut buf[filled..])?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            // The server-supplied `part_size` here is authoritative for this
            // specific part (the upload plan already accounts for the final
            // short part), so a short read means the local file does not
            // match the declared new_size. Surface that explicitly instead
            // of uploading short data.
            buf.truncate(filled);
            if buf.len() != part_size as usize {
                return Err(Drive9Error::Other(format!(
                    "short read for part {}: expected {} bytes, got {}",
                    part_num,
                    part_size,
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
            part_size,
            expected_revision,
        ))?;
        Ok(())
    }
}

const CHUNK_SIZE: usize = 64 * 1024;

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
