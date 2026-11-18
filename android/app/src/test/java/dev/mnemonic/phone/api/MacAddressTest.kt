package dev.mnemonic.phone.api

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class MacAddressTest {
    @Test
    fun addsSchemeLowercasesHostAndKeepsPort() {
        val address = MacAddress.parse("Jays-MacBook-Pro.tail6cfd16.ts.net:47232/")
        assertEquals("https://jays-macbook-pro.tail6cfd16.ts.net:47232", address?.origin)
        assertEquals("https://jays-macbook-pro.tail6cfd16.ts.net:47232/", address?.baseUrl)
    }

    @Test
    fun dropsPathQueryAndFragment() {
        val address = MacAddress.parse("  http://100.79.128.81:47232/x?y#z ")
        assertEquals("http://100.79.128.81:47232", address?.origin)
    }

    @Test
    fun dropsDefaultPort() {
        assertEquals("https://host.ts.net", MacAddress.parse("https://host.ts.net:443")?.origin)
        assertEquals("http://host.ts.net", MacAddress.parse("http://host.ts.net:80")?.origin)
    }

    @Test
    fun refusesOtherSchemesAndEmptyText() {
        assertNull(MacAddress.parse("ftp://x"))
        assertNull(MacAddress.parse(""))
        assertNull(MacAddress.parse("   "))
    }
}
