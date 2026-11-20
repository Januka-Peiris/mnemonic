package dev.mnemonic.phone.chat

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class PendingMessagesTest {
    private fun texts(list: List<PendingMessage>) = list.map { it.text }

    @Test
    fun addShowsImmediately() {
        val pending = PendingMessages()
        pending.add("hello")
        assertEquals(listOf("hello"), texts(pending.items))
    }

    @Test
    fun reconcileDropsWhenTranscriptHasIt() {
        val pending = PendingMessages()
        pending.add("hello")
        assertEquals(listOf("hello"), texts(pending.reconcile(emptyList())))
        assertEquals(emptyList<String>(), texts(pending.reconcile(listOf("hello"))))
    }

    @Test
    fun twoSendsKeepOrder() {
        val pending = PendingMessages()
        pending.add("a")
        pending.add("b")
        assertEquals(listOf("b"), texts(pending.reconcile(listOf("a"))))
        assertEquals(emptyList<String>(), texts(pending.reconcile(listOf("a", "b"))))
    }

    @Test
    fun sameTextTwiceNeedsTwoTranscriptMessages() {
        val pending = PendingMessages()
        pending.add("go")
        pending.add("go")
        assertEquals(2, pending.reconcile(emptyList()).size)
        assertEquals(1, pending.reconcile(listOf("go")).size)
        assertEquals(0, pending.reconcile(listOf("go", "go")).size)
    }

    @Test
    fun earlierTranscriptMessagesDoNotClearALaterSend() {
        val pending = PendingMessages()
        pending.add("go", userMessageCount = 1)
        assertEquals(1, pending.reconcile(listOf("go")).size)
        assertEquals(0, pending.reconcile(listOf("go", "go")).size)
    }

    @Test
    fun failKeepsWithError() {
        val pending = PendingMessages()
        val message = pending.add("hello")
        val items = pending.fail(message.id, "Mac said no")
        assertEquals("Mac said no", items.single().error)
        assertEquals("hello", pending.reconcile(emptyList()).single().text)
    }

    @Test
    fun retryClearsError() {
        val pending = PendingMessages()
        val message = pending.add("hello")
        pending.fail(message.id, "Mac said no")
        assertNull(pending.retry(message.id).single().error)
    }
}
