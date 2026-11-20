package dev.mnemonic.phone.chat

import dev.mnemonic.phone.api.TeamAgentState
import org.junit.Assert.assertEquals
import org.junit.Test

class ChatStatusTest {
    @Test
    fun needsYouBeatsWorking() {
        assertEquals(ChatStatus.NeedsYou, chatStatus(true, true, TeamAgentState.Working))
    }

    @Test
    fun workingBeatsError() {
        assertEquals(ChatStatus.Working, chatStatus(true, false, TeamAgentState.Error))
    }

    @Test
    fun errorBeatsIdle() {
        assertEquals(ChatStatus.Error, chatStatus(false, false, TeamAgentState.Error))
    }

    @Test
    fun nothingIsIdle() {
        assertEquals(ChatStatus.Idle, chatStatus(false, false, null))
    }
}
