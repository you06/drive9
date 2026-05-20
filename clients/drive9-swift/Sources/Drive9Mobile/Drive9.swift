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
}
