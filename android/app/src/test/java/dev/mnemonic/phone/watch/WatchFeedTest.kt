package dev.mnemonic.phone.watch

import dev.mnemonic.phone.Fixtures
import dev.mnemonic.phone.api.MacJson
import dev.mnemonic.phone.api.TeamChatUpdate
import dev.mnemonic.phone.chat.ChatState
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class WatchFeedTest {
    @Test
    fun watchAnswersTheToolThatIsWaiting() {
        val state = ChatState()
        state.apply(MacJson.decodeFromString<TeamChatUpdate>(Fixtures.wire("chat_update.json")))

        val message = chatMessage("chat-1", "login: Fix", state)

        assertEquals("tool-2", message["call"])
        val options = message["options"] as List<*>
        assertEquals(state.permissions.getValue("tool-2").map { it.optionId }, options.map { (it as Map<*, *>)["id"] })
        assertEquals("Looking at the form handler", message["text"])
    }

    @Test
    fun longReplyKeepsItsEnd() {
        val reply = "word ".repeat(400) + "Shall I open a PR?"

        val text = watchText(reply)

        assertTrue(text.length <= WATCH_TEXT_LIMIT + 3)
        assertTrue(text.endsWith("Shall I open a PR?"))
    }
}
