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
}
