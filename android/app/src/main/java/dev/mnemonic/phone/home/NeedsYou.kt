package dev.mnemonic.phone.home

import dev.mnemonic.phone.api.TeamChat
import dev.mnemonic.phone.api.TeamWorkspace

sealed interface NeedsYouItem {
    val workspace: TeamWorkspace
    val chat: TeamChat

    data class Waiting(override val workspace: TeamWorkspace, override val chat: TeamChat) : NeedsYouItem
    data class Finished(
        override val workspace: TeamWorkspace,
        override val chat: TeamChat,
        val errored: Boolean,
    ) : NeedsYouItem
}

fun needsYou(
    workspaces: List<TeamWorkspace>,
    openedAt: (chatId: String) -> Long,
    installedAt: Long,
): List<NeedsYouItem> {
    val waiting = mutableListOf<NeedsYouItem.Waiting>()
    val finished = mutableListOf<NeedsYouItem.Finished>()
    for (workspace in workspaces) {
        for (chat in workspace.chats) {
            if (chat.waiting) {
                waiting.add(NeedsYouItem.Waiting(workspace, chat))
                continue
            }
            val ended = chat.lastTurnEndedAt ?: 0L
            if (ended > installedAt && ended > openedAt(chat.id)) {
                finished.add(NeedsYouItem.Finished(workspace, chat, chat.lastTurnErrored))
            }
        }
    }
    finished.sortByDescending { it.chat.lastTurnEndedAt ?: 0L }
    return waiting + finished
}

fun unreadChat(workspace: TeamWorkspace, items: List<NeedsYouItem>): NeedsYouItem.Finished? =
    items.filterIsInstance<NeedsYouItem.Finished>().firstOrNull { it.workspace.id == workspace.id }
