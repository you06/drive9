use crate::client::Client;
use crate::error::{check_error, Drive9Error};
use crate::models::{CompletePart, PartURL, PresignedPart, UploadMeta, UploadPlan, UploadPlanV2};
use bytes::Bytes;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use serde_json::json;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio_util::io::StreamReader;

const PART_SIZE: i64 = 8 * 1024 * 1024;
const UPLOAD_MAX_CONCURRENCY: usize = 16;
const UPLOAD_MAX_BUFFER_BYTES: i64 = 256 * 1024 * 1024;

fn upload_parallelism(part_size: i64) -> usize {
    let by_memory = (UPLOAD_MAX_BUFFER_BYTES / part_size).max(1) as usize;
    by_memory.min(UPLOAD_MAX_CONCURRENCY)
}

fn checksum_parallelism(part_size: i64, part_count: usize) -> usize {
    let by_memory = (UPLOAD_MAX_BUFFER_BYTES / part_size).max(1) as usize;
    part_count.min(by_memory)
}

fn compute_crc32c(data: &[u8]) -> String {
    let v = crc32c::crc32c(data);
    let b = v.to_be_bytes();
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b)
}

fn calc_parts(total_size: i64, part_size: i64) -> Vec<(i32, i64)> {
    if total_size <= 0 {
        return vec![];
    }
    let num_parts = (total_size + part_size - 1) / part_size;
    (0..num_parts)
        .map(|i| {
            let offset = i * part_size;
            let size = part_size.min(total_size - offset);
            ((i + 1) as i32, size)
        })
        .collect()
}

pub trait SeekableReader: Read + Seek + Send {}
impl<T: Read + Seek + Send> SeekableReader for T {}

/// Upload progress hook for [`Client::write_stream_with_hooks`].
///
/// Currently only used by upload stream hooks (small file single PUT and
/// v1 / v2 multipart). Not a general transfer-framework abstraction: if
/// download or patch grow real progress reporting later, we'll decide
/// whether to share a trait or have a separate one then.
pub trait UploadProgress: Send + Sync {
    /// `transferred` is monotonically non-decreasing across calls within
    /// a single upload and never exceeds `total`. Implementations must be
    /// cheap; they are invoked from upload worker tasks.
    fn on_progress(&self, transferred: u64, total: u64);
}

/// Cooperative cancel signal for [`Client::write_stream_with_hooks`].
///
/// Polled at safe boundaries (before initiate, before each part upload).
/// In-flight HTTP requests for an already-started part are allowed to
/// finish so server-side multipart state stays consistent; the abort
/// happens via [`abort_upload_v2`](Client::abort_upload_v2) after the
/// outstanding work settles.
///
/// Only used by upload stream hooks for now; see [`UploadProgress`] for
/// the same scope caveat.
pub trait CancelSignal: Send + Sync {
    fn is_cancelled(&self) -> bool;
}

/// Resolve once the cancel signal flips to `true`. Polls because trait
/// objects can't easily expose `async` methods; the small-file PUT path
/// races this against the single PUT future via `tokio::select!`, so an
/// in-flight HTTP request gets dropped on cancel. Multipart paths poll
/// `is_cancelled()` directly at part boundaries instead.
async fn wait_for_cancel(cancel: Arc<dyn CancelSignal>) {
    let mut delay = std::time::Duration::from_millis(10);
    let max_delay = std::time::Duration::from_millis(100);
    loop {
        if cancel.is_cancelled() {
            return;
        }
        tokio::time::sleep(delay).await;
        if delay < max_delay {
            delay = (delay * 2).min(max_delay);
        }
    }
}

struct SyncReader {
    inner: std::sync::Mutex<Box<dyn SeekableReader>>,
}

impl SyncReader {
    fn new(inner: Box<dyn SeekableReader>) -> Self {
        Self {
            inner: std::sync::Mutex::new(inner),
        }
    }
    fn seek_read(&self, offset: u64, size: usize) -> std::io::Result<Vec<u8>> {
        let mut guard = self.inner.lock().unwrap();
        guard.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; size];
        guard.read_exact(&mut buf)?;
        Ok(buf)
    }
}

impl Client {
    pub async fn write_stream(
        &self,
        path: &str,
        reader: Box<dyn SeekableReader>,
        size: i64,
    ) -> Result<(), Drive9Error> {
        self.write_stream_conditional(path, reader, size, -1).await
    }

    pub async fn write_stream_conditional(
        &self,
        path: &str,
        reader: Box<dyn SeekableReader>,
        size: i64,
        expected_revision: i64,
    ) -> Result<(), Drive9Error> {
        let threshold = self.small_file_threshold;
        if size < threshold {
            let data = tokio::task::spawn_blocking(move || {
                let mut buf = Vec::new();
                let mut r = reader;
                r.read_to_end(&mut buf)?;
                Ok::<_, std::io::Error>(buf)
            })
            .await
            .map_err(|e| Drive9Error::Other(format!("join error: {}", e)))?;
            let data = data.map_err(Drive9Error::Io)?;
            return self
                .write_with_revision(path, &data, expected_revision)
                .await;
        }

        let sync_reader = Arc::new(SyncReader::new(reader));

        match self
            .write_stream_v2(path, Arc::clone(&sync_reader), size, expected_revision)
            .await
        {
            Ok(()) => Ok(()),
            Err(Drive9Error::Other(ref s)) if s.contains("v2 upload API not available") => {
                self.write_stream_v1(path, sync_reader, size, expected_revision)
                    .await
            }
            Err(e) => Err(e),
        }
    }

    /// Upload `reader` to `path` with optional progress + cancel hooks.
    ///
    /// Behaviour vs [`write_stream_conditional`]:
    /// - Cancel observed before [`initiate_upload_v2`] (or the small-file
    ///   PUT): returns [`Drive9Error::Cancelled`] without any HTTP request.
    /// - Cancel observed mid-upload: outstanding part PUTs are allowed to
    ///   finish; subsequent parts are short-circuited; then
    ///   [`abort_upload_v2`] is invoked so server-side state does not
    ///   leak; returns [`Drive9Error::Cancelled`] regardless of any
    ///   per-part errors observed while draining.
    /// - Progress: each successful part adds its size to a counter and
    ///   triggers `on_progress(transferred, total)`. Small-file path
    ///   emits `(0, total)` before the PUT and `(total, total)` only on
    ///   success. Cancelled / failed uploads never emit a "completed"
    ///   progress event.
    pub async fn write_stream_with_hooks(
        &self,
        path: &str,
        reader: Box<dyn SeekableReader>,
        size: i64,
        expected_revision: i64,
        progress: Option<Arc<dyn UploadProgress>>,
        cancel: Option<Arc<dyn CancelSignal>>,
    ) -> Result<(), Drive9Error> {
        if let Some(c) = &cancel {
            if c.is_cancelled() {
                return Err(Drive9Error::Cancelled);
            }
        }

        let threshold = self.small_file_threshold;
        if size < threshold {
            return self
                .write_stream_small_with_hooks(
                    path,
                    reader,
                    size,
                    expected_revision,
                    progress,
                    cancel,
                )
                .await;
        }

        let sync_reader = Arc::new(SyncReader::new(reader));
        self.write_stream_v2_with_hooks(
            path,
            Arc::clone(&sync_reader),
            size,
            expected_revision,
            progress,
            cancel,
        )
        .await
    }

    async fn write_stream_small_with_hooks(
        &self,
        path: &str,
        reader: Box<dyn SeekableReader>,
        size: i64,
        expected_revision: i64,
        progress: Option<Arc<dyn UploadProgress>>,
        cancel: Option<Arc<dyn CancelSignal>>,
    ) -> Result<(), Drive9Error> {
        let total = size.max(0) as u64;
        if let Some(p) = &progress {
            p.on_progress(0, total);
        }

        let data = tokio::task::spawn_blocking(move || {
            let mut buf = Vec::new();
            let mut r = reader;
            r.read_to_end(&mut buf)?;
            Ok::<_, std::io::Error>(buf)
        })
        .await
        .map_err(|e| Drive9Error::Other(format!("join error: {}", e)))?
        .map_err(Drive9Error::Io)?;

        if let Some(c) = &cancel {
            if c.is_cancelled() {
                return Err(Drive9Error::Cancelled);
            }
        }

        let upload_fut = self.write_with_revision(path, &data, expected_revision);
        let result = match cancel.clone() {
            Some(c) => tokio::select! {
                r = upload_fut => r,
                _ = wait_for_cancel(c) => return Err(Drive9Error::Cancelled),
            },
            None => upload_fut.await,
        };
        result?;
        if let Some(p) = &progress {
            p.on_progress(total, total);
        }
        Ok(())
    }

    async fn write_stream_v2_with_hooks(
        &self,
        path: &str,
        reader: Arc<SyncReader>,
        size: i64,
        expected_revision: i64,
        progress: Option<Arc<dyn UploadProgress>>,
        cancel: Option<Arc<dyn CancelSignal>>,
    ) -> Result<(), Drive9Error> {
        let plan = self.initiate_upload_v2(path, size, expected_revision).await?;
        let total = size.max(0) as u64;
        if let Some(p) = &progress {
            p.on_progress(0, total);
        }

        let upload_id = plan.upload_id.clone();
        let parts_result = self
            .upload_parts_v2_with_hooks(&plan, reader, total, progress.clone(), cancel.clone())
            .await;

        // If cancel was observed at any point, treat the whole upload as
        // cancelled and clean up server-side multipart state. We do this
        // even when upload_parts_v2_with_hooks returned Ok, because a
        // race between the last part completing and the cancel signal
        // arriving should still produce a Cancelled outcome to the
        // caller.
        let cancelled = cancel.as_ref().map(|c| c.is_cancelled()).unwrap_or(false);
        if cancelled {
            let _ = self.abort_upload_v2(&upload_id).await;
            return Err(Drive9Error::Cancelled);
        }

        let parts = match parts_result {
            Ok(p) => p,
            Err(e) => {
                let _ = self.abort_upload_v2(&upload_id).await;
                return Err(e);
            }
        };

        if let Err(e) = self.complete_upload_v2(&upload_id, &parts).await {
            let _ = self.abort_upload_v2(&upload_id).await;
            return Err(e);
        }
        Ok(())
    }

    async fn upload_parts_v2_with_hooks(
        &self,
        plan: &UploadPlanV2,
        reader: Arc<SyncReader>,
        total_bytes: u64,
        progress: Option<Arc<dyn UploadProgress>>,
        cancel: Option<Arc<dyn CancelSignal>>,
    ) -> Result<Vec<CompletePart>, Drive9Error> {
        let part_size = plan.part_size;
        let total_parts = plan.total_parts;
        let upload_id = plan.upload_id.clone();
        let parallelism = upload_parallelism(part_size);

        let mut presigned = vec![];
        let batch_size = parallelism as i32;
        let mut start = 1i32;
        while start <= total_parts {
            if cancel.as_ref().map(|c| c.is_cancelled()).unwrap_or(false) {
                return Err(Drive9Error::Cancelled);
            }
            let end = (start + batch_size - 1).min(total_parts);
            let batch = self.presign_batch(&upload_id, start, end).await?;
            presigned.extend(batch);
            start = end + 1;
        }

        let sem = Arc::new(Semaphore::new(parallelism));
        let mut tasks = vec![];
        let results: Vec<Option<CompletePart>> = vec![None; total_parts as usize];
        let results_arc = Arc::new(tokio::sync::Mutex::new(results));
        let transferred = Arc::new(std::sync::atomic::AtomicU64::new(0));

        for pp in presigned {
            let permit = Arc::clone(&sem).acquire_owned().await.unwrap();
            let client = self.clone();
            let r = Arc::clone(&reader);
            let res = Arc::clone(&results_arc);
            let psize = part_size;
            let uid = upload_id.clone();
            let cancel_for_task = cancel.clone();
            let progress_for_task = progress.clone();
            let transferred_for_task = Arc::clone(&transferred);
            let task = tokio::spawn(async move {
                let _permit = permit;
                if cancel_for_task
                    .as_ref()
                    .map(|c| c.is_cancelled())
                    .unwrap_or(false)
                {
                    return Err(Drive9Error::Cancelled);
                }
                let offset = (pp.number - 1) as i64 * psize;
                let data = tokio::task::spawn_blocking(move || {
                    r.seek_read(offset as u64, pp.size as usize)
                })
                .await
                .map_err(|e| Drive9Error::Other(format!("join error: {}", e)))?;
                let data = data.map_err(Drive9Error::Io)?;
                let part_bytes = data.len() as u64;
                let etag = client.upload_one_part_v2(&uid, &pp, &data).await?;
                // Progress is only emitted if the upload as a whole has
                // not been cancelled at the moment this part completes;
                // this avoids the "got total but actually cancelled"
                // race the reviewer flagged.
                let cancelled_now = cancel_for_task
                    .as_ref()
                    .map(|c| c.is_cancelled())
                    .unwrap_or(false);
                if !cancelled_now {
                    let new_total = transferred_for_task
                        .fetch_add(part_bytes, std::sync::atomic::Ordering::SeqCst)
                        + part_bytes;
                    if let Some(p) = &progress_for_task {
                        p.on_progress(new_total, total_bytes);
                    }
                }
                res.lock().await[pp.number as usize - 1] = Some(CompletePart {
                    number: pp.number,
                    etag,
                });
                Ok::<(), Drive9Error>(())
            });
            tasks.push(task);
        }

        let mut first_err: Option<Drive9Error> = None;
        for t in tasks {
            match t.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) if first_err.is_none() => first_err = Some(e),
                Err(e) if first_err.is_none() => {
                    first_err = Some(Drive9Error::Other(format!("task panicked: {}", e)))
                }
                _ => {}
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }

        let guard = results_arc.lock().await;
        Ok(guard.iter().filter_map(|x| x.clone()).collect())
    }

    async fn write_stream_v1(
        &self,
        path: &str,
        reader: Arc<SyncReader>,
        size: i64,
        expected_revision: i64,
    ) -> Result<(), Drive9Error> {
        let checksums = compute_part_checksums(Arc::clone(&reader), size, PART_SIZE).await?;
        let plan = self
            .initiate_upload(path, size, &checksums, expected_revision)
            .await?;
        self.upload_parts_v1(&plan, reader).await
    }

    async fn write_stream_v2(
        &self,
        path: &str,
        reader: Arc<SyncReader>,
        size: i64,
        expected_revision: i64,
    ) -> Result<(), Drive9Error> {
        let plan = self
            .initiate_upload_v2(path, size, expected_revision)
            .await?;
        let parts = match self.upload_parts_v2(&plan, reader).await {
            Ok(p) => p,
            Err(e) => {
                let _ = self.abort_upload_v2(&plan.upload_id).await;
                return Err(e);
            }
        };
        if let Err(e) = self.complete_upload_v2(&plan.upload_id, &parts).await {
            let _ = self.abort_upload_v2(&plan.upload_id).await;
            return Err(e);
        }
        Ok(())
    }

    pub async fn read_stream(
        &self,
        path: &str,
    ) -> Result<Box<dyn tokio::io::AsyncRead + Unpin + Send>, Drive9Error> {
        let resp = self
            .http
            .get(self.fs_url(path))
            .headers(self.auth_headers())
            .send()
            .await?;
        let status = resp.status();
        if status.as_u16() == 302 || status.as_u16() == 307 {
            let location = resp
                .headers()
                .get("location")
                .or_else(|| resp.headers().get("Location"))
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| Drive9Error::Other("302 without Location header".to_string()))?;
            let resp2 = self.http.get(location).send().await?;
            let resp2 = check_error(resp2).await?;
            let stream = resp2.bytes_stream();
            let reader =
                StreamReader::new(stream.map(|item| {
                    item.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
                }));
            return Ok(Box::new(reader));
        }
        let resp = check_error(resp).await?;
        let stream = resp.bytes_stream();
        let reader = StreamReader::new(
            stream.map(|item| item.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))),
        );
        Ok(Box::new(reader))
    }

    pub async fn read_stream_range(
        &self,
        path: &str,
        offset: i64,
        length: i64,
    ) -> Result<Box<dyn tokio::io::AsyncRead + Unpin + Send>, Drive9Error> {
        if length <= 0 {
            return Ok(Box::new(std::io::Cursor::new(Vec::new())));
        }
        let resp = self
            .http
            .get(self.fs_url(path))
            .headers(self.auth_headers())
            .header("Range", format!("bytes={}-{}", offset, offset + length - 1))
            .send()
            .await?;
        let status = resp.status();
        if status.as_u16() == 206 {
            let stream = resp.bytes_stream();
            let reader = StreamReader::new(stream.map(|item| {
                item.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
            }));
            return Ok(Box::new(reader));
        }
        if status.as_u16() == 416 {
            return Ok(Box::new(std::io::Cursor::new(Vec::new())));
        }
        if status.as_u16() == 302 || status.as_u16() == 307 {
            let location = resp
                .headers()
                .get("location")
                .or_else(|| resp.headers().get("Location"))
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| Drive9Error::Other("302 without Location header".to_string()))?;
            let resp2 = self
                .http
                .get(location)
                .header("Range", format!("bytes={}-{}", offset, offset + length - 1))
                .send()
                .await?;
            let status2 = resp2.status();
            if status2.as_u16() == 206 {
                let stream = resp2.bytes_stream();
                let reader = StreamReader::new(stream.map(|item| {
                    item.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
                }));
                return Ok(Box::new(reader));
            }
            if status2.as_u16() == 416 {
                return Ok(Box::new(std::io::Cursor::new(Vec::new())));
            }
            let resp2 = check_error(resp2).await?;
            let body = resp2.bytes().await?;
            let sliced = slice_body(body, offset as usize, length as usize);
            return Ok(Box::new(std::io::Cursor::new(sliced)));
        }
        let resp = check_error(resp).await?;
        let body = resp.bytes().await?;
        let sliced = slice_body(body, offset as usize, length as usize);
        Ok(Box::new(std::io::Cursor::new(sliced)))
    }

    pub async fn resume_upload(
        &self,
        path: &str,
        reader: Box<dyn SeekableReader>,
        total_size: i64,
    ) -> Result<(), Drive9Error> {
        let meta = self.query_upload(path).await?;
        let sync_reader = Arc::new(SyncReader::new(reader));
        let checksums =
            compute_part_checksums(Arc::clone(&sync_reader), total_size, PART_SIZE).await?;
        let plan = self.request_resume(&meta.upload_id, &checksums).await?;
        if plan.parts.is_empty() {
            return self.complete_upload(&plan.upload_id).await;
        }
        self.upload_missing_parts(&plan, Arc::clone(&sync_reader), meta.parts_total)
            .await?;
        self.complete_upload(&plan.upload_id).await
    }

    // ------------------------------------------------------------------
    // v1 upload internals
    // ------------------------------------------------------------------

    async fn initiate_upload(
        &self,
        path: &str,
        size: i64,
        checksums: &[String],
        expected_revision: i64,
    ) -> Result<UploadPlan, Drive9Error> {
        match self
            .initiate_upload_by_body(path, size, checksums, expected_revision)
            .await
        {
            Ok(p) => return Ok(p),
            Err((status, err)) => {
                if status == 404 || status == 405 {
                    return self
                        .initiate_upload_legacy(path, size, checksums, expected_revision)
                        .await;
                }
                if status == 400 && err.to_lowercase().contains("unknown upload action") {
                    return self
                        .initiate_upload_legacy(path, size, checksums, expected_revision)
                        .await;
                }
                return Err(Drive9Error::Status {
                    status_code: status,
                    message: err,
                });
            }
        }
    }

    async fn initiate_upload_by_body(
        &self,
        path: &str,
        size: i64,
        checksums: &[String],
        expected_revision: i64,
    ) -> Result<UploadPlan, (u16, String)> {
        let mut payload = json!({
            "path": path,
            "total_size": size,
            "part_checksums": checksums,
        });
        if expected_revision >= 0 {
            payload["expected_revision"] = json!(expected_revision);
        }
        let resp = self
            .http
            .post(format!("{}/v1/uploads/initiate", self.base_url))
            .headers(self.auth_headers())
            .header(CONTENT_TYPE, "application/json")
            .body(payload.to_string())
            .send()
            .await
            .map_err(|e| (0u16, e.to_string()))?;
        let status = resp.status().as_u16();
        if status == 202 {
            return resp
                .json::<UploadPlan>()
                .await
                .map_err(|e| (0u16, e.to_string()));
        }
        let text = resp.text().await.unwrap_or_default();
        Err((status, text))
    }

    async fn initiate_upload_legacy(
        &self,
        path: &str,
        size: i64,
        checksums: &[String],
        expected_revision: i64,
    ) -> Result<UploadPlan, Drive9Error> {
        let mut headers = self.auth_headers();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        headers.insert(
            "X-Dat9-Content-Length",
            HeaderValue::from_str(&size.to_string()).unwrap(),
        );
        if !checksums.is_empty() {
            headers.insert(
                "X-Dat9-Part-Checksums",
                HeaderValue::from_str(&checksums.join(",")).unwrap(),
            );
        }
        if expected_revision >= 0 {
            headers.insert(
                "X-Dat9-Expected-Revision",
                HeaderValue::from_str(&expected_revision.to_string()).unwrap(),
            );
        }
        let resp = self
            .http
            .put(self.fs_url(path))
            .headers(headers)
            .send()
            .await?;
        let resp = check_error(resp).await?;
        Ok(resp.json().await?)
    }

    async fn upload_parts_v1(
        &self,
        plan: &UploadPlan,
        reader: Arc<SyncReader>,
    ) -> Result<(), Drive9Error> {
        let mut std_part_size = plan.part_size;
        if std_part_size <= 0 && !plan.parts.is_empty() {
            std_part_size = plan.parts[0].size;
        }
        if std_part_size <= 0 {
            std_part_size = PART_SIZE;
        }
        let max_concurrency = upload_parallelism(std_part_size);
        let sem = Arc::new(Semaphore::new(max_concurrency));
        let mut tasks = vec![];
        for part in &plan.parts {
            let permit = Arc::clone(&sem).acquire_owned().await.unwrap();
            let client = self.clone();
            let r = Arc::clone(&reader);
            let p = part.clone();
            let offset = (p.number - 1) as i64 * std_part_size;
            let task = tokio::spawn(async move {
                let _permit = permit;
                let data = tokio::task::spawn_blocking(move || {
                    r.seek_read(offset as u64, p.size as usize)
                })
                .await
                .map_err(|e| Drive9Error::Other(format!("join error: {}", e)))?;
                let data = data.map_err(Drive9Error::Io)?;
                client.upload_one_part(&p, &data).await
            });
            tasks.push(task);
        }
        for t in tasks {
            t.await
                .map_err(|e| Drive9Error::Other(format!("task panicked: {}", e)))??;
        }
        self.complete_upload(&plan.upload_id).await
    }

    async fn upload_one_part(&self, part: &PartURL, data: &[u8]) -> Result<String, Drive9Error> {
        let checksum = part
            .checksum_crc32c
            .clone()
            .unwrap_or_else(|| compute_crc32c(data));
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-amz-checksum-crc32c",
            HeaderValue::from_str(&checksum).unwrap(),
        );
        if let Some(ref ph) = part.headers {
            for (k, v) in ph {
                if let Ok(hv) = HeaderValue::from_str(v.as_str().unwrap_or("")) {
                    if k.eq_ignore_ascii_case("host") {
                        continue;
                    }
                    headers.insert(
                        HeaderName::from_bytes(k.as_bytes()).unwrap_or(CONTENT_TYPE),
                        hv,
                    );
                }
            }
        }
        let resp = self
            .http
            .put(&part.url)
            .headers(headers)
            .body(data.to_vec())
            .send()
            .await?;
        let resp = check_error(resp).await?;
        Ok(resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string())
    }

    pub(crate) async fn complete_upload(&self, upload_id: &str) -> Result<(), Drive9Error> {
        let resp = self
            .http
            .post(format!(
                "{}/v1/uploads/{}/complete",
                self.base_url, upload_id
            ))
            .headers(self.auth_headers())
            .send()
            .await?;
        check_error(resp).await?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // v2 upload internals
    // ------------------------------------------------------------------

    pub(crate) async fn initiate_upload_v2(
        &self,
        path: &str,
        size: i64,
        expected_revision: i64,
    ) -> Result<UploadPlanV2, Drive9Error> {
        let mut payload = json!({
            "path": path,
            "total_size": size,
        });
        if expected_revision >= 0 {
            payload["expected_revision"] = json!(expected_revision);
        }
        let resp = self
            .http
            .post(format!("{}/v2/uploads/initiate", self.base_url))
            .headers(self.auth_headers())
            .header(CONTENT_TYPE, "application/json")
            .body(payload.to_string())
            .send()
            .await?;
        if resp.status().as_u16() == 404 {
            return Err(Drive9Error::Other(
                "v2 upload API not available".to_string(),
            ));
        }
        let resp = check_error(resp).await?;
        Ok(resp.json().await?)
    }

    async fn upload_parts_v2(
        &self,
        plan: &UploadPlanV2,
        reader: Arc<SyncReader>,
    ) -> Result<Vec<CompletePart>, Drive9Error> {
        let part_size = plan.part_size;
        let total_parts = plan.total_parts;
        let upload_id = plan.upload_id.clone();
        let parallelism = upload_parallelism(part_size);

        let mut presigned = vec![];
        let batch_size = parallelism as i32;
        let mut start = 1i32;
        while start <= total_parts {
            let end = (start + batch_size - 1).min(total_parts);
            let batch = self.presign_batch(&upload_id, start, end).await?;
            presigned.extend(batch);
            start = end + 1;
        }

        let sem = Arc::new(Semaphore::new(parallelism));
        let mut tasks = vec![];
        let results: Vec<Option<CompletePart>> = vec![None; total_parts as usize];
        let results_arc = Arc::new(tokio::sync::Mutex::new(results));

        for pp in presigned {
            let permit = Arc::clone(&sem).acquire_owned().await.unwrap();
            let client = self.clone();
            let r = Arc::clone(&reader);
            let res = Arc::clone(&results_arc);
            let psize = part_size;
            let uid = upload_id.clone();
            let task = tokio::spawn(async move {
                let _permit = permit;
                let offset = (pp.number - 1) as i64 * psize;
                let data = tokio::task::spawn_blocking(move || {
                    r.seek_read(offset as u64, pp.size as usize)
                })
                .await
                .map_err(|e| Drive9Error::Other(format!("join error: {}", e)))?;
                let data = data.map_err(Drive9Error::Io)?;
                let etag = client.upload_one_part_v2(&uid, &pp, &data).await?;
                res.lock().await[pp.number as usize - 1] = Some(CompletePart {
                    number: pp.number,
                    etag,
                });
                Ok::<(), Drive9Error>(())
            });
            tasks.push(task);
        }
        for t in tasks {
            t.await
                .map_err(|e| Drive9Error::Other(format!("task panicked: {}", e)))??;
        }

        let guard = results_arc.lock().await;
        Ok(guard.iter().filter_map(|x| x.clone()).collect())
    }

    async fn presign_batch(
        &self,
        upload_id: &str,
        start: i32,
        end: i32,
    ) -> Result<Vec<PresignedPart>, Drive9Error> {
        let entries: Vec<_> = (start..=end).map(|i| json!({"part_number": i})).collect();
        let resp = self
            .http
            .post(format!(
                "{}/v2/uploads/{}/presign-batch",
                self.base_url, upload_id
            ))
            .headers(self.auth_headers())
            .header(CONTENT_TYPE, "application/json")
            .json(&json!({"parts": entries}))
            .send()
            .await?;
        let resp = check_error(resp).await?;
        let result: serde_json::Value = resp.json().await?;
        Ok(serde_json::from_value(
            result.get("parts").cloned().unwrap_or(json!([])),
        )?)
    }

    pub(crate) async fn upload_one_part_v2(
        &self,
        upload_id: &str,
        part: &PresignedPart,
        data: &[u8],
    ) -> Result<String, Drive9Error> {
        let mut headers = HeaderMap::new();
        if let Some(ref ph) = part.headers {
            for (k, v) in ph {
                if let Ok(hv) = HeaderValue::from_str(v.as_str().unwrap_or("")) {
                    if k.eq_ignore_ascii_case("host") {
                        continue;
                    }
                    headers.insert(
                        HeaderName::from_bytes(k.as_bytes()).unwrap_or(CONTENT_TYPE),
                        hv,
                    );
                }
            }
        }
        let resp = self
            .http
            .put(&part.url)
            .headers(headers)
            .body(data.to_vec())
            .send()
            .await?;
        if resp.status().as_u16() == 403 {
            let fresh = self.presign_one_part(upload_id, part.number).await?;
            let mut headers = HeaderMap::new();
            if let Some(ref ph) = fresh.headers {
                for (k, v) in ph {
                    if let Ok(hv) = HeaderValue::from_str(v.as_str().unwrap_or("")) {
                        if k.eq_ignore_ascii_case("host") {
                            continue;
                        }
                        headers.insert(
                            HeaderName::from_bytes(k.as_bytes()).unwrap_or(CONTENT_TYPE),
                            hv,
                        );
                    }
                }
            }
            let resp = self
                .http
                .put(&fresh.url)
                .headers(headers)
                .body(data.to_vec())
                .send()
                .await?;
            let resp = check_error(resp).await?;
            return Ok(resp
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string());
        }
        let resp = check_error(resp).await?;
        Ok(resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string())
    }

    pub(crate) async fn presign_one_part(
        &self,
        upload_id: &str,
        part_number: i32,
    ) -> Result<PresignedPart, Drive9Error> {
        let resp = self
            .http
            .post(format!(
                "{}/v2/uploads/{}/presign",
                self.base_url, upload_id
            ))
            .headers(self.auth_headers())
            .header(CONTENT_TYPE, "application/json")
            .json(&json!({"part_number": part_number}))
            .send()
            .await?;
        let resp = check_error(resp).await?;
        Ok(resp.json().await?)
    }

    pub(crate) async fn complete_upload_v2(
        &self,
        upload_id: &str,
        parts: &[CompletePart],
    ) -> Result<(), Drive9Error> {
        let resp = self
            .http
            .post(format!(
                "{}/v2/uploads/{}/complete",
                self.base_url, upload_id
            ))
            .headers(self.auth_headers())
            .header(CONTENT_TYPE, "application/json")
            .json(&json!({"parts": parts}))
            .send()
            .await?;
        check_error(resp).await?;
        Ok(())
    }

    pub(crate) async fn abort_upload_v2(&self, upload_id: &str) -> Result<(), Drive9Error> {
        let resp = self
            .http
            .post(format!("{}/v2/uploads/{}/abort", self.base_url, upload_id))
            .headers(self.auth_headers())
            .send()
            .await?;
        check_error(resp).await?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // resume helpers
    // ------------------------------------------------------------------

    async fn query_upload(&self, path: &str) -> Result<UploadMeta, Drive9Error> {
        let resp = self
            .http
            .get(format!(
                "{}/v1/uploads?path={}&status=UPLOADING",
                self.base_url,
                urlencoding::encode(path)
            ))
            .headers(self.auth_headers())
            .send()
            .await?;
        let resp = check_error(resp).await?;
        let result: serde_json::Value = resp.json().await?;
        let uploads: Vec<UploadMeta> =
            serde_json::from_value(result.get("uploads").cloned().unwrap_or(json!([])))?;
        uploads
            .into_iter()
            .next()
            .ok_or_else(|| Drive9Error::Other(format!("no active upload for {}", path)))
    }

    async fn request_resume(
        &self,
        upload_id: &str,
        checksums: &[String],
    ) -> Result<UploadPlan, Drive9Error> {
        match self.request_resume_by_body(upload_id, checksums).await {
            Ok(p) => return Ok(p),
            Err((status, err)) => {
                if status == 400
                    && err
                        .to_lowercase()
                        .contains("missing x-dat9-part-checksums header")
                {
                    return self.request_resume_legacy(upload_id, checksums).await;
                }
                return Err(Drive9Error::Status {
                    status_code: status,
                    message: err,
                });
            }
        }
    }

    async fn request_resume_by_body(
        &self,
        upload_id: &str,
        checksums: &[String],
    ) -> Result<UploadPlan, (u16, String)> {
        let resp = self
            .http
            .post(format!("{}/v1/uploads/{}/resume", self.base_url, upload_id))
            .headers(self.auth_headers())
            .header(CONTENT_TYPE, "application/json")
            .json(&json!({"part_checksums": checksums}))
            .send()
            .await
            .map_err(|e| (0u16, e.to_string()))?;
        let status = resp.status().as_u16();
        if status == 410 {
            let text = resp.text().await.unwrap_or_default();
            return Err((status, text));
        }
        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err((status, text));
        }
        resp.json::<UploadPlan>()
            .await
            .map_err(|e| (0u16, e.to_string()))
    }

    async fn request_resume_legacy(
        &self,
        upload_id: &str,
        checksums: &[String],
    ) -> Result<UploadPlan, Drive9Error> {
        let mut headers = self.auth_headers();
        if !checksums.is_empty() {
            headers.insert(
                "X-Dat9-Part-Checksums",
                HeaderValue::from_str(&checksums.join(",")).unwrap(),
            );
        }
        let resp = self
            .http
            .post(format!("{}/v1/uploads/{}/resume", self.base_url, upload_id))
            .headers(headers)
            .send()
            .await?;
        if resp.status().as_u16() == 410 {
            return Err(Drive9Error::Other(format!(
                "upload {} has expired",
                upload_id
            )));
        }
        let resp = check_error(resp).await?;
        Ok(resp.json().await?)
    }

    async fn upload_missing_parts(
        &self,
        plan: &UploadPlan,
        reader: Arc<SyncReader>,
        _total_parts: i32,
    ) -> Result<(), Drive9Error> {
        let mut std_part_size = plan.part_size;
        if std_part_size <= 0 {
            std_part_size = PART_SIZE;
        }
        let max_concurrency = upload_parallelism(std_part_size);
        let sem = Arc::new(Semaphore::new(max_concurrency));
        let mut tasks = vec![];
        for part in &plan.parts {
            let permit = Arc::clone(&sem).acquire_owned().await.unwrap();
            let client = self.clone();
            let r = Arc::clone(&reader);
            let p = part.clone();
            let offset = (p.number - 1) as i64 * std_part_size;
            let task = tokio::spawn(async move {
                let _permit = permit;
                let data = tokio::task::spawn_blocking(move || {
                    r.seek_read(offset as u64, p.size as usize)
                })
                .await
                .map_err(|e| Drive9Error::Other(format!("join error: {}", e)))?;
                let data = data.map_err(Drive9Error::Io)?;
                client.upload_one_part(&p, &data).await
            });
            tasks.push(task);
        }
        for t in tasks {
            t.await
                .map_err(|e| Drive9Error::Other(format!("task panicked: {}", e)))??;
        }
        Ok(())
    }
}

async fn compute_part_checksums(
    reader: Arc<SyncReader>,
    total_size: i64,
    part_size: i64,
) -> Result<Vec<String>, Drive9Error> {
    let parts = calc_parts(total_size, part_size);
    let part_count = parts.len();
    if part_count == 0 {
        return Ok(vec![]);
    }
    let workers = checksum_parallelism(part_size, part_count);
    let mut handles = vec![];
    let mut per_worker = vec![vec![]; workers];
    for (i, p) in parts.into_iter().enumerate() {
        per_worker[i % workers].push(p);
    }
    for chunk in per_worker {
        let r = Arc::clone(&reader);
        let psize = part_size as usize;
        let handle = tokio::task::spawn_blocking(move || {
            let mut out = Vec::with_capacity(chunk.len());
            for (number, size) in chunk {
                let data =
                    r.seek_read(((number - 1) as i64 * psize as i64) as u64, size as usize)?;
                out.push((number, compute_crc32c(&data)));
            }
            Ok::<Vec<(i32, String)>, std::io::Error>(out)
        });
        handles.push(handle);
    }
    let mut results: Vec<Option<String>> = vec![None; part_count];
    for h in handles {
        let chunk_results = h
            .await
            .map_err(|e| Drive9Error::Other(format!("join error: {}", e)))?;
        let chunk_results = chunk_results.map_err(Drive9Error::Io)?;
        for (number, checksum) in chunk_results {
            results[number as usize - 1] = Some(checksum);
        }
    }
    results
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| Drive9Error::Other("checksum computation failed".to_string()))
}

fn slice_body(body: Bytes, offset: usize, length: usize) -> Vec<u8> {
    let end = (offset + length).min(body.len());
    if offset >= body.len() {
        return vec![];
    }
    body[offset..end].to_vec()
}
