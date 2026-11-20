package dev.mnemonic.phone.chat

data class PendingMessage(val id: Long, val text: String, val error: String? = null)

/** Messages sent from the phone that the transcript doesn't show yet. */
class PendingMessages {
    private var nextId = 1L
    private var seen = 0
    private var list: List<PendingMessage> = emptyList()

    val items: List<PendingMessage> get() = list

    /** [userMessageCount] is how many user messages the transcript held before this send. */
    fun add(text: String, userMessageCount: Int = 0): PendingMessage {
        if (list.isEmpty()) seen = userMessageCount
        val message = PendingMessage(nextId++, text)
        list = list + message
        return message
    }

    fun fail(id: Long, error: String): List<PendingMessage> = update(id) { it.copy(error = error) }

    fun retry(id: Long): List<PendingMessage> = update(id) { it.copy(error = null) }

    /** Drops each pending message once a later user message in [userMessages] has the same text. */
    fun reconcile(userMessages: List<String>): List<PendingMessage> {
        if (userMessages.size < seen) seen = userMessages.size
        val remaining = list.toMutableList()
        for (text in userMessages.drop(seen)) {
            val index = remaining.indexOfFirst { it.text.trim() == text.trim() }
            if (index >= 0) remaining.removeAt(index)
        }
        seen = userMessages.size
        list = remaining
        return list
    }

    private fun update(id: Long, change: (PendingMessage) -> PendingMessage): List<PendingMessage> {
        list = list.map { if (it.id == id) change(it) else it }
        return list
    }
}
