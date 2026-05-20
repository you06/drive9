import XCTest
@testable import Drive9Mobile

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
