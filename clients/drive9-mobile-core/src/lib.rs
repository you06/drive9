//! Mobile-friendly UniFFI wrapper for the drive9 Rust SDK.
//!
//! Exposes a minimal FS API (`write`, `read`, `list`, `stat`, `delete`) plus a
//! flat `Drive9Exception` shape that preserves HTTP status and 409 conflict
//! revisions. Each `Drive9MobileClient` owns its own multi-thread Tokio
//! runtime; all calls block the calling thread on that runtime. Higher-level
//! Kotlin / Swift facades wrap these calls in their idiomatic async APIs.

use std::collections::HashMap;
use std::sync::Arc;

use drive9::{Client, Drive9Error, FileInfo, SearchResult, StatResult};
use tokio::runtime::{Builder, Runtime};

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
}
