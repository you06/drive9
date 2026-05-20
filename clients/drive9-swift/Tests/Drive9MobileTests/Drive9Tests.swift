import XCTest
@testable import Drive9Mobile

/// Test helpers: shared state across server handlers running on the
/// MockHTTPServer thread.
final class ReceivedBody: @unchecked Sendable {
    private var body: Data = Data()
    private let lock = NSLock()
    func set(_ value: Data) { lock.lock(); body = value; lock.unlock() }
    func get() -> Data { lock.lock(); defer { lock.unlock() }; return body }
}

final class HitCounter: @unchecked Sendable {
    private var count = 0
    private let lock = NSLock()
    func bump() { lock.lock(); count += 1; lock.unlock() }
    func get() -> Int { lock.lock(); defer { lock.unlock() }; return count }
}

/// Captures every `onProgress` callback for assertion in tests. The Rust
/// side calls into Swift from a Tokio worker thread, so guard the buffer
/// with a lock.
final class RecordingProgressListener: Drive9ProgressListener, @unchecked Sendable {
    struct Update {
        let transferred: UInt64
        let total: UInt64
    }
    private var updates: [Update] = []
    private let lock = NSLock()

    func onProgress(transferred: UInt64, total: UInt64) {
        lock.lock()
        updates.append(Update(transferred: transferred, total: total))
        lock.unlock()
    }

    func snapshot() -> [Update] {
        lock.lock()
        defer { lock.unlock() }
        return updates
    }
}

/// Smoke tests against an in-process `MockHTTPServer`. They exercise the FFI
/// surface end-to-end (native lib load, runtime dispatch, error mapping)
/// without needing a real Drive9 backend.
final class Drive9Tests: XCTestCase {
    var server: MockHTTPServer!

    override func setUp() async throws {
        server = try MockHTTPServer()
        try server.start()
    }

    override func tearDown() async throws {
        server.stop()
        server = nil
    }

    func testWriteThenReadRoundtrip() async throws {
        server.route("PUT", "/v1/fs/hello.txt") { _ in
            MockResponse(status: 200, body: Data())
        }
        server.route("GET", "/v1/fs/hello.txt") { _ in
            MockResponse(status: 200, body: Data("hello swift".utf8))
        }

        let client = Drive9Client(baseUrl: server.baseURL, apiKey: "test-key")
        try await client.write(path: "/hello.txt", data: Data("hello swift".utf8))
        let data = try await client.read(path: "/hello.txt")
        XCTAssertEqual(data, Data("hello swift".utf8))
    }

    func testListReturnsEntries() async throws {
        server.route("GET", "/v1/fs/data/?list=1") { _ in
            let body = #"{"entries":[{"name":"a.txt","size":3,"isDir":false},{"name":"sub","size":0,"isDir":true}]}"#
            return MockResponse(status: 200, body: Data(body.utf8), contentType: "application/json")
        }

        let client = Drive9Client(baseUrl: server.baseURL, apiKey: "k")
        let entries = try await client.list(path: "/data/")
        XCTAssertEqual(entries.count, 2)
        XCTAssertEqual(entries[0].name, "a.txt")
        XCTAssertEqual(entries[0].size, 3)
        XCTAssertFalse(entries[0].isDir)
        XCTAssertTrue(entries[1].isDir)
    }

    func testStatReportsRevisionAndSize() async throws {
        server.route("HEAD", "/v1/fs/f.bin") { _ in
            MockResponse(
                status: 200,
                body: Data(),
                extraHeaders: [
                    "Content-Length": "42",
                    "X-Dat9-Revision": "9",
                ]
            )
        }

        let client = Drive9Client(baseUrl: server.baseURL, apiKey: "k")
        let s = try await client.stat(path: "/f.bin")
        XCTAssertEqual(s.size, 42)
        XCTAssertEqual(s.revision, 9)
        XCTAssertFalse(s.isDir)
    }

    func testDeleteSucceeds() async throws {
        server.route("DELETE", "/v1/fs/gone.txt") { _ in
            MockResponse(status: 204, body: Data())
        }

        let client = Drive9Client(baseUrl: server.baseURL, apiKey: "k")
        try await client.delete(path: "/gone.txt")
    }

    func testConflictPreservesServerRevision() async throws {
        server.route("PUT", "/v1/fs/r.txt") { _ in
            let body = #"{"error":"revision mismatch","server_revision":12}"#
            return MockResponse(status: 409, body: Data(body.utf8), contentType: "application/json")
        }

        let client = Drive9Client(baseUrl: server.baseURL, apiKey: "k")
        do {
            try await client.write(path: "/r.txt", data: Data("x".utf8), expectedRevision: 7)
            XCTFail("expected conflict")
        } catch let error as Drive9Exception {
            guard case let .Drive9(code, statusCode, _, serverRevision) = error else {
                XCTFail("unexpected variant: \(error)")
                return
            }
            XCTAssertEqual(code, "conflict")
            XCTAssertEqual(statusCode, 409)
            XCTAssertEqual(serverRevision, 12)
        }
    }

    func testCopyRenameMkdirSucceed() async throws {
        server.route("POST", "/v1/fs/dst.txt?copy") { _ in
            MockResponse(status: 200, body: Data())
        }
        server.route("POST", "/v1/fs/new.txt?rename") { _ in
            MockResponse(status: 200, body: Data())
        }
        server.route("POST", "/v1/fs/dir/?mkdir") { _ in
            MockResponse(status: 200, body: Data())
        }

        let client = Drive9Client(baseUrl: server.baseURL, apiKey: "k")
        try await client.copy(srcPath: "/src.txt", dstPath: "/dst.txt")
        try await client.rename(oldPath: "/old.txt", newPath: "/new.txt")
        try await client.mkdir(path: "/dir/")
    }

    func testGrepReturnsSearchResults() async throws {
        server.route("GET", "/v1/fs/?grep=hello&limit=3") { _ in
            let body = #"[{"path":"/a.txt","name":"a.txt","size_bytes":7,"score":0.5}]"#
            return MockResponse(status: 200, body: Data(body.utf8), contentType: "application/json")
        }

        let client = Drive9Client(baseUrl: server.baseURL, apiKey: "k")
        let hits = try await client.grep(query: "hello", pathPrefix: "/", limit: 3)
        XCTAssertEqual(hits.count, 1)
        XCTAssertEqual(hits[0].path, "/a.txt")
        XCTAssertEqual(hits[0].sizeBytes, 7)
        XCTAssertEqual(hits[0].score, 0.5)
    }

    func testFindForwardsParams() async throws {
        // HashMap iteration order is non-deterministic; the handler matches
        // path only and asserts each piece is present in the query string.
        server.routeAnyQuery("GET", "/v1/fs/data/") { request in
            XCTAssertTrue(request.query.contains("find="), "missing find=: \(request.query)")
            XCTAssertTrue(request.query.contains("type=file"), "missing type=file: \(request.query)")
            XCTAssertTrue(request.query.contains("limit=10"), "missing limit=10: \(request.query)")
            let body = #"[{"path":"/data/x.txt","name":"x.txt","size_bytes":1,"score":null}]"#
            return MockResponse(status: 200, body: Data(body.utf8), contentType: "application/json")
        }

        let client = Drive9Client(baseUrl: server.baseURL, apiKey: "k")
        let hits = try await client.find(pathPrefix: "/data/", params: ["type": "file", "limit": "10"])
        XCTAssertEqual(hits.count, 1)
        XCTAssertEqual(hits[0].path, "/data/x.txt")
    }

    func testSqlReturnsJsonStrings() async throws {
        server.route("POST", "/v1/sql") { _ in
            let body = #"[{"path":"/a.txt","size":10},{"path":"/b","size":0}]"#
            return MockResponse(status: 200, body: Data(body.utf8), contentType: "application/json")
        }

        let client = Drive9Client(baseUrl: server.baseURL, apiKey: "k")
        let rows = try await client.sql(query: "SELECT path, size FROM files")
        XCTAssertEqual(rows.count, 2)
        // Don't assume key order; parse each row and confirm a field.
        XCTAssertTrue(rows[0].contains("\"path\":\"/a.txt\""))
        XCTAssertTrue(rows[0].contains("\"size\":10"))
        XCTAssertTrue(rows[1].contains("\"path\":\"/b\""))
    }

    func testDownloadFileRoundtripWithProgress() async throws {
        let body = Data(repeating: UInt8(ascii: "a"), count: 200_000)
        server.route("HEAD", "/v1/fs/big.bin") { _ in
            MockResponse(
                status: 200,
                body: Data(),
                extraHeaders: [
                    "Content-Length": "\(body.count)",
                    "X-Dat9-Revision": "1",
                ]
            )
        }
        server.route("GET", "/v1/fs/big.bin") { _ in
            MockResponse(status: 200, body: body)
        }

        let dest = FileManager.default.temporaryDirectory
            .appendingPathComponent("drive9-swift-download-\(UUID().uuidString).bin")
        defer { try? FileManager.default.removeItem(at: dest) }

        let recorder = RecordingProgressListener()
        let client = Drive9Client(baseUrl: server.baseURL, apiKey: "k")
        try await client.downloadFile(
            remotePath: "/big.bin",
            localPath: dest.path,
            progress: recorder
        )

        XCTAssertEqual(try Data(contentsOf: dest), body)
        let updates = recorder.snapshot()
        XCTAssertGreaterThanOrEqual(updates.count, 2, "want at least start + finish")
        XCTAssertEqual(updates.first?.transferred, 0)
        XCTAssertEqual(updates.first?.total, 200_000)
        XCTAssertEqual(updates.last?.transferred, 200_000)
        XCTAssertEqual(updates.last?.total, 200_000)
        for i in 1..<updates.count {
            XCTAssertLessThanOrEqual(
                updates[i - 1].transferred,
                updates[i].transferred,
                "non-monotonic progress at index \(i)"
            )
        }
    }

    func testUploadFileSmallRoundtripWithProgress() async throws {
        let received = ReceivedBody()
        server.route("PUT", "/v1/fs/up.bin") { req in
            received.set(req.body)
            return MockResponse(status: 200, body: Data())
        }

        let local = FileManager.default.temporaryDirectory
            .appendingPathComponent("drive9-swift-upload-\(UUID().uuidString).bin")
        let payload = Data(repeating: UInt8(ascii: "x"), count: 100)
        try payload.write(to: local)
        defer { try? FileManager.default.removeItem(at: local) }

        let recorder = RecordingProgressListener()
        let client = Drive9Client(baseUrl: server.baseURL, apiKey: "k")
        try await client.uploadFile(
            localPath: local.path,
            remotePath: "/up.bin",
            progress: recorder
        )

        XCTAssertEqual(received.get(), payload)
        let updates = recorder.snapshot()
        XCTAssertEqual(updates.map { ($0.transferred, $0.total) }.map { "\($0)-\($1)" },
                       ["0-100", "100-100"])
    }

    func testUploadFileCancelBeforeReturnsCancelled() async throws {
        let putHits = HitCounter()
        server.route("PUT", "/v1/fs/up-cancel.bin") { _ in
            putHits.bump()
            return MockResponse(status: 200, body: Data())
        }

        let local = FileManager.default.temporaryDirectory
            .appendingPathComponent("drive9-swift-upload-cancel-\(UUID().uuidString).bin")
        try Data(repeating: UInt8(ascii: "y"), count: 100).write(to: local)
        defer { try? FileManager.default.removeItem(at: local) }

        let token = Drive9CancelToken()
        token.cancel()

        let client = Drive9Client(baseUrl: server.baseURL, apiKey: "k")
        do {
            try await client.uploadFile(
                localPath: local.path,
                remotePath: "/up-cancel.bin",
                cancel: token
            )
            XCTFail("expected cancellation")
        } catch let error as Drive9Exception {
            guard case let .Drive9(code, _, _, _) = error else {
                XCTFail("unexpected variant: \(error)")
                return
            }
            XCTAssertEqual(code, "cancelled")
            XCTAssertEqual(putHits.get(), 0, "PUT should not happen when cancel is pre-set")
        }
    }

    func testDownloadFileCancellationLeavesNoFileWhenDestinationDidNotExist() async throws {
        let body = Data(repeating: UInt8(ascii: "b"), count: 10_000)
        server.route("HEAD", "/v1/fs/cancel.bin") { _ in
            MockResponse(status: 200, body: Data(), extraHeaders: ["Content-Length": "\(body.count)"])
        }
        server.route("GET", "/v1/fs/cancel.bin") { _ in
            MockResponse(status: 200, body: body)
        }

        // Use a fresh subdirectory so we can also assert no leftover temp
        // file remains after the cancellation path runs.
        let parent = FileManager.default.temporaryDirectory
            .appendingPathComponent("drive9-swift-cancel-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: true)
        let dest = parent.appendingPathComponent("nonexistent.bin")
        defer { try? FileManager.default.removeItem(at: parent) }

        let token = Drive9CancelToken()
        token.cancel()

        let client = Drive9Client(baseUrl: server.baseURL, apiKey: "k")
        do {
            try await client.downloadFile(
                remotePath: "/cancel.bin",
                localPath: dest.path,
                progress: nil,
                cancel: token
            )
            XCTFail("expected cancellation")
        } catch let error as Drive9Exception {
            guard case let .Drive9(code, _, _, _) = error else {
                XCTFail("unexpected variant: \(error)")
                return
            }
            XCTAssertEqual(code, "cancelled")
            XCTAssertFalse(
                FileManager.default.fileExists(atPath: dest.path),
                "destination must not be created on cancel"
            )
            let leftover = try FileManager.default.contentsOfDirectory(at: parent, includingPropertiesForKeys: nil)
            XCTAssertEqual(leftover, [], "no leftover temp files expected")
        }
    }

    func testDownloadFilePreservesPreexistingDestinationOnFailure() async throws {
        let body = Data(repeating: UInt8(ascii: "b"), count: 10_000)
        server.route("HEAD", "/v1/fs/cancel.bin") { _ in
            MockResponse(status: 200, body: Data(), extraHeaders: ["Content-Length": "\(body.count)"])
        }
        server.route("GET", "/v1/fs/cancel.bin") { _ in
            MockResponse(status: 200, body: body)
        }

        let dest = FileManager.default.temporaryDirectory
            .appendingPathComponent("drive9-swift-preserve-\(UUID().uuidString).bin")
        let original = Data("do not overwrite me".utf8)
        try original.write(to: dest)
        defer { try? FileManager.default.removeItem(at: dest) }

        let token = Drive9CancelToken()
        token.cancel()

        let client = Drive9Client(baseUrl: server.baseURL, apiKey: "k")
        do {
            try await client.downloadFile(
                remotePath: "/cancel.bin",
                localPath: dest.path,
                progress: nil,
                cancel: token
            )
            XCTFail("expected cancellation")
        } catch let error as Drive9Exception {
            guard case let .Drive9(code, _, _, _) = error else {
                XCTFail("unexpected variant: \(error)")
                return
            }
            XCTAssertEqual(code, "cancelled")
            let after = try Data(contentsOf: dest)
            XCTAssertEqual(after, original, "pre-existing destination must be unchanged on failure")
        }
    }

    func testPatchFilePartsValidatesInputs() async throws {
        let local = FileManager.default.temporaryDirectory
            .appendingPathComponent("drive9-swift-patch-\(UUID().uuidString).bin")
        try Data("xx".utf8).write(to: local)
        defer { try? FileManager.default.removeItem(at: local) }

        let client = Drive9Client(baseUrl: server.baseURL, apiKey: "k")
        await assertPatchValidationError(client: client, localPath: local.path, newSize: -1, partSize: 100, dirtyParts: [1], wantToken: "new_size")
        await assertPatchValidationError(client: client, localPath: local.path, newSize: 100, partSize: 0, dirtyParts: [1], wantToken: "part_size")
        await assertPatchValidationError(client: client, localPath: local.path, newSize: 100, partSize: 100, dirtyParts: [0, 1], wantToken: "dirty_parts")
    }

    private func assertPatchValidationError(
        client: Drive9Client,
        localPath: String,
        newSize: Int64,
        partSize: Int64,
        dirtyParts: [Int32],
        wantToken: String,
        file: StaticString = #file,
        line: UInt = #line
    ) async {
        do {
            try await client.patchFileParts(
                localPath: localPath,
                remotePath: "/r",
                dirtyParts: dirtyParts,
                newSize: newSize,
                partSize: partSize
            )
            XCTFail("expected validation error", file: file, line: line)
        } catch let error as Drive9Exception {
            guard case let .Drive9(code, _, detail, _) = error else {
                XCTFail("unexpected variant: \(error)", file: file, line: line)
                return
            }
            XCTAssertEqual(code, "other", file: file, line: line)
            XCTAssertTrue(detail.contains(wantToken), "expected \(wantToken) in: \(detail)", file: file, line: line)
        } catch {
            XCTFail("unexpected error type: \(error)", file: file, line: line)
        }
    }

    func testStreamUploadHappyPathTwoParts() async throws {
        let uploadId = "u-stream-swift"
        let partSize: Int64 = 100
        server.route("POST", "/v2/uploads/initiate") { _ in
            let body = #"{"upload_id":"\#(uploadId)","key":"k","part_size":\#(partSize),"total_parts":2}"#
            return MockResponse(status: 200, body: Data(body.utf8), contentType: "application/json")
        }
        let baseURL = server.baseURL
        server.routeAnyQuery("POST", "/v2/uploads/\(uploadId)/presign") { request in
            let req = String(data: request.body, encoding: .utf8) ?? ""
            let partNum = req
                .components(separatedBy: "\"part_number\":")
                .last?
                .components(separatedBy: CharacterSet.decimalDigits.inverted)
                .first
                .flatMap { Int($0) } ?? 0
            let body = #"{"number":\#(partNum),"url":"\#(baseURL)/upload/\#(partNum)","size":\#(partSize)}"#
            return MockResponse(status: 200, body: Data(body.utf8), contentType: "application/json")
        }
        let putHits = HitCounter()
        server.route("PUT", "/upload/1") { _ in
            putHits.bump()
            return MockResponse(status: 200, body: Data(), extraHeaders: ["ETag": "e1"])
        }
        server.route("PUT", "/upload/2") { _ in
            putHits.bump()
            return MockResponse(status: 200, body: Data(), extraHeaders: ["ETag": "e2"])
        }
        let completeCalls = HitCounter()
        server.route("POST", "/v2/uploads/\(uploadId)/complete") { _ in
            completeCalls.bump()
            return MockResponse(status: 200, body: Data())
        }

        let client = Drive9Client(baseUrl: server.baseURL, apiKey: "k")
        let upload = try await client.newStreamUpload(
            remotePath: "/big.bin",
            totalSize: partSize * 2
        )
        try upload.writePart(partNum: 1, data: Data(repeating: UInt8(ascii: "a"), count: Int(partSize)))
        try upload.complete(finalPartNum: 2, finalData: Data(repeating: UInt8(ascii: "a"), count: Int(partSize)))
        XCTAssertEqual(putHits.get(), 2)
        XCTAssertEqual(completeCalls.get(), 1)

        do {
            try upload.writePart(partNum: 3, data: Data())
            XCTFail("expected rejection after complete")
        } catch let error as Drive9Exception {
            guard case let .Drive9(code, _, detail, _) = error else {
                XCTFail("unexpected variant: \(error)"); return
            }
            XCTAssertEqual(code, "other")
            XCTAssertTrue(detail.contains("completed"), "want completed reason: \(detail)")
        }
    }

    func testStreamUploadParameterErrorKeepsUploadActive() async throws {
        let uploadId = "u-stream-param-sw"
        let partSize: Int64 = 100
        server.route("POST", "/v2/uploads/initiate") { _ in
            let body = #"{"upload_id":"\#(uploadId)","key":"k","part_size":\#(partSize),"total_parts":1}"#
            return MockResponse(status: 200, body: Data(body.utf8), contentType: "application/json")
        }
        let baseURL = server.baseURL
        server.routeAnyQuery("POST", "/v2/uploads/\(uploadId)/presign") { request in
            let req = String(data: request.body, encoding: .utf8) ?? ""
            let partNum = req
                .components(separatedBy: "\"part_number\":")
                .last?
                .components(separatedBy: CharacterSet.decimalDigits.inverted)
                .first
                .flatMap { Int($0) } ?? 0
            let body = #"{"number":\#(partNum),"url":"\#(baseURL)/upload/\#(partNum)","size":\#(partSize)}"#
            return MockResponse(status: 200, body: Data(body.utf8), contentType: "application/json")
        }
        server.route("PUT", "/upload/1") { _ in
            MockResponse(status: 200, body: Data(), extraHeaders: ["ETag": "e1"])
        }
        let completeCalls = HitCounter()
        server.route("POST", "/v2/uploads/\(uploadId)/complete") { _ in
            completeCalls.bump()
            return MockResponse(status: 200, body: Data())
        }

        let client = Drive9Client(baseUrl: server.baseURL, apiKey: "k")
        let upload = try await client.newStreamUpload(remotePath: "/param.bin", totalSize: partSize)

        // Parameter error: part_num must be >= 1
        do {
            try upload.writePart(partNum: 0, data: Data(repeating: UInt8(ascii: "a"), count: Int(partSize)))
            XCTFail("expected parameter error")
        } catch let error as Drive9Exception {
            guard case let .Drive9(_, _, detail, _) = error else {
                XCTFail("unexpected variant: \(error)"); return
            }
            XCTAssertTrue(detail.contains("part number must be"),
                          "want parameter error detail: \(detail)")
        }

        // Legitimate path still works — upload must not be poisoned.
        try upload.writePart(partNum: 1, data: Data(repeating: UInt8(ascii: "a"), count: Int(partSize)))
        try upload.complete(finalPartNum: 1, finalData: Data())
        XCTAssertEqual(completeCalls.get(), 1)
    }

    func testStreamUploadCompleteAloneWorksForOnePartStream() async throws {
        let uploadId = "u-stream-one-shot-sw"
        let partSize: Int64 = 100
        server.route("POST", "/v2/uploads/initiate") { _ in
            let body = #"{"upload_id":"\#(uploadId)","key":"k","part_size":\#(partSize),"total_parts":1}"#
            return MockResponse(status: 200, body: Data(body.utf8), contentType: "application/json")
        }
        let baseURL = server.baseURL
        server.routeAnyQuery("POST", "/v2/uploads/\(uploadId)/presign") { request in
            let req = String(data: request.body, encoding: .utf8) ?? ""
            let partNum = req
                .components(separatedBy: "\"part_number\":")
                .last?
                .components(separatedBy: CharacterSet.decimalDigits.inverted)
                .first
                .flatMap { Int($0) } ?? 0
            let body = #"{"number":\#(partNum),"url":"\#(baseURL)/upload/\#(partNum)","size":\#(partSize)}"#
            return MockResponse(status: 200, body: Data(body.utf8), contentType: "application/json")
        }
        let putHits = HitCounter()
        server.route("PUT", "/upload/1") { _ in
            putHits.bump()
            return MockResponse(status: 200, body: Data(), extraHeaders: ["ETag": "e1"])
        }
        let completeCalls = HitCounter()
        server.route("POST", "/v2/uploads/\(uploadId)/complete") { _ in
            completeCalls.bump()
            return MockResponse(status: 200, body: Data())
        }

        let client = Drive9Client(baseUrl: server.baseURL, apiKey: "k")
        let upload = try await client.newStreamUpload(remotePath: "/one-shot.bin", totalSize: partSize)
        // No prior writePart; deliver the whole payload via complete().
        try upload.complete(finalPartNum: 1, finalData: Data(repeating: UInt8(ascii: "a"), count: Int(partSize)))
        XCTAssertEqual(putHits.get(), 1)
        XCTAssertEqual(completeCalls.get(), 1)
    }

    func testStreamUploadAbortIsIdempotentAndRejectsFurtherWrites() async throws {
        let uploadId = "u-stream-swift-abort"
        let partSize: Int64 = 100
        server.route("POST", "/v2/uploads/initiate") { _ in
            let body = #"{"upload_id":"\#(uploadId)","key":"k","part_size":\#(partSize),"total_parts":1}"#
            return MockResponse(status: 200, body: Data(body.utf8), contentType: "application/json")
        }
        let baseURL = server.baseURL
        server.routeAnyQuery("POST", "/v2/uploads/\(uploadId)/presign") { request in
            let req = String(data: request.body, encoding: .utf8) ?? ""
            let partNum = req
                .components(separatedBy: "\"part_number\":")
                .last?
                .components(separatedBy: CharacterSet.decimalDigits.inverted)
                .first
                .flatMap { Int($0) } ?? 0
            let body = #"{"number":\#(partNum),"url":"\#(baseURL)/upload/\#(partNum)","size":\#(partSize)}"#
            return MockResponse(status: 200, body: Data(body.utf8), contentType: "application/json")
        }
        server.route("PUT", "/upload/1") { _ in
            MockResponse(status: 200, body: Data(), extraHeaders: ["ETag": "e1"])
        }
        let abortCalls = HitCounter()
        server.route("POST", "/v2/uploads/\(uploadId)/abort") { _ in
            abortCalls.bump()
            return MockResponse(status: 200, body: Data())
        }

        let client = Drive9Client(baseUrl: server.baseURL, apiKey: "k")
        let upload = try await client.newStreamUpload(
            remotePath: "/abrt.bin",
            totalSize: partSize
        )
        try upload.writePart(partNum: 1, data: Data(repeating: UInt8(ascii: "a"), count: Int(partSize)))
        try upload.abort()
        try upload.abort() // idempotent
        XCTAssertEqual(abortCalls.get(), 1)

        do {
            try upload.writePart(partNum: 2, data: Data())
            XCTFail("expected rejection after abort")
        } catch let error as Drive9Exception {
            guard case let .Drive9(code, _, detail, _) = error else {
                XCTFail("unexpected variant: \(error)"); return
            }
            XCTAssertEqual(code, "other")
            XCTAssertTrue(detail.contains("aborted"), "want aborted reason: \(detail)")
        }
    }

    func testVaultListReadableSecretsHappyPath() async throws {
        server.route("GET", "/v1/vault/read") { _ in
            let body = #"{"secrets":["alpha","beta"]}"#
            return MockResponse(status: 200, body: Data(body.utf8), contentType: "application/json")
        }

        let client = Drive9Client(baseUrl: server.baseURL, apiKey: "k")
        let names = try await client.vaultListReadableSecrets()
        XCTAssertEqual(names, ["alpha", "beta"])
    }

    func testVaultReadSecretFieldPassesJsonLookingStringThroughUntouched() async throws {
        let raw = #"{"k":1,"nested":{"flag":true}}"#
        server.route("GET", "/v1/vault/read/dest-config/payload") { _ in
            MockResponse(status: 200, body: Data(raw.utf8))
        }

        let client = Drive9Client(baseUrl: server.baseURL, apiKey: "k")
        let value = try await client.vaultReadSecretField(name: "dest-config", field: "payload")
        XCTAssertEqual(value, raw)
    }

    func testVaultUnauthorizedSurfacesAsHttpStatus() async throws {
        server.route("GET", "/v1/vault/read") { _ in
            let body = #"{"error":"token expired"}"#
            return MockResponse(status: 401, body: Data(body.utf8), contentType: "application/json")
        }

        let client = Drive9Client(baseUrl: server.baseURL, apiKey: "k")
        do {
            _ = try await client.vaultListReadableSecrets()
            XCTFail("expected unauthorized error")
        } catch let error as Drive9Exception {
            guard case let .Drive9(code, statusCode, detail, _) = error else {
                XCTFail("unexpected variant: \(error)")
                return
            }
            XCTAssertEqual(code, "http_status")
            XCTAssertEqual(statusCode, 401)
            XCTAssertEqual(detail, "token expired")
        }
    }

    func testStatusErrorCarriesCode() async throws {
        server.route("GET", "/v1/fs/missing.txt") { _ in
            let body = #"{"error":"forbidden"}"#
            return MockResponse(status: 403, body: Data(body.utf8), contentType: "application/json")
        }

        let client = Drive9Client(baseUrl: server.baseURL, apiKey: "k")
        do {
            _ = try await client.read(path: "/missing.txt")
            XCTFail("expected forbidden")
        } catch let error as Drive9Exception {
            guard case let .Drive9(code, statusCode, detail, _) = error else {
                XCTFail("unexpected variant: \(error)")
                return
            }
            XCTAssertEqual(code, "http_status")
            XCTAssertEqual(statusCode, 403)
            XCTAssertEqual(detail, "forbidden")
        }
    }
}
