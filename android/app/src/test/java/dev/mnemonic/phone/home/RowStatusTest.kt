package dev.mnemonic.phone.home

import dev.mnemonic.phone.api.TeamAgentState
import dev.mnemonic.phone.api.TeamChat
import dev.mnemonic.phone.api.TeamWorkspace
import dev.mnemonic.phone.ui.StatusKind
import org.junit.Assert.assertEquals
import org.junit.Test

class RowStatusTest {
    private fun chat(endedAt: Long?, errored: Boolean = false) =
        TeamChat(id = "c", title = "c", agent = "claude", open = true, live = true,
            lastTurnEndedAt = endedAt, lastTurnErrored = errored, waiting = false)

    private fun workspace(chat: TeamChat) =
        TeamWorkspace(id = "ws", project = "p", name = "n", branch = "b",
            agentState = TeamAgentState.Idle, idleSeconds = 600, chats = listOf(chat))

    @Test
    fun aReadFinishedRunIsGreen() {
        assertEquals(RowStatus(StatusKind.Finished, "Finished · 10m"), rowStatus(workspace(chat(2_000)), null))
    }

    @Test
    fun aReadErroredRunIsRed() {
        assertEquals(RowStatus(StatusKind.Error, "Errored · 10m"), rowStatus(workspace(chat(2_000, errored = true)), null))
    }

    @Test
    fun noEndedTurnStaysIdle() {
        assertEquals(RowStatus(StatusKind.Idle, "Idle 10m"), rowStatus(workspace(chat(null)), null))
    }

    @Test
    fun unreadIsMarked() {
        val workspace = workspace(chat(2_000, errored = true))
        val unread = NeedsYouItem.Finished(workspace, workspace.chats[0], errored = true)
        assertEquals(RowStatus(StatusKind.Error, "Errored · 10m · unread"), rowStatus(workspace, unread))
    }
}
