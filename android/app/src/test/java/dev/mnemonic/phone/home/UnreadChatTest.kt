package dev.mnemonic.phone.home

import dev.mnemonic.phone.api.TeamAgentState
import dev.mnemonic.phone.api.TeamChat
import dev.mnemonic.phone.api.TeamWorkspace
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class UnreadChatTest {
    private fun chat(id: String) =
        TeamChat(id = id, title = id, agent = "claude", open = true, live = true,
            lastTurnEndedAt = 2_000, lastTurnErrored = false, waiting = false)

    private fun workspace(id: String, vararg chats: TeamChat) =
        TeamWorkspace(id = id, project = "p", name = id, branch = "b",
            agentState = TeamAgentState.Idle, chats = chats.toList())

    @Test
    fun aWorkspaceWithAFinishedItemIsUnreadWithThatItem() {
        val target = workspace("a", chat("a1"))
        val finished = NeedsYouItem.Finished(target, target.chats[0], errored = true)
        val other = workspace("b", chat("b1"))
        val items = listOf(NeedsYouItem.Waiting(other, other.chats[0]), finished)

        assertEquals(finished, unreadChat(target, items))
    }

    @Test
    fun aWorkspaceWithOnlyAWaitingItemIsNotUnread() {
        val target = workspace("a", chat("a1"))

        assertNull(unreadChat(target, listOf(NeedsYouItem.Waiting(target, target.chats[0]))))
    }

    @Test
    fun aWorkspaceWithNoItemsIsNotUnread() {
        assertNull(unreadChat(workspace("a", chat("a1")), emptyList()))
    }
}
