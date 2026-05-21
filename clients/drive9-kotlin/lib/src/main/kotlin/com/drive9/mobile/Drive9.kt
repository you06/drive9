package com.drive9.mobile

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.flow
import kotlinx.coroutines.withContext
import uniffi.drive9_mobile_core.Drive9MobileClient as RawDrive9Client

/**
 * Idiomatic Kotlin facade over the UniFFI-generated bindings.
 *
 * Each instance owns a dedicated multi-thread Tokio runtime on the Rust side.
 * Share a single instance across the app rather than constructing per-request.
 */
public class Drive9Client(baseUrl: String, apiKey: String) {
    private val inner: RawDrive9Client = RawDrive9Client(baseUrl, apiKey)

    public suspend fun write(
        path: String,
        data: ByteArray,
        expectedRevision: Long? = null,
    ): Unit = withContext(Dispatchers.IO) {
        inner.write(path, data, expectedRevision)
    }

    public suspend fun read(path: String): ByteArray = withContext(Dispatchers.IO) {
        inner.read(path)
    }

    public suspend fun list(path: String): List<Drive9FileInfo> = withContext(Dispatchers.IO) {
        inner.list(path).map { it.toFacade() }
    }

    public suspend fun stat(path: String): Drive9StatResult = withContext(Dispatchers.IO) {
        inner.stat(path).toFacade()
    }

    public suspend fun delete(path: String): Unit = withContext(Dispatchers.IO) {
        inner.delete(path)
    }

    public suspend fun copy(srcPath: String, dstPath: String): Unit =
        withContext(Dispatchers.IO) { inner.copy(srcPath, dstPath) }

    public suspend fun rename(oldPath: String, newPath: String): Unit =
        withContext(Dispatchers.IO) { inner.rename(oldPath, newPath) }

    public suspend fun mkdir(path: String): Unit = withContext(Dispatchers.IO) {
        inner.mkdir(path)
    }

    /**
     * Search by content. `limit` of 0 lets the server pick the default.
     */
    public suspend fun grep(
        query: String,
        pathPrefix: String,
        limit: Int = 0,
    ): List<Drive9SearchResult> = withContext(Dispatchers.IO) {
        inner.grep(query, pathPrefix, limit).map { it.toFacade() }
    }

    /**
     * Search by metadata. `params` is forwarded verbatim to the server; the
     * facade does not interpret keys.
     */
    public suspend fun find(
        pathPrefix: String,
        params: Map<String, String> = emptyMap(),
    ): List<Drive9SearchResult> = withContext(Dispatchers.IO) {
        inner.find(pathPrefix, params).map { it.toFacade() }
    }

    /**
     * Run a SQL query. Each row is a JSON-encoded string; parse with your
     * preferred JSON library on the caller side.
     */
    public suspend fun sql(query: String): List<String> = withContext(Dispatchers.IO) {
        inner.sql(query)
    }

    /**
     * Stream a remote file as a Kotlin [Flow] of [ByteArray] chunks.
     *
     * Backpressure is genuine: each chunk is `emit`ted, which suspends
     * the producer coroutine until the downstream collector resumes.
     * Cancellation of the collecting coroutine propagates as a
     * `CancellationException` which the `finally` block uses to call
     * `Drive9StreamDownload.close` — releasing the underlying socket
     * once the in-flight read settles. For an immediate mid-read
     * abort, also pass a [Drive9CancelToken] and call `cancel()` on
     * it: the underlying `read_chunk` is interrupted via
     * `tokio::select!`.
     */
    public fun downloadFlow(
        remotePath: String,
        cancel: Drive9CancelToken? = null,
    ): Flow<ByteArray> = flow {
        val reader = inner.newStreamDownload(remotePath, cancel)
        try {
            while (true) {
                val chunk = withContext(Dispatchers.IO) { reader.readChunk() } ?: break
                emit(chunk)
            }
        } finally {
            // close() is idempotent; safe whether we exited via EOF,
            // an exception, or coroutine cancellation.
            reader.closeStream()
        }
    }

    /**
     * Stream a [Flow] of [ByteArray] chunks into a remote multipart
     * upload. Each chunk becomes one server-side part (1-indexed in
     * emission order); after the Flow completes, the upload is
     * finalized with `complete(lastPartNum, empty)`. Callers are
     * responsible for emitting chunks at the server-chosen part size
     * — passing chunks of a different size will fail at write_part
     * or complete time.
     *
     * Zero-chunk source: the upload is `abort()`ed and the call
     * throws a Drive9Exception with `code = "other"`. If the source
     * Flow throws or its coroutine is cancelled mid-stream, the
     * upload is `abort()`ed before the exception propagates.
     */
    public suspend fun uploadFlow(
        remotePath: String,
        totalSize: Long,
        chunks: Flow<ByteArray>,
        expectedRevision: Long? = null,
    ) {
        val upload = inner.newStreamUpload(remotePath, totalSize, expectedRevision)
        var aborted = false
        suspend fun abortQuietly() {
            if (aborted) return
            aborted = true
            try {
                withContext(Dispatchers.IO) { upload.abort() }
            } catch (_: Throwable) {
                // Best-effort cleanup.
            }
        }
        try {
            // Phase 4C: ask the server for its part_size up front so we
            // can auto-rechunk the caller's source. This also forces
            // /v2/uploads/initiate to happen here, so even a
            // zero-chunk source now produces a wire-level /abort when
            // we tear the upload down.
            val partSize = withContext(Dispatchers.IO) { upload.partSize() }
            require(partSize > 0) { "server returned non-positive part_size=$partSize" }
            val partSizeInt = partSize.toInt()

            var pending = ByteArray(0)
            var nextPart = 1
            var anyChunkEmitted = false

            chunks.collect { chunk ->
                anyChunkEmitted = true
                // Append + drain: ensure the buffer never carries a
                // full part's worth of bytes — split out partSize
                // slices as soon as available. Worst-case buffer
                // length after this loop is partSize-1 bytes.
                pending = if (pending.isEmpty()) chunk else pending + chunk
                while (pending.size >= partSizeInt) {
                    val data = pending.copyOfRange(0, partSizeInt)
                    val n = nextPart++
                    withContext(Dispatchers.IO) { upload.writePart(n, data) }
                    pending = pending.copyOfRange(partSizeInt, pending.size)
                }
            }

            if (!anyChunkEmitted) {
                abortQuietly()
                throw uniffi.drive9_mobile_core.Drive9Exception.Drive9(
                    code = "other",
                    statusCode = null,
                    detail = "uploadFlow: source produced no chunks",
                    serverRevision = null,
                )
            }

            // Finalize. Two paths:
            // - pending is non-empty: that's the last (short) part;
            //   complete(nextPart, pending) uploads it and finalizes.
            // - pending is empty (source aligned to partSize): just
            //   finalize without an extra PUT via complete(nextPart-1,
            //   empty). The PUT counter on the server side must equal
            //   the number of full parts we already wrote.
            if (pending.isNotEmpty()) {
                val finalPart = nextPart
                val finalData = pending
                withContext(Dispatchers.IO) { upload.complete(finalPart, finalData) }
            } else {
                val lastPart = nextPart - 1
                withContext(Dispatchers.IO) { upload.complete(lastPart, byteArrayOf()) }
            }
        } catch (e: Throwable) {
            abortQuietly()
            throw e
        } finally {
            upload.close()
        }
    }

    /**
     * Open a streaming multipart upload. The returned
     * [Drive9StreamUpload] receives parts incrementally. Phase 4A only
     * exposes the object-based API; idiomatic [kotlinx.coroutines.flow.Flow]
     * wrappers are Phase 4B.
     */
    public suspend fun newStreamUpload(
        remotePath: String,
        totalSize: Long,
        expectedRevision: Long? = null,
    ): Drive9StreamUpload = withContext(Dispatchers.IO) {
        inner.newStreamUpload(remotePath, totalSize, expectedRevision)
    }

    /**
     * List vault secret names readable by the current API key / token.
     *
     * Mobile vault surface is intentionally read-only: admin operations
     * (create/update/delete secret, issue/revoke token, audit) are not
     * exposed via FFI. Token issuance happens elsewhere; the resulting
     * scoped token is what the client constructor's `apiKey` carries.
     *
     * Authorization failures and missing secrets surface through the
     * existing [Drive9Exception.Drive9] with `code = "http_status"`
     * and `statusCode` set to 401/403/404 as appropriate.
     */
    public suspend fun vaultListReadableSecrets(): List<String> =
        withContext(Dispatchers.IO) { inner.vaultListReadableSecrets() }

    /**
     * Read a single field from a vault secret. The value is returned
     * exactly as the server delivered it — JSON-looking strings are
     * NOT parsed or re-encoded.
     */
    public suspend fun vaultReadSecretField(name: String, field: String): String =
        withContext(Dispatchers.IO) { inner.vaultReadSecretField(name, field) }

    /**
     * Stream a local file to a remote path. Progress is reported from
     * the v2 multipart path only after each individual part PUT completes
     * (so transferred bytes never exceed real on-server bytes); the
     * small-file single-PUT path emits `(0, total)` before the PUT and
     * `(total, total)` after success. Cancelled or failed uploads never
     * emit a `(total, total)` event.
     *
     * Cancellation via [Drive9CancelToken] is cooperative; in-flight
     * part PUTs are allowed to drain so server-side multipart state
     * stays consistent, then `abort_upload_v2` is invoked. A cancelled
     * upload surfaces as [Drive9Exception.Drive9] with `code = "cancelled"`.
     */
    public suspend fun uploadFile(
        localPath: String,
        remotePath: String,
        expectedRevision: Long? = null,
        progress: Drive9ProgressListener? = null,
        token: Drive9CancelToken? = null,
    ): Unit = withContext(Dispatchers.IO) {
        inner.uploadFile(localPath, remotePath, expectedRevision, progress, token)
    }

    /**
     * Stream a remote file to a local path. Progress is precise (bytes
     * transferred equal bytes already on disk). Cancellation is cooperative
     * via [Drive9CancelToken]; on cancel or any failure the partial local
     * file is deleted.
     *
     * Caller must hold a reference to [progress] and [token] for the
     * lifetime of this call; the bindings keep raw references across FFI.
     */
    public suspend fun downloadFile(
        remotePath: String,
        localPath: String,
        progress: Drive9ProgressListener? = null,
        token: Drive9CancelToken? = null,
    ): Unit = withContext(Dispatchers.IO) {
        inner.downloadFile(remotePath, localPath, progress, token)
    }

    /**
     * Patch specific parts of a remote file using bytes read from
     * [localPath]. [dirtyParts] are 1-based part numbers known to the caller
     * (e.g. from a local diff). The wrapper does not support cancel /
     * progress for patch in this iteration; see Phase 2B-2.
     */
    public suspend fun patchFileParts(
        localPath: String,
        remotePath: String,
        dirtyParts: List<Int>,
        newSize: Long,
        partSize: Long,
        expectedRevision: Long? = null,
    ): Unit = withContext(Dispatchers.IO) {
        inner.patchFileParts(localPath, remotePath, dirtyParts, newSize, partSize, expectedRevision)
    }
}

public typealias Drive9CancelToken = uniffi.drive9_mobile_core.Drive9CancelToken
public typealias Drive9ProgressListener = uniffi.drive9_mobile_core.Drive9ProgressListener
public typealias Drive9StreamUpload = uniffi.drive9_mobile_core.Drive9StreamUpload
public typealias Drive9StreamDownload = uniffi.drive9_mobile_core.Drive9StreamDownload

public data class Drive9FileInfo(
    val name: String,
    val size: Long,
    val isDir: Boolean,
    val mtimeUnix: Long?,
)

public data class Drive9StatResult(
    val size: Long,
    val isDir: Boolean,
    val revision: Long,
    val mtimeUnix: Long?,
)

public data class Drive9SearchResult(
    val path: String,
    val name: String,
    val sizeBytes: Long,
    val score: Double?,
)

private fun uniffi.drive9_mobile_core.Drive9FileInfo.toFacade(): Drive9FileInfo =
    Drive9FileInfo(name = name, size = size, isDir = isDir, mtimeUnix = mtimeUnix)

private fun uniffi.drive9_mobile_core.Drive9StatResult.toFacade(): Drive9StatResult =
    Drive9StatResult(size = size, isDir = isDir, revision = revision, mtimeUnix = mtimeUnix)

private fun uniffi.drive9_mobile_core.Drive9SearchResult.toFacade(): Drive9SearchResult =
    Drive9SearchResult(path = path, name = name, sizeBytes = sizeBytes, score = score)

/**
 * Flat exception surface re-exported so consumers do not need to import from
 * the `uniffi.drive9_mobile_core` package. The underlying type is unchanged.
 */
public typealias Drive9Exception = uniffi.drive9_mobile_core.Drive9Exception
