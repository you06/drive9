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
}

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

private fun uniffi.drive9_mobile_core.Drive9FileInfo.toFacade(): Drive9FileInfo =
    Drive9FileInfo(name = name, size = size, isDir = isDir, mtimeUnix = mtimeUnix)

private fun uniffi.drive9_mobile_core.Drive9StatResult.toFacade(): Drive9StatResult =
    Drive9StatResult(size = size, isDir = isDir, revision = revision, mtimeUnix = mtimeUnix)

/**
 * Flat exception surface re-exported so consumers do not need to import from
 * the `uniffi.drive9_mobile_core` package. The underlying type is unchanged.
 */
public typealias Drive9Exception = uniffi.drive9_mobile_core.Drive9Exception
