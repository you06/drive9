package com.drive9.mobile

import com.sun.net.httpserver.HttpExchange
import com.sun.net.httpserver.HttpHandler
import com.sun.net.httpserver.HttpServer
import kotlinx.coroutines.runBlocking
import uniffi.drive9_mobile_core.Drive9Exception
import java.net.InetSocketAddress
import java.nio.charset.StandardCharsets
import kotlin.test.AfterTest
import kotlin.test.BeforeTest
import kotlin.test.Test
import kotlin.test.assertContentEquals
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertFalse
import kotlin.test.assertTrue

/**
 * JVM-runnable smoke tests against an in-process [HttpServer]. The goal is to
 * exercise the FFI surface end-to-end (native lib load, runtime dispatch,
 * error mapping) without needing an Android device.
 */
class Drive9Test {
    private lateinit var server: HttpServer
    private lateinit var baseUrl: String
    private val routes = mutableMapOf<String, HttpHandler>()

    @BeforeTest
    fun startServer() {
        server = HttpServer.create(InetSocketAddress("127.0.0.1", 0), 0)
        server.createContext("/") { ex ->
            val key = "${ex.requestMethod} ${ex.requestURI.rawPath}${ex.requestURI.rawQuery?.let { "?$it" } ?: ""}"
            val handler = routes[key]
            if (handler == null) {
                ex.sendResponseHeaders(404, -1)
                ex.close()
                return@createContext
            }
            handler.handle(ex)
        }
        server.start()
        baseUrl = "http://127.0.0.1:${server.address.port}"
    }

    @AfterTest
    fun stopServer() {
        server.stop(0)
    }

    private fun route(method: String, path: String, handler: (HttpExchange) -> Unit) {
        routes["$method $path"] = HttpHandler { ex -> handler(ex) }
    }

    @Test
    fun writeThenReadRoundtrip() = runBlocking {
        route("PUT", "/v1/fs/hello.txt") { ex ->
            ex.requestBody.readBytes()
            ex.sendResponseHeaders(200, -1)
            ex.close()
        }
        route("GET", "/v1/fs/hello.txt") { ex ->
            val body = "hello kotlin".toByteArray(StandardCharsets.UTF_8)
            ex.sendResponseHeaders(200, body.size.toLong())
            ex.responseBody.write(body)
            ex.close()
        }

        val client = Drive9Client(baseUrl, "test-key")
        client.write("/hello.txt", "hello kotlin".toByteArray())
        val data = client.read("/hello.txt")
        assertContentEquals("hello kotlin".toByteArray(), data)
    }

    @Test
    fun listReturnsEntries() = runBlocking {
        route("GET", "/v1/fs/data/?list=1") { ex ->
            val body = """{"entries":[{"name":"a.txt","size":3,"isDir":false},{"name":"sub","size":0,"isDir":true}]}"""
                .toByteArray(StandardCharsets.UTF_8)
            ex.responseHeaders.add("Content-Type", "application/json")
            ex.sendResponseHeaders(200, body.size.toLong())
            ex.responseBody.write(body)
            ex.close()
        }

        val client = Drive9Client(baseUrl, "k")
        val entries = client.list("/data/")
        assertEquals(2, entries.size)
        assertEquals("a.txt", entries[0].name)
        assertEquals(3, entries[0].size)
        assertFalse(entries[0].isDir)
        assertTrue(entries[1].isDir)
    }

    @Test
    fun statReportsRevisionAndSize() = runBlocking {
        route("HEAD", "/v1/fs/f.bin") { ex ->
            ex.responseHeaders.add("Content-Length", "42")
            ex.responseHeaders.add("X-Dat9-Revision", "9")
            ex.sendResponseHeaders(200, -1)
            ex.close()
        }

        val client = Drive9Client(baseUrl, "k")
        val s = client.stat("/f.bin")
        assertEquals(42, s.size)
        assertEquals(9, s.revision)
        assertFalse(s.isDir)
    }

    @Test
    fun deleteSucceeds() = runBlocking {
        route("DELETE", "/v1/fs/gone.txt") { ex ->
            ex.sendResponseHeaders(204, -1)
            ex.close()
        }

        val client = Drive9Client(baseUrl, "k")
        client.delete("/gone.txt")
    }

    @Test
    fun conflictPreservesServerRevision() = runBlocking {
        route("PUT", "/v1/fs/r.txt") { ex ->
            ex.requestBody.readBytes()
            val body = """{"error":"revision mismatch","server_revision":12}"""
                .toByteArray(StandardCharsets.UTF_8)
            ex.responseHeaders.add("Content-Type", "application/json")
            ex.sendResponseHeaders(409, body.size.toLong())
            ex.responseBody.write(body)
            ex.close()
        }

        val client = Drive9Client(baseUrl, "k")
        val err = assertFailsWith<Drive9Exception.Drive9> {
            client.write("/r.txt", "x".toByteArray(), expectedRevision = 7L)
        }
        assertEquals("conflict", err.code)
        assertEquals(409, err.statusCode)
        assertEquals(12L, err.serverRevision)
    }

    @Test
    fun copyRenameMkdirSucceed() = runBlocking {
        route("POST", "/v1/fs/dst.txt?copy") { ex ->
            assertEquals("/src.txt", ex.requestHeaders.getFirst("X-Dat9-Copy-Source"))
            ex.sendResponseHeaders(200, -1); ex.close()
        }
        route("POST", "/v1/fs/new.txt?rename") { ex ->
            assertEquals("/old.txt", ex.requestHeaders.getFirst("X-Dat9-Rename-Source"))
            ex.sendResponseHeaders(200, -1); ex.close()
        }
        route("POST", "/v1/fs/dir/?mkdir") { ex ->
            ex.sendResponseHeaders(200, -1); ex.close()
        }

        val client = Drive9Client(baseUrl, "k")
        client.copy("/src.txt", "/dst.txt")
        client.rename("/old.txt", "/new.txt")
        client.mkdir("/dir/")
    }

    @Test
    fun grepReturnsSearchResults() = runBlocking {
        route("GET", "/v1/fs/?grep=hello&limit=3") { ex ->
            val body = """[{"path":"/a.txt","name":"a.txt","size_bytes":7,"score":0.5}]"""
                .toByteArray(StandardCharsets.UTF_8)
            ex.responseHeaders.add("Content-Type", "application/json")
            ex.sendResponseHeaders(200, body.size.toLong())
            ex.responseBody.write(body); ex.close()
        }

        val client = Drive9Client(baseUrl, "k")
        val hits = client.grep("hello", "/", 3)
        assertEquals(1, hits.size)
        assertEquals("/a.txt", hits[0].path)
        assertEquals(7L, hits[0].sizeBytes)
        assertEquals(0.5, hits[0].score)
    }

    @Test
    fun findForwardsParams() = runBlocking {
        // HashMap iteration order is not deterministic; match each piece on
        // the server side instead of assuming a fixed query string.
        server.createContext("/v1/fs/data/") { ex ->
            val query = ex.requestURI.rawQuery.orEmpty()
            assertTrue("find=" in query, "missing find=: $query")
            assertTrue("type=file" in query, "missing type=file: $query")
            assertTrue("limit=10" in query, "missing limit=10: $query")
            val body = """[{"path":"/data/x.txt","name":"x.txt","size_bytes":1,"score":null}]"""
                .toByteArray(StandardCharsets.UTF_8)
            ex.responseHeaders.add("Content-Type", "application/json")
            ex.sendResponseHeaders(200, body.size.toLong())
            ex.responseBody.write(body); ex.close()
        }

        val client = Drive9Client(baseUrl, "k")
        val hits = client.find("/data/", mapOf("type" to "file", "limit" to "10"))
        assertEquals(1, hits.size)
        assertEquals("/data/x.txt", hits[0].path)
    }

    @Test
    fun sqlReturnsJsonStrings() = runBlocking {
        route("POST", "/v1/sql") { ex ->
            ex.requestBody.readBytes()
            val body = """[{"path":"/a.txt","size":10},{"path":"/b","size":0}]"""
                .toByteArray(StandardCharsets.UTF_8)
            ex.responseHeaders.add("Content-Type", "application/json")
            ex.sendResponseHeaders(200, body.size.toLong())
            ex.responseBody.write(body); ex.close()
        }

        val client = Drive9Client(baseUrl, "k")
        val rows = client.sql("SELECT path, size FROM files")
        assertEquals(2, rows.size)
        // The wrapper does not interpret schema; we just confirm each row is
        // valid JSON that we can parse back to inspect a field.
        assertTrue("\"path\":\"/a.txt\"" in rows[0])
        assertTrue("\"size\":10" in rows[0])
        assertTrue("\"path\":\"/b\"" in rows[1])
    }

    @Test
    fun statusErrorCarriesCode() = runBlocking {
        route("GET", "/v1/fs/missing.txt") { ex ->
            val body = """{"error":"forbidden"}""".toByteArray(StandardCharsets.UTF_8)
            ex.responseHeaders.add("Content-Type", "application/json")
            ex.sendResponseHeaders(403, body.size.toLong())
            ex.responseBody.write(body)
            ex.close()
        }

        val client = Drive9Client(baseUrl, "k")
        val err = assertFailsWith<Drive9Exception.Drive9> {
            client.read("/missing.txt")
        }
        assertEquals("http_status", err.code)
        assertEquals(403, err.statusCode)
        assertEquals("forbidden", err.detail)
    }
}
