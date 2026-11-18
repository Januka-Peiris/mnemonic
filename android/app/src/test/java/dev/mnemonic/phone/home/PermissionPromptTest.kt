package dev.mnemonic.phone.home

import dev.mnemonic.phone.api.MacJson
import dev.mnemonic.phone.api.TeamChatUpdate
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class PermissionPromptTest {
    private fun update(vararg updates: String): TeamChatUpdate =
        MacJson.decodeFromString(
            TeamChatUpdate.serializer(),
            """{"cursor":"c","reset":true,"len":1,
               "changes":[{"index":0,"entry":{"kind":"updates","updates":[${updates.joinToString(",")}]}}],
               "permissions":[{"tool_call_id":"t1","options":[]}]}""",
        )

    @Test
    fun anUpdateBeforeItsToolCallIsIgnored() {
        val prompt = promptFrom(
            update(
                """{"sessionUpdate":"tool_call_update","toolCallId":"t1","title":"early","kind":"execute"}""",
                """{"sessionUpdate":"tool_call","toolCallId":"t1","title":"late"}""",
            ),
            0,
        )

        assertEquals(PromptTool("t1", "late", "other"), prompt.tool)
    }

    @Test
    fun aToolWithoutATitleStillCountsAsATool() {
        val prompt = promptFrom(update("""{"sessionUpdate":"tool_call","toolCallId":"t1"}"""), 0)

        assertEquals(PromptTool("t1", "", "other"), prompt.tool)
    }

    @Test
    fun noToolCallLeavesNoTool() {
        assertNull(promptFrom(update("""{"sessionUpdate":"agent_message_chunk"}"""), 0).tool)
    }

    @Test
    fun nonPrimitiveFieldsDoNotThrow() {
        val prompt = promptFrom(
            update("""{"sessionUpdate":"tool_call","toolCallId":"t1","title":{"a":1},"kind":["x"]}"""),
            0,
        )

        assertEquals(PromptTool("t1", "", "other"), prompt.tool)
    }
}
