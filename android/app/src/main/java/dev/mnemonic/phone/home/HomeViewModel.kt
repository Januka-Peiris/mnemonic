package dev.mnemonic.phone.home

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import dev.mnemonic.phone.AppState
import dev.mnemonic.phone.AppState.Companion.installedAt
import dev.mnemonic.phone.AppState.Companion.openedAt
import dev.mnemonic.phone.api.MacClient
import dev.mnemonic.phone.api.MacResult
import dev.mnemonic.phone.api.PhoneActionOutcome
import dev.mnemonic.phone.api.TeamAgentState
import dev.mnemonic.phone.api.TeamChatUpdate
import dev.mnemonic.phone.api.TeamEntry
import dev.mnemonic.phone.api.TeamPermissionRequest
import dev.mnemonic.phone.api.TeamWorkspace
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.JsonPrimitive

private const val WORKSPACE_POLL_MS = 5000L
private const val BACKOFF_START_MS = 2000L
private const val BACKOFF_MAX_MS = 15000L
private const val PROMPT_REFRESH_MS = 20000L

fun groupWorkspaces(workspaces: List<TeamWorkspace>): List<Pair<String, List<TeamWorkspace>>> {
    val byAttention = compareBy<TeamWorkspace>(
        { attentionRank(it.agentState) },
        { it.idleSeconds ?: Long.MAX_VALUE },
    )
    return workspaces
        .groupBy { it.project }
        .toSortedMap(String.CASE_INSENSITIVE_ORDER.thenComparing(naturalOrder()))
        .map { (project, rows) -> project to rows.sortedWith(byAttention) }
}

private fun attentionRank(state: TeamAgentState): Int =
    when (state) {
        TeamAgentState.WaitingForApproval -> 0
        TeamAgentState.Error -> 1
        TeamAgentState.Working -> 2
        TeamAgentState.Idle -> 3
    }

data class PromptTool(val id: String, val title: String, val kind: String)

data class PermissionPrompt(
    val request: TeamPermissionRequest?,
    val tool: PromptTool?,
    val fetchedAt: Long,
)

fun promptFrom(update: TeamChatUpdate, fetchedAt: Long): PermissionPrompt {
    val request = update.permissions.firstOrNull()
    var found: PromptTool? = null
    if (request != null) {
        for (change in update.changes) {
            val updates = (change.entry as? TeamEntry.Updates)?.updates ?: continue
            var current: PromptTool? = null
            for (item in updates) {
                when (item.text("sessionUpdate")) {
                    "tool_call" -> current = PromptTool(
                        id = item.text("toolCallId").orEmpty(),
                        title = item.text("title").orEmpty(),
                        kind = item.text("kind")?.takeIf { it.isNotEmpty() } ?: "other",
                    )
                    "tool_call_update" -> if (current != null && current.id == item.text("toolCallId")) {
                        current = current.copy(
                            title = item.text("title")?.takeIf { it.isNotEmpty() } ?: current.title,
                            kind = item.text("kind")?.takeIf { it.isNotEmpty() } ?: current.kind,
                        )
                    }
                }
            }
            if (current != null && current.id == request.toolCallId) found = current
        }
    }
    return PermissionPrompt(request, found, fetchedAt)
}

private fun JsonObject.text(key: String): String? = (this[key] as? JsonPrimitive)?.contentOrNull

data class HomeUiState(
    val workspaces: List<TeamWorkspace> = emptyList(),
    val loaded: Boolean = false,
    val refreshing: Boolean = false,
    val banner: String? = null,
    val answerError: String? = null,
    val prompts: Map<String, PermissionPrompt> = emptyMap(),
    val answering: Set<String> = emptySet(),
    val answered: Set<String> = emptySet(),
    val needsYou: List<NeedsYouItem> = emptyList(),
)

class HomeViewModel(private val client: MacClient, private val appState: AppState) : ViewModel() {
    private val mutableState = MutableStateFlow(HomeUiState())
    val state: StateFlow<HomeUiState> = mutableState

    val lastRepositoryId: Flow<String?> = appState.lastRepositoryId

    suspend fun repositories() = client.repositories()

    suspend fun agents() = client.agents()

    suspend fun startTask(repositoryId: String, agentId: String, text: String): MacResult<PhoneActionOutcome> {
        val result = client.startTask(repositoryId, agentId, text)
        if (result is MacResult.Ok) appState.setLastRepositoryId(repositoryId)
        return result
    }

    private val loadingPrompts = mutableSetOf<String>()

    suspend fun poll() {
        var backoff = BACKOFF_START_MS
        while (true) {
            val reachable = load()
            if (reachable) {
                backoff = BACKOFF_START_MS
                delay(WORKSPACE_POLL_MS)
            } else {
                delay(backoff)
                backoff = minOf(backoff * 2, BACKOFF_MAX_MS)
            }
        }
    }

    fun refresh() {
        viewModelScope.launch {
            mutableState.update { it.copy(refreshing = true, answerError = null) }
            load()
            mutableState.update { it.copy(refreshing = false) }
        }
    }

    fun markOpened(chatId: String, endedAt: Long) {
        viewModelScope.launch { appState.markOpened(chatId, endedAt) }
    }

    fun answer(chatId: String, toolCallId: String, optionId: String) {
        viewModelScope.launch {
            mutableState.update { it.copy(answering = it.answering + chatId, answerError = null) }
            val result = client.answerPermission(chatId, toolCallId, optionId)
            val settled = result is MacResult.Ok || (result is MacResult.Refused && result.status == 409)
            mutableState.update {
                it.copy(
                    answering = it.answering - chatId,
                    answered = if (settled) it.answered + "$chatId $toolCallId" else it.answered,
                    prompts = it.prompts - chatId,
                    answerError = when {
                        settled -> null
                        result is MacResult.Refused -> result.reason
                        else -> CANT_REACH
                    },
                )
            }
            load()
        }
    }

    private suspend fun load(): Boolean {
        when (val result = client.workspaces()) {
            is MacResult.Ok -> {
                val preferences = appState.preferences.first()
                val workspaces = result.value.workspaces
                val items = needsYou(workspaces, { preferences.openedAt(it) }, preferences.installedAt())
                val waiting = items.filterIsInstance<NeedsYouItem.Waiting>().map { it.chat.id }.toSet()
                mutableState.update {
                    it.copy(
                        workspaces = workspaces,
                        loaded = true,
                        banner = null,
                        needsYou = items,
                        prompts = it.prompts.filterKeys { chatId -> chatId in waiting },
                    )
                }
                waiting.forEach { loadPrompt(it) }
                return true
            }
            is MacResult.Refused -> mutableState.update { it.copy(banner = result.reason, loaded = true) }
            is MacResult.Unreachable -> mutableState.update { it.copy(banner = CANT_REACH, loaded = true) }
        }
        return false
    }

    private fun loadPrompt(chatId: String) {
        val cached = mutableState.value.prompts[chatId]
        if (chatId in loadingPrompts) return
        if (cached != null && System.currentTimeMillis() - cached.fetchedAt < PROMPT_REFRESH_MS) return
        loadingPrompts.add(chatId)
        viewModelScope.launch {
            val update = client.chatUpdate(chatId, null, 0)
            loadingPrompts.remove(chatId)
            val prompt = (update as? MacResult.Ok)?.value?.let { promptFrom(it, System.currentTimeMillis()) }
                ?: PermissionPrompt(null, null, System.currentTimeMillis())
            mutableState.update {
                if (it.needsYou.any { item -> item is NeedsYouItem.Waiting && item.chat.id == chatId }) {
                    it.copy(prompts = it.prompts + (chatId to prompt))
                } else {
                    it
                }
            }
        }
    }

    companion object {
        const val CANT_REACH = "Can't reach the Mac. Is Tailscale on?"
    }
}
