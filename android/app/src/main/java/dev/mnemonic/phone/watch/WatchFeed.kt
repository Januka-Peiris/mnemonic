package dev.mnemonic.phone.watch

import dev.mnemonic.phone.api.TeamAgentState
import dev.mnemonic.phone.api.TeamWorkspace
import dev.mnemonic.phone.chat.ChatItem
import dev.mnemonic.phone.chat.ChatState
import dev.mnemonic.phone.home.NeedsYouItem
import dev.mnemonic.phone.home.needsYou

/** Must match the id in garmin/manifest.xml. */
const val WATCH_APP_ID = "163dee77350747308e7952aee33d65ec"

// Every message crosses Bluetooth through Garmin Connect, and the watch app
// has little memory, so the watch gets the end of a reply and a short list.
const val WATCH_CHAT_LIMIT = 10
const val WATCH_TEXT_LIMIT = 800
private const val WATCH_OPTION_LIMIT = 4

data class WatchRow(val chatId: String, val title: String, val status: String)

fun watchRows(workspaces: List<TeamWorkspace>, openedAt: (String) -> Long, installedAt: Long): List<WatchRow> {
    val needs = needsYou(workspaces, openedAt, installedAt).map { item ->
        val status = when (item) {
            is NeedsYouItem.Waiting -> "Needs you"
            is NeedsYouItem.Finished -> if (item.errored) "Failed" else "Finished"
        }
        WatchRow(item.chat.id, item.workspace.name, status)
    }
    val shown = needs.map { it.chatId }.toSet()
    val others = workspaces
        .flatMap { workspace -> workspace.chats.filter { it.open && it.id !in shown }.map { workspace to it } }
        .sortedByDescending { (_, chat) -> chat.lastTurnEndedAt ?: 0L }
        .map { (workspace, chat) ->
            WatchRow(chat.id, workspace.name, if (workspace.agentState == TeamAgentState.Working) "Working" else chat.agent)
        }
    return (needs + others).take(WATCH_CHAT_LIMIT)
}

fun listMessage(rows: List<WatchRow>, error: String?): Map<String, Any> =
    mapOf(
        "t" to "list",
        "chats" to rows.map { mapOf("id" to it.chatId, "title" to it.title, "status" to it.status) },
        "error" to error.orEmpty(),
    )

fun chatMessage(chatId: String, title: String, state: ChatState): Map<String, Any> {
    val waiting = state.items.filterIsInstance<ChatItem.Tool>().lastOrNull { it.waiting }
    val options = waiting?.let { state.permissions[it.id] }.orEmpty().take(WATCH_OPTION_LIMIT)
    val reply = state.items.filterIsInstance<ChatItem.AgentMessage>().lastOrNull()?.text.orEmpty()
    return mapOf(
        "t" to "chat",
        "id" to chatId,
        "title" to title,
        "text" to watchText(reply),
        "tool" to waiting?.title.orEmpty(),
        "call" to waiting?.id.orEmpty(),
        "options" to options.map { mapOf("id" to it.optionId, "name" to it.name) },
        "busy" to state.generating,
        "error" to "",
    )
}

fun chatErrorMessage(chatId: String, title: String, error: String): Map<String, Any> =
    mapOf("t" to "chat", "id" to chatId, "title" to title, "error" to error)

fun doneMessage(chatId: String, text: String): Map<String, Any> = mapOf("t" to "done", "id" to chatId, "text" to text)

fun heardMessage(chatId: String, text: String, error: String): Map<String, Any> =
    mapOf("t" to "heard", "id" to chatId, "text" to text, "error" to error)

/** The end of a reply is what asks you something, so a long one keeps its end. */
fun watchText(markdown: String): String {
    val plain = markdown
        .replace("```", "")
        .replace("**", "")
        .replace("`", "")
        .lines()
        .joinToString("\n") { it.trimEnd() }
        .replace(Regex("\n{3,}"), "\n\n")
        .trim()
    if (plain.length <= WATCH_TEXT_LIMIT) return plain
    val tail = plain.takeLast(WATCH_TEXT_LIMIT)
    val start = tail.indexOf(' ').takeIf { it in 0..40 } ?: 0
    return "..." + tail.substring(start).trimStart()
}

sealed interface WatchRequest {
    data object Hello : WatchRequest
    data class Open(val chatId: String) : WatchRequest
    data object Close : WatchRequest
    data class Answer(val chatId: String, val toolCallId: String, val optionId: String) : WatchRequest
    data class Send(val chatId: String, val text: String) : WatchRequest
    data class Listen(val chatId: String) : WatchRequest
    data class FinishListening(val chatId: String) : WatchRequest
    data object CancelListening : WatchRequest
    data class Stop(val chatId: String) : WatchRequest
}

fun parseWatchRequest(message: Any?): WatchRequest? {
    val fields = message as? Map<*, *> ?: return null
    fun field(name: String): String = fields[name] as? String ?: ""
    val chatId = field("id")
    return when (field("op")) {
        "hello" -> WatchRequest.Hello
        "close" -> WatchRequest.Close
        "open" -> chatId.takeIf { it.isNotEmpty() }?.let { WatchRequest.Open(it) }
        "answer" -> WatchRequest.Answer(chatId, field("call"), field("option"))
            .takeIf { chatId.isNotEmpty() && it.toolCallId.isNotEmpty() && it.optionId.isNotEmpty() }
        "send" -> WatchRequest.Send(chatId, field("text")).takeIf { chatId.isNotEmpty() && it.text.isNotBlank() }
        "listen" -> chatId.takeIf { it.isNotEmpty() }?.let { WatchRequest.Listen(it) }
        "finish" -> chatId.takeIf { it.isNotEmpty() }?.let { WatchRequest.FinishListening(it) }
        "cancel" -> WatchRequest.CancelListening
        "stop" -> chatId.takeIf { it.isNotEmpty() }?.let { WatchRequest.Stop(it) }
        else -> null
    }
}
