package dev.mnemonic.phone.chat

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import dev.mnemonic.phone.AppState
import dev.mnemonic.phone.api.MacClient
import dev.mnemonic.phone.api.MacResult
import dev.mnemonic.phone.api.PermissionOption
import dev.mnemonic.phone.api.PhoneAgent
import dev.mnemonic.phone.api.PhoneActionOutcome
import dev.mnemonic.phone.api.PhonePrStatus
import dev.mnemonic.phone.api.TeamAgentState
import dev.mnemonic.phone.api.TeamWorkspace
import dev.mnemonic.phone.home.HomeViewModel
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

private const val CHAT_WAIT_SECONDS = 25
private const val FIRST_RETRY_MS = 2000L
private const val LONGEST_RETRY_MS = 15000L
private const val NEW_CHAT_GRACE_MS = 6000L
private const val NEW_CHAT_RETRY_MS = 500L
private const val WORKSPACE_POLL_MS = 5000L
private const val PR_REFRESH_MS = 15000L
private const val PR_REQUEST_START_MS = 30000L

val QUICK_REPLIES = listOf("Yes, go ahead", "Continue", "Open a PR")

sealed interface PrAction {
    data object None : PrAction
    data object CreatePr : PrAction
    data object Waiting : PrAction
    data class Merge(
        val number: Long,
        val title: String,
        val method: String,
        val head: String,
    ) : PrAction {
        val shortHead: String get() = head.take(7)
    }
}

fun prAction(pr: PhonePrStatus?, agentWorking: Boolean, requestHeld: Boolean): PrAction {
    val number = pr?.number
    val open = pr != null && pr.found && number != null && pr.state.orEmpty().uppercase() == "OPEN"
    if (!open) return if (requestHeld) PrAction.Waiting else PrAction.CreatePr
    if (!pr.canMerge) return PrAction.None
    return PrAction.Merge(
        number = number,
        title = pr.title.orEmpty(),
        method = pr.mergeMethod.ifEmpty { "default" },
        head = pr.headRefOid.orEmpty(),
    )
}

/** Keeps Create PR held after a tap, until a PR exists or the agent has stopped working. */
class PrRequestHold {
    private var requestedAt = 0L
    private var active = false
    private var sawWorking = false
    private var stoppedAt: Long? = null

    fun start(now: Long) {
        active = true
        requestedAt = now
        sawWorking = false
        stoppedAt = null
    }

    fun clear() {
        active = false
    }

    /** True while the request is still pending; [statusFetchedAt] is when the PR status was last read. */
    fun held(now: Long, agentWorking: Boolean, statusLoading: Boolean, statusFetchedAt: Long): Boolean {
        if (!active) return false
        if (agentWorking) {
            sawWorking = true
            stoppedAt = null
            return true
        }
        if (!sawWorking) {
            if (now - requestedAt < PR_REQUEST_START_MS) return true
        } else if (stoppedAt == null) {
            stoppedAt = now
            return true
        } else if (statusLoading || statusFetchedAt <= (stoppedAt ?: 0L)) {
            return true
        }
        active = false
        return false
    }

    /** True once the agent stopped but the status last read predates that, so it must be read again. */
    fun needsRefresh(statusFetchedAt: Long): Boolean {
        val stopped = stoppedAt ?: return false
        return active && statusFetchedAt <= stopped
    }
}

data class ChatUiState(
    val items: List<ChatItem> = emptyList(),
    val permissions: Map<String, List<PermissionOption>> = emptyMap(),
    val unplaced: Map<String, List<PermissionOption>> = emptyMap(),
    val generating: Boolean = false,
    val loaded: Boolean = false,
    val status: String? = null,
    val workspace: TeamWorkspace? = null,
    val expandedFolds: Set<String> = emptySet(),
    val answering: Set<String> = emptySet(),
    val answered: Set<String> = emptySet(),
    val dismissed: Set<String> = emptySet(),
    val pr: PhonePrStatus? = null,
    val prFailed: Boolean = false,
    val prLoading: Boolean = true,
    val prAction: PrAction = PrAction.None,
    val prBusy: Boolean = false,
    val sending: Boolean = false,
    val error: String? = null,
    val notice: String? = null,
)

class ChatViewModel(
    val chatId: String,
    private val client: MacClient,
    private val appState: AppState,
) : ViewModel() {
    private val chat = ChatState()
    private val mutableState = MutableStateFlow(ChatUiState())
    val state: StateFlow<ChatUiState> = mutableState

    private val createdAt = System.currentTimeMillis()
    private val hold = PrRequestHold()
    private var prFetchedAt = 0L
    private var prKey: String? = null

    init {
        markOpened()
    }

    fun markOpened() {
        val ended = mutableState.value.workspace?.chats?.firstOrNull { it.id == chatId }?.lastTurnEndedAt ?: 0L
        viewModelScope.launch { withContext(NonCancellable) { appState.markOpened(chatId, ended) } }
    }

    suspend fun follow() {
        var retry = 0L
        while (true) {
            when (val result = client.chatUpdate(chatId, chat.cursor, CHAT_WAIT_SECONDS)) {
                is MacResult.Ok -> {
                    retry = 0L
                    chat.apply(result.value)
                    publish(status = null, loaded = true)
                }
                is MacResult.Refused -> {
                    val starting = result.status == 404 && System.currentTimeMillis() - createdAt < NEW_CHAT_GRACE_MS
                    if (starting) {
                        delay(NEW_CHAT_RETRY_MS)
                    } else {
                        publish(status = result.reason)
                        retry = nextRetry(retry)
                        delay(retry)
                    }
                }
                is MacResult.Unreachable -> {
                    publish(status = "Can't reach the Mac, retrying...")
                    retry = nextRetry(retry)
                    delay(retry)
                }
            }
        }
    }

    suspend fun watch() {
        while (true) {
            loadWorkspace()
            recomputePr()
            delay(if (mutableState.value.workspace == null) NEW_CHAT_RETRY_MS * 2 else WORKSPACE_POLL_MS)
        }
    }

    private fun nextRetry(previous: Long) = if (previous == 0L) FIRST_RETRY_MS else minOf(previous * 2, LONGEST_RETRY_MS)

    private fun publish(status: String?, loaded: Boolean = mutableState.value.loaded) {
        mutableState.update {
            it.copy(
                items = chat.items,
                permissions = chat.permissions,
                unplaced = chat.unplacedPermissions,
                generating = chat.generating,
                status = status,
                loaded = loaded,
            )
        }
        recomputePr()
    }

    private suspend fun loadWorkspace() {
        val result = client.workspaces()
        if (result !is MacResult.Ok) return
        val workspace = result.value.workspaces.firstOrNull { found -> found.chats.any { it.id == chatId } } ?: return
        val first = mutableState.value.workspace == null
        mutableState.update { it.copy(workspace = workspace) }
        if (first) markOpened()
        val key = "${workspace.prNumber} ${workspace.prState}"
        val stale = System.currentTimeMillis() - prFetchedAt >= PR_REFRESH_MS
        if (prKey != key || stale || hold.needsRefresh(prFetchedAt)) {
            prKey = key
            loadPr(workspace.id)
        }
    }

    private suspend fun loadPr(workspaceId: String) {
        mutableState.update { it.copy(prLoading = true) }
        recomputePr()
        val result = client.prStatus(workspaceId)
        prFetchedAt = System.currentTimeMillis()
        mutableState.update {
            when (result) {
                is MacResult.Ok -> it.copy(pr = result.value, prFailed = false, prLoading = false)
                else -> it.copy(prFailed = true, prLoading = false)
            }
        }
        recomputePr()
    }

    private fun recomputePr() {
        val current = mutableState.value
        val working = current.generating || current.workspace?.agentState.let {
            it == TeamAgentState.Working || it == TeamAgentState.WaitingForApproval
        }
        val status = current.pr
        if (status != null && status.found && status.state.orEmpty().uppercase() == "OPEN") hold.clear()
        val held = hold.held(System.currentTimeMillis(), working, current.prLoading, prFetchedAt)
        mutableState.update { it.copy(prAction = prAction(it.pr, working, held)) }
    }

    fun toggleFold(key: String) {
        mutableState.update {
            it.copy(expandedFolds = if (key in it.expandedFolds) it.expandedFolds - key else it.expandedFolds + key)
        }
    }

    fun reportMissingWorkspace() {
        mutableState.update { it.copy(error = "This chat's workspace isn't in the list yet") }
    }

    fun reportError(message: String) {
        mutableState.update { it.copy(error = message) }
    }

    fun dismissMessages() {
        mutableState.update { it.copy(error = null, notice = null) }
    }

    fun dismissPermission(toolCallId: String) {
        mutableState.update { it.copy(dismissed = it.dismissed + toolCallId) }
    }

    fun reopenPermissions() {
        mutableState.update { it.copy(dismissed = emptySet()) }
    }

    fun answer(toolCallId: String, optionId: String) {
        viewModelScope.launch {
            mutableState.update { it.copy(answering = it.answering + toolCallId, error = null) }
            val result = client.answerPermission(chatId, toolCallId, optionId)
            val settled = result is MacResult.Ok || (result is MacResult.Refused && result.status == 409)
            mutableState.update {
                it.copy(
                    answering = it.answering - toolCallId,
                    answered = if (settled) it.answered + toolCallId else it.answered,
                    error = if (settled) null else failure(result),
                )
            }
        }
    }

    fun send(text: String, onSent: () -> Unit = {}) {
        if (text.isBlank()) return
        viewModelScope.launch {
            mutableState.update { it.copy(sending = true, error = null) }
            val result = client.sendMessage(chatId, text)
            mutableState.update { it.copy(sending = false, error = if (result is MacResult.Ok) null else failure(result)) }
            if (result is MacResult.Ok) onSent()
        }
    }

    fun stop() {
        viewModelScope.launch {
            val result = client.stop(chatId)
            if (result !is MacResult.Ok) mutableState.update { it.copy(error = failure(result)) }
        }
    }

    suspend fun agents(): MacResult<List<PhoneAgent>> =
        when (val result = client.agents()) {
            is MacResult.Ok -> MacResult.Ok(result.value.agents)
            is MacResult.Refused -> result
            is MacResult.Unreachable -> result
        }

    fun newChat(agent: String, onOpened: (String) -> Unit) {
        val workspace = mutableState.value.workspace
        if (workspace == null) {
            reportMissingWorkspace()
            return
        }
        viewModelScope.launch {
            val result = client.newChat(workspace.id, agent)
            val chatId = (result as? MacResult.Ok)?.value?.chatId
            if (chatId != null) {
                onOpened(chatId)
            } else {
                mutableState.update { it.copy(error = failure(result) ?: "The Mac didn't open a chat") }
            }
        }
    }

    fun createPr() {
        val workspace = mutableState.value.workspace ?: return
        runPrCall {
            val result = client.createPr(workspace.id)
            if (result is MacResult.Ok) hold.start(System.currentTimeMillis())
            result
        }
    }

    fun merge(confirmed: PrAction.Merge) {
        val workspace = mutableState.value.workspace ?: return
        runPrCall {
            val result = client.merge(workspace.id, confirmed.head)
            if (result is MacResult.Ok) {
                mutableState.update { it.copy(notice = result.value.message ?: "Merged PR #${confirmed.number}") }
            }
            result
        }
    }

    fun retryPr() {
        val workspace = mutableState.value.workspace ?: return
        viewModelScope.launch { loadPr(workspace.id) }
    }

    private fun runPrCall(call: suspend () -> MacResult<PhoneActionOutcome>) {
        viewModelScope.launch {
            mutableState.update { it.copy(prBusy = true, error = null, notice = null) }
            val result = call()
            val message = (result as? MacResult.Ok)?.value?.message
            mutableState.update {
                it.copy(
                    prBusy = false,
                    error = failure(result),
                    notice = it.notice ?: message,
                )
            }
            mutableState.value.workspace?.let { loadPr(it.id) }
        }
    }

    private fun failure(result: MacResult<*>): String? =
        when (result) {
            is MacResult.Ok -> null
            is MacResult.Refused -> result.reason
            is MacResult.Unreachable -> HomeViewModel.CANT_REACH
        }
}
