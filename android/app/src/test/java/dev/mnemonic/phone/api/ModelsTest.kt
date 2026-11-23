package dev.mnemonic.phone.api

import dev.mnemonic.phone.Fixtures
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class ModelsTest {
    @Test
    fun workspacesDecode() {
        val list = MacJson.decodeFromString<TeamWorkspaceList>(Fixtures.wire("workspaces.json"))
        val workspace = list.workspaces.single()
        assertEquals(TeamAgentState.WaitingForApproval, workspace.agentState)
        assertEquals(listOf(true, false), workspace.chats.map { it.waiting })
        assertEquals(1_700_000_100_000L, workspace.chats[1].lastTurnEndedAt)
    }

    @Test
    fun unknownAgentStateIsIdle() {
        val workspace = MacJson.decodeFromString<TeamWorkspace>(
            """{"id":"w","project":"p","name":"n","branch":"b","agent_state":"sleeping"}""",
        )
        assertEquals(TeamAgentState.Idle, workspace.agentState)
    }

    @Test
    fun chatUpdateDecodes() {
        val update = MacJson.decodeFromString<TeamChatUpdate>(Fixtures.wire("chat_update.json"))
        assertTrue(update.reset)
        assertEquals(5, update.changes.size)
        assertTrue(update.changes[0].entry is TeamEntry.Updates)
        assertEquals("tool-2", update.permissions.single().toolCallId)
        assertEquals("allow", update.permissions.single().options[0].optionId)
        assertEquals(2, update.plan.size)
    }

    @Test
    fun unknownEntryKindIsUnknown() {
        val entry = MacJson.decodeFromString<TeamEntry>("""{"kind":"hologram","x":1}""")
        assertTrue(entry is TeamEntry.Unknown)
    }

    @Test
    fun actionAndStatusAnswersDecode() {
        val pr = MacJson.decodeFromString<PhonePrStatus>(Fixtures.wire("pr_status.json"))
        assertTrue(pr.canMerge)
        assertEquals("0123abcd", pr.headRefOid)
        assertEquals("claude", MacJson.decodeFromString<PhoneAgentList>(Fixtures.wire("agents.json")).agents.single().id)
        assertEquals(
            "repo-1",
            MacJson.decodeFromString<PhoneRepositoryList>(Fixtures.wire("repositories.json")).repositories.single().id,
        )
        val outcome = MacJson.decodeFromString<PhoneActionOutcome>(Fixtures.wire("outcome.json"))
        assertEquals("chat-9", outcome.chatId)
        assertEquals("ws-9", outcome.workspaceId)
    }

    @Test
    fun chatOptionsDecode() {
        val options = MacJson.decodeFromString<PhoneChatOptionList>(Fixtures.wire("chat_options.json")).options
        assertEquals(listOf("model", "effort"), options.map { it.kind })
        assertEquals("opus", options[0].current)
        assertEquals(listOf("opus", "sonnet"), options[0].values.map { it.id })
        val offered = MacJson.decodeFromString<PhoneAgentList>(Fixtures.wire("agents.json")).agents.single().options.single()
        assertEquals("model", offered.id)
        assertEquals(null, offered.current)
    }
}
