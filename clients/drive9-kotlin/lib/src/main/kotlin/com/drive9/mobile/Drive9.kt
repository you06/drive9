package com.drive9.mobile

import kotlinx.coroutines.Dispatchers
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
