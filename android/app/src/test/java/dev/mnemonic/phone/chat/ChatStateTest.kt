package dev.mnemonic.phone.chat

import dev.mnemonic.phone.Fixtures
import dev.mnemonic.phone.api.MacJson
import dev.mnemonic.phone.api.PlanEntry
import dev.mnemonic.phone.api.TeamChatUpdate
import dev.mnemonic.phone.api.TeamEntry
import dev.mnemonic.phone.api.TeamEntryChange
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ChatStateTest {
    private fun fixtureUpdate() = MacJson.decodeFromString<TeamChatUpdate>(Fixtures.wire("chat_update.json"))

    @Test
    fun waitingToolIsNeverFolded() {
        val state = ChatState()
        state.apply(fixtureUpdate())

        val items = state.items
        assertEquals(6, items.size)
        assertEquals(ChatItem.UserMessage("Fix the login page"), items[0])
        assertEquals(ChatItem.AgentMessage("Looking at the form handler"), items[1])
        val first = items[2] as ChatItem.FoldedTools
        assertEquals(listOf("tool-1"), first.tools.map { it.id })
        assertEquals("Read login.rs", first.label)
        val waiting = items[3] as ChatItem.Tool
        assertEquals("tool-2", waiting.id)
        assertTrue(waiting.waiting)
        val last = items[4] as ChatItem.FoldedTools
        assertEquals(listOf("tool-3"), last.tools.map { it.id })
        assertEquals("in_progress", last.tools.single().status)
        val plan = items[5] as ChatItem.Plan
        assertEquals(2, plan.entries.size)
        assertFalse(plan.done)
        assertTrue(state.generating)
        assertEquals("cursor-5", state.cursor)
    }

    @Test
    fun consecutiveToolsFoldIntoOneLine() {
        val state = ChatState()
        val update = fixtureUpdate().copy(permissions = emptyList())
        state.apply(update)

        val fold = state.items[2] as ChatItem.FoldedTools
        assertEquals(listOf("tool-1", "tool-2", "tool-3"), fold.tools.map { it.id })
        assertEquals("3 tool calls", fold.label)
        assertFalse(state.items.any { it is ChatItem.Tool })
    }

    @Test
    fun resetReplacesEverything() {
        val state = ChatState()
        state.apply(fixtureUpdate())
        val replacement = MacJson.decodeFromString<TeamChatUpdate>(
            """{"cursor":"c2","reset":true,"len":1,"changes":[{"index":0,"entry":{"kind":"updates","updates":[
            {"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"Start over"}}]}}]}""",
        )

        state.apply(replacement)

        assertEquals(listOf<ChatItem>(ChatItem.UserMessage("Start over")), state.items)
        assertFalse(state.generating)
        assertTrue(state.permissions.isEmpty())
        assertEquals("c2", state.cursor)
    }

    @Test
    fun completedPlanIsMarkedDoneAndLivePlanIsNot() {
        val state = ChatState()
        val update = fixtureUpdate().copy(
            reset = true,
            len = 1,
            changes = listOf(
                TeamEntryChange(0, TeamEntry.CompletedPlan(listOf(PlanEntry("Ship it", "medium", "completed")))),
            ),
            permissions = emptyList(),
        )
        state.apply(update)

        val plans = state.items.filterIsInstance<ChatItem.Plan>()
        assertEquals(listOf(true, false), plans.map { it.done })
    }
}
