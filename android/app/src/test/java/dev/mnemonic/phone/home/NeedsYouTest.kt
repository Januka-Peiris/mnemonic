package dev.mnemonic.phone.home

import dev.mnemonic.phone.api.TeamAgentState
import dev.mnemonic.phone.api.TeamChat
import dev.mnemonic.phone.api.TeamWorkspace
import org.junit.Assert.assertEquals
import org.junit.Test

class NeedsYouTest {
    private fun chat(id: String, waiting: Boolean = false, endedAt: Long? = null, errored: Boolean = false) =
        TeamChat(id = id, title = id, agent = "claude", open = true, live = true,
            lastTurnEndedAt = endedAt, lastTurnErrored = errored, waiting = waiting)

    private fun workspace(vararg chats: TeamChat) =
        TeamWorkspace(id = "ws", project = "p", name = "n", branch = "b",
            agentState = TeamAgentState.Idle, chats = chats.toList())

    @Test
    fun waitingChatsComeFirstThenNewestFinished() {
        val workspaces = listOf(
            workspace(chat("old", endedAt = 2_000), chat("wait", waiting = true), chat("new", endedAt = 3_000, errored = true)),
        )

        val items = needsYou(workspaces, openedAt = { 0 }, installedAt = 1_000)

        assertEquals(
            listOf("wait" to "waiting", "new" to "errored", "old" to "finished"),
            items.map {
                when (it) {
                    is NeedsYouItem.Waiting -> it.chat.id to "waiting"
                    is NeedsYouItem.Finished -> it.chat.id to if (it.errored) "errored" else "finished"
                }
            },
        )
    }

    @Test
    fun aChatOpenedAfterItsTurnEndedIsNotListed() {
        val workspaces = listOf(workspace(chat("seen", endedAt = 2_000), chat("unseen", endedAt = 5_000)))

        val items = needsYou(workspaces, openedAt = { if (it == "seen") 2_500 else 0 }, installedAt = 0)

        assertEquals(listOf("unseen"), items.map { (it as NeedsYouItem.Finished).chat.id })
    }

    @Test
    fun turnsEndedBeforeInstallAreNotListed() {
        val workspaces = listOf(workspace(chat("before", endedAt = 900), chat("after", endedAt = 1_100)))

        val items = needsYou(workspaces, openedAt = { 0 }, installedAt = 1_000)

        assertEquals(listOf("after"), items.map { (it as NeedsYouItem.Finished).chat.id })
    }
}
