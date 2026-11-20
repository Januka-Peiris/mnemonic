package dev.mnemonic.phone.chat

import org.junit.Assert.assertEquals
import org.junit.Test

class ToolSummaryTest {
    private fun tool(kind: String, title: String = "t", status: String = "completed") =
        ChatItem.Tool("id", title, status, waiting = false, kind = kind)

    @Test
    fun mixedKindsAreCountedByKind() {
        val tools = listOf("read", "read", "read", "read", "execute", "execute").map { tool(it) }
        assertEquals("read 4 files · ran 2 commands", toolRunSummary(tools))
    }

    @Test
    fun editSearchFetchAndOther() {
        val tools = listOf("edit", "search", "search", "fetch", "think").map { tool(it) }
        assertEquals("edited 1 file · searched 2 times · fetched 1 page · 1 other step", toolRunSummary(tools))
    }

    @Test
    fun singleToolUsesItsTitle() {
        assertEquals("cargo test", toolRunSummary(listOf(tool("execute", "cargo test"))))
    }

    @Test
    fun failuresAreCounted() {
        val tools = listOf(tool("read", status = "failed"), tool("read", status = "failed"), tool("read"))
        assertEquals(2, toolRunFailed(tools))
    }
}
