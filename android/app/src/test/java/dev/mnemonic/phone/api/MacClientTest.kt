package dev.mnemonic.phone.api

import kotlinx.coroutines.runBlocking
import okhttp3.OkHttpClient
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test

class MacClientTest {
    private lateinit var server: MockWebServer
    private lateinit var address: MacAddress

    @Before
    fun start() {
        server = MockWebServer()
        server.start()
        address = requireNotNull(MacAddress.parse("http://${server.hostName}:${server.port}"))
    }

    @After
    fun stop() {
        server.shutdown()
    }

    @Test
    fun postCarriesTheHeadersTheMacRequires() = runBlocking {
        server.enqueue(MockResponse().setBody("{}"))

        MacClient(address, OkHttpClient()).sendMessage("chat-1", "hello")

        val request = server.takeRequest()
        assertEquals("POST", request.method)
        assertEquals("/v1/chats/chat-1/message", request.path)
        assertEquals("app", request.getHeader("X-Mnemonic-Phone"))
        assertTrue(request.getHeader("Content-Type").orEmpty().startsWith("application/json"))
        assertEquals(address.origin, request.getHeader("Origin"))
        assertEquals("""{"text":"hello"}""", request.body.readUtf8())
    }

    @Test
    fun aRefusalKeepsTheMacsWordsVerbatim() = runBlocking {
        server.enqueue(MockResponse().setResponseCode(409).setBody("Open Mnemonic on the Mac first"))

        val result = MacClient(address, OkHttpClient()).stop("chat-1")

        assertEquals(MacResult.Refused(409, "Open Mnemonic on the Mac first"), result)
    }

    @Test
    fun aClosedPortIsUnreachable() = runBlocking {
        server.shutdown()

        val result = MacClient(address, OkHttpClient()).workspaces()

        assertTrue(result is MacResult.Unreachable)
    }
}
