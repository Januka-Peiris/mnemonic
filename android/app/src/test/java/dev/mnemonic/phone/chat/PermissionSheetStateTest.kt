package dev.mnemonic.phone.chat

import dev.mnemonic.phone.api.TeamPermissionRequest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class PermissionSheetStateTest {
    private fun request(id: String) = TeamPermissionRequest(toolCallId = id)

    @Test
    fun oldestFirst() {
        assertEquals("a", sheetPermission(listOf(request("a"), request("b")), emptySet())?.toolCallId)
    }

    @Test
    fun dismissedSkipsToNext() {
        assertEquals("b", sheetPermission(listOf(request("a"), request("b")), setOf("a"))?.toolCallId)
    }

    @Test
    fun allDismissedGivesNull() {
        assertNull(sheetPermission(listOf(request("a"), request("b")), setOf("a", "b")))
    }

    @Test
    fun newRequestAfterDismissShows() {
        assertEquals("c", sheetPermission(listOf(request("a"), request("c")), setOf("a"))?.toolCallId)
    }
}
