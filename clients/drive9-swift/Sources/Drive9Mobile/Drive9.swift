import Foundation

/// Idiomatic Swift facade over the UniFFI-generated bindings.
///
/// Each instance owns a dedicated multi-thread Tokio runtime on the Rust side.
/// Share a single instance across the app rather than constructing per-request.
public final class Drive9Client: @unchecked Sendable {
    private let inner: Drive9MobileClient

    public init(baseUrl: String, apiKey: String) {
        self.inner = Drive9MobileClient(baseUrl: baseUrl, apiKey: apiKey)
    }

    public func write(path: String, data: Data, expectedRevision: Int64? = nil) async throws {
        let inner = self.inner
        try await Task.detached(priority: .userInitiated) {
            try inner.write(path: path, data: data, expectedRevision: expectedRevision)
        }.value
    }

    public func read(path: String) async throws -> Data {
        let inner = self.inner
        return try await Task.detached(priority: .userInitiated) {
            try inner.read(path: path)
        }.value
    }

    public func list(path: String) async throws -> [Drive9FileInfo] {
        let inner = self.inner
        return try await Task.detached(priority: .userInitiated) {
            try inner.list(path: path)
        }.value
    }

    public func stat(path: String) async throws -> Drive9StatResult {
        let inner = self.inner
        return try await Task.detached(priority: .userInitiated) {
            try inner.stat(path: path)
        }.value
    }

    public func delete(path: String) async throws {
        let inner = self.inner
        try await Task.detached(priority: .userInitiated) {
            try inner.delete(path: path)
        }.value
    }

    public func copy(srcPath: String, dstPath: String) async throws {
        let inner = self.inner
        try await Task.detached(priority: .userInitiated) {
            try inner.copy(srcPath: srcPath, dstPath: dstPath)
        }.value
    }

    public func rename(oldPath: String, newPath: String) async throws {
        let inner = self.inner
        try await Task.detached(priority: .userInitiated) {
            try inner.rename(oldPath: oldPath, newPath: newPath)
        }.value
    }

    public func mkdir(path: String) async throws {
        let inner = self.inner
        try await Task.detached(priority: .userInitiated) {
            try inner.mkdir(path: path)
        }.value
    }

    /// Search by content. `limit` of 0 lets the server pick the default.
    public func grep(query: String, pathPrefix: String, limit: Int32 = 0) async throws -> [Drive9SearchResult] {
        let inner = self.inner
        return try await Task.detached(priority: .userInitiated) {
            try inner.grep(query: query, pathPrefix: pathPrefix, limit: limit)
        }.value
    }

    /// Search by metadata. `params` is forwarded verbatim to the server; the
    /// facade does not interpret keys.
    public func find(pathPrefix: String, params: [String: String] = [:]) async throws -> [Drive9SearchResult] {
        let inner = self.inner
        return try await Task.detached(priority: .userInitiated) {
            try inner.find(pathPrefix: pathPrefix, params: params)
        }.value
    }

    /// Run a SQL query. Each row is a JSON-encoded string; parse with
    /// `JSONSerialization` or your preferred decoder on the caller side.
    public func sql(query: String) async throws -> [String] {
        let inner = self.inner
        return try await Task.detached(priority: .userInitiated) {
            try inner.sql(query: query)
        }.value
    }

    /// Open a streaming multipart upload. The returned
    /// ``Drive9StreamUpload`` receives parts incrementally. Phase 4A
    /// only exposes the object-based API; idiomatic
    /// `AsyncSequence` wrappers are Phase 4B.
    public func newStreamUpload(
        remotePath: String,
        totalSize: Int64,
        expectedRevision: Int64? = nil
    ) async throws -> Drive9StreamUpload {
        let inner = self.inner
        return try await Task.detached(priority: .userInitiated) {
            inner.newStreamUpload(
                remotePath: remotePath,
                totalSize: totalSize,
                expectedRevision: expectedRevision
            )
        }.value
    }

    /// List vault secret names readable by the current api key / token.
    ///
    /// Mobile vault surface is intentionally read-only: admin operations
    /// (create/update/delete secret, issue/revoke token, audit) are not
    /// exposed via FFI. Token issuance happens elsewhere; the resulting
    /// scoped token is what the client constructor's `apiKey` carries.
    public func vaultListReadableSecrets() async throws -> [String] {
        let inner = self.inner
        return try await Task.detached(priority: .userInitiated) {
            try inner.vaultListReadableSecrets()
        }.value
    }

    /// Read a single field from a vault secret. The value is returned
    /// exactly as the server delivered it — JSON-looking strings are
    /// NOT parsed or re-encoded.
    public func vaultReadSecretField(name: String, field: String) async throws -> String {
        let inner = self.inner
        return try await Task.detached(priority: .userInitiated) {
            try inner.vaultReadSecretField(name: name, field: field)
        }.value
    }

    /// Stream a local file to a remote path. Progress reports come from
    /// completed part PUTs (multipart) or the success transition of the
    /// single PUT (small file); cancelled or failed uploads never emit
    /// a `(total, total)` event. A cancelled upload surfaces as
    /// `Drive9Exception` with `code = "cancelled"`.
    public func uploadFile(
        localPath: String,
        remotePath: String,
        expectedRevision: Int64? = nil,
        progress: Drive9ProgressListener? = nil,
        cancel: Drive9CancelToken? = nil
    ) async throws {
        let inner = self.inner
        try await Task.detached(priority: .userInitiated) {
            try inner.uploadFile(
                localPath: localPath,
                remotePath: remotePath,
                expectedRevision: expectedRevision,
                progress: progress,
                cancel: cancel
            )
        }.value
    }

    /// Stream a remote file to a local path. Progress is precise (bytes
    /// transferred equal bytes already on disk). Cancellation is
    /// cooperative via ``Drive9CancelToken``; on cancel or any failure the
    /// partial local file is deleted.
    public func downloadFile(
        remotePath: String,
        localPath: String,
        progress: Drive9ProgressListener? = nil,
        cancel: Drive9CancelToken? = nil
    ) async throws {
        let inner = self.inner
        try await Task.detached(priority: .userInitiated) {
            try inner.downloadFile(
                remotePath: remotePath,
                localPath: localPath,
                progress: progress,
                cancel: cancel
            )
        }.value
    }

    /// Patch specific parts of a remote file using bytes read from
    /// `localPath`. `dirtyParts` are 1-based part numbers known to the
    /// caller. Cancel / progress are not supported in this iteration; see
    /// Phase 2B-2.
    public func patchFileParts(
        localPath: String,
        remotePath: String,
        dirtyParts: [Int32],
        newSize: Int64,
        partSize: Int64,
        expectedRevision: Int64? = nil
    ) async throws {
        let inner = self.inner
        try await Task.detached(priority: .userInitiated) {
            try inner.patchFileParts(
                localPath: localPath,
                remotePath: remotePath,
                dirtyParts: dirtyParts,
                newSize: newSize,
                partSize: partSize,
                expectedRevision: expectedRevision
            )
        }.value
    }
}
