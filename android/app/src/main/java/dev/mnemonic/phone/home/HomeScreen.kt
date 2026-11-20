package dev.mnemonic.phone.home

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.ArrowUpward
import androidx.compose.material.icons.rounded.Mic
import androidx.compose.material.icons.rounded.Settings
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.pulltorefresh.PullToRefreshBox
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.repeatOnLifecycle
import dev.mnemonic.phone.api.TeamAgentState
import dev.mnemonic.phone.api.TeamWorkspace
import dev.mnemonic.phone.push.AlertStatus
import dev.mnemonic.phone.push.PushRegistration
import dev.mnemonic.phone.push.rememberTurnOnAlerts
import dev.mnemonic.phone.task.NewTaskSheet
import dev.mnemonic.phone.ui.ErrorBar
import dev.mnemonic.phone.ui.MnColors
import dev.mnemonic.phone.ui.MonoFamily
import dev.mnemonic.phone.ui.PrimaryButton
import dev.mnemonic.phone.ui.SecondaryButton
import dev.mnemonic.phone.ui.SectionLabel
import dev.mnemonic.phone.ui.StatusKind
import dev.mnemonic.phone.ui.StatusLabel
import dev.mnemonic.phone.ui.StatusSquare
import dev.mnemonic.phone.ui.TopBar
import okhttp3.OkHttpClient

fun formatIdle(seconds: Long?): String =
    when {
        seconds == null -> "idle"
        seconds < 60 -> "idle now"
        seconds < 3600 -> "idle ${seconds / 60}m"
        seconds < 86400 -> "idle ${seconds / 3600}h"
        else -> "idle ${seconds / 86400}d"
    }

fun stateLabel(workspace: TeamWorkspace): String =
    when (workspace.agentState) {
        TeamAgentState.WaitingForApproval -> "needs you"
        TeamAgentState.Error -> "error"
        TeamAgentState.Working -> "working"
        TeamAgentState.Idle -> formatIdle(workspace.idleSeconds)
    }

fun permissionLine(toolKind: String?, hasTool: Boolean): String =
    when (toolKind) {
        "execute" -> "needs permission to run a command"
        "edit", "delete", "move" -> "needs permission to change files"
        "read", "search" -> "needs permission to read files"
        "fetch" -> "needs permission to fetch from the web"
        else -> if (hasTool) "needs permission to use a tool" else "needs permission"
    }

private fun statusKind(workspace: TeamWorkspace, finished: Boolean): StatusKind =
    when (workspace.agentState) {
        TeamAgentState.WaitingForApproval -> StatusKind.NeedsYou
        TeamAgentState.Error -> StatusKind.Error
        TeamAgentState.Working -> StatusKind.Working
        TeamAgentState.Idle -> if (finished) StatusKind.Finished else StatusKind.Idle
    }

private fun rowDetail(workspace: TeamWorkspace, finished: Boolean): String =
    when {
        finished && workspace.agentState == TeamAgentState.Idle -> "Finished · " + formatIdle(workspace.idleSeconds)
        else -> stateLabel(workspace).replaceFirstChar { it.uppercase() }
    }

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun HomeScreen(
    viewModel: HomeViewModel,
    http: OkHttpClient,
    onOpenChat: (String) -> Unit,
    onSettings: () -> Unit,
) {
    val state by viewModel.state.collectAsState()
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    LaunchedEffect(viewModel, lifecycle) {
        lifecycle.repeatOnLifecycle(Lifecycle.State.RESUMED) { viewModel.poll() }
    }
    val context = LocalContext.current
    val alertStatus by PushRegistration.status.collectAsState()
    val turnOnAlerts = rememberTurnOnAlerts(context, http)
    val expanded = rememberSaveable { mutableStateOf(setOf<String>()) }
    var sheetOpen by rememberSaveable { mutableStateOf(false) }

    fun open(chatId: String) {
        val ended = state.workspaces.flatMap { it.chats }.firstOrNull { it.id == chatId }?.lastTurnEndedAt ?: 0L
        viewModel.markOpened(chatId, ended)
        onOpenChat(chatId)
    }

    val finishedWorkspaces = state.needsYou.filterIsInstance<NeedsYouItem.Finished>().map { it.workspace.id }.toSet()

    Scaffold(containerColor = MnColors.background) { padding ->
        Column(Modifier.fillMaxSize().padding(padding)) {
            TopBar(
                title = "Mnemonic",
                subtitle = null,
                onBack = null,
                actions = {
                    IconButton(onClick = onSettings) {
                        Icon(Icons.Rounded.Settings, contentDescription = "Settings", tint = MnColors.textSecondary, modifier = Modifier.size(20.dp))
                    }
                },
            )
            listOfNotNull(state.banner, state.answerError).forEach { ErrorBar(it) }
            PullToRefreshBox(
                isRefreshing = state.refreshing,
                onRefresh = viewModel::refresh,
                modifier = Modifier.weight(1f).fillMaxWidth(),
            ) {
                LazyColumn(modifier = Modifier.fillMaxSize(), contentPadding = PaddingValues(bottom = 8.dp)) {
                    if (alertStatus == AlertStatus.NeedsPermission) {
                        item {
                            Row(
                                verticalAlignment = Alignment.CenterVertically,
                                modifier = Modifier.fillMaxWidth().padding(horizontal = 14.dp, vertical = 6.dp),
                            ) {
                                Text("Get alerts when an agent needs you", fontSize = 12.sp, color = MnColors.textSecondary, modifier = Modifier.weight(1f))
                                PrimaryButton("Turn on alerts", onClick = turnOnAlerts)
                            }
                        }
                    }
                    if (state.needsYou.isNotEmpty()) {
                        item { SectionLabel("Needs you") }
                        items(state.needsYou, key = { "need-" + it.chat.id }) { item ->
                            when (item) {
                                is NeedsYouItem.Waiting -> WaitingCard(item, state, viewModel, ::open)
                                is NeedsYouItem.Finished -> FinishedRow(item, ::open)
                            }
                        }
                    }
                    if (!state.loaded) {
                        item { Text("Loading...", fontSize = 12.sp, color = MnColors.textMuted, modifier = Modifier.padding(14.dp)) }
                    } else if (state.workspaces.isEmpty()) {
                        item { Text("No workspaces", fontSize = 12.sp, color = MnColors.textMuted, modifier = Modifier.padding(14.dp)) }
                    }
                    for ((project, rows) in groupWorkspaces(state.workspaces)) {
                        item(key = "project-$project") { SectionLabel(project.ifEmpty { "Other" }) }
                        items(rows, key = { "ws-" + it.id }) { workspace ->
                            val isExpanded = workspace.id in expanded.value
                            WorkspaceRow(
                                workspace = workspace,
                                finished = workspace.id in finishedWorkspaces,
                                expanded = isExpanded,
                                onOpen = ::open,
                            ) {
                                val only = workspace.chats.singleOrNull()
                                if (only != null && only.open) {
                                    open(only.id)
                                } else {
                                    expanded.value = if (isExpanded) expanded.value - workspace.id else expanded.value + workspace.id
                                }
                            }
                        }
                    }
                }
            }
            NewTaskBar { sheetOpen = true }
        }
    }

    if (sheetOpen) {
        NewTaskSheet(
            viewModel = viewModel,
            onDismiss = { sheetOpen = false },
            onStarted = { chatId ->
                sheetOpen = false
                onOpenChat(chatId)
            },
        )
    }
}

@Composable
private fun NewTaskBar(onClick: () -> Unit) {
    val shape = RoundedCornerShape(999.dp)
    Row(
        Modifier
            .fillMaxWidth()
            .padding(horizontal = 10.dp, vertical = 6.dp)
            .clip(shape)
            .background(MnColors.surface)
            .border(1.dp, MnColors.border, shape)
            .clickable(onClick = onClick)
            .padding(start = 14.dp, top = 5.dp, bottom = 5.dp, end = 5.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Text("Start a new task...", color = MnColors.textMuted, fontSize = 13.sp, maxLines = 1, modifier = Modifier.weight(1f))
        Icon(Icons.Rounded.Mic, contentDescription = null, tint = MnColors.textSecondary, modifier = Modifier.size(18.dp))
        Box(Modifier.size(28.dp).clip(CircleShape).background(MnColors.accent), contentAlignment = Alignment.Center) {
            Icon(Icons.Rounded.ArrowUpward, contentDescription = "New task", tint = Color.White, modifier = Modifier.size(16.dp))
        }
    }
}

@Composable
private fun WaitingCard(
    item: NeedsYouItem.Waiting,
    state: HomeUiState,
    viewModel: HomeViewModel,
    onOpen: (String) -> Unit,
) {
    val chatId = item.chat.id
    val prompt = state.prompts[chatId]
    val request = prompt?.request?.takeIf { "$chatId ${it.toolCallId}" !in state.answered }
    val answered = prompt?.request != null && request == null
    val allow = request?.options?.firstOrNull { it.kind == "allow_once" }
    val deny = request?.options?.firstOrNull { it.kind == "reject_once" }
    val busy = chatId in state.answering
    val shape = RoundedCornerShape(10.dp)
    val line = if (answered) "answered" else permissionLine(prompt?.tool?.kind, prompt?.tool != null)
    Column(
        Modifier
            .fillMaxWidth()
            .padding(horizontal = 10.dp, vertical = 6.dp)
            .clip(shape)
            .background(MnColors.warningCard)
            .border(1.dp, MnColors.warningSoft, shape)
            .clickable { onOpen(chatId) }
            .padding(horizontal = 10.dp, vertical = 8.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(10.dp)) {
            StatusSquare(StatusKind.NeedsYou)
            Column(Modifier.weight(1f)) {
                Text(
                    item.workspace.name,
                    fontSize = 12.sp,
                    color = MnColors.text,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Text(
                    listOf(item.workspace.project, line).filter { it.isNotEmpty() }.joinToString(" · "),
                    fontSize = 10.5.sp,
                    color = MnColors.textMuted,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
        Column(Modifier.padding(start = 32.dp)) {
            prompt?.tool?.title?.takeIf { it.isNotEmpty() }?.let {
                Text(
                    (if (prompt.tool.kind == "execute") "$ " else "") + it,
                    fontFamily = MonoFamily,
                    fontSize = 11.sp,
                    color = MnColors.textSecondary,
                    maxLines = 2,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.padding(top = 5.dp),
                )
            }
            if (request != null && allow != null && deny != null) {
                Row(horizontalArrangement = Arrangement.spacedBy(6.dp), modifier = Modifier.padding(top = 7.dp)) {
                    PrimaryButton("Allow once", enabled = !busy, onClick = { viewModel.answer(chatId, request.toolCallId, allow.optionId) })
                    SecondaryButton("Deny", enabled = !busy, onClick = { viewModel.answer(chatId, request.toolCallId, deny.optionId) })
                }
            }
        }
    }
}

@Composable
private fun FinishedRow(item: NeedsYouItem.Finished, onOpen: (String) -> Unit) {
    val title = item.chat.title.ifEmpty { "Untitled chat" }
    val kind = if (item.errored) StatusKind.Error else StatusKind.Finished
    Row(
        Modifier.fillMaxWidth().clickable { onOpen(item.chat.id) }.padding(horizontal = 14.dp, vertical = 6.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        StatusSquare(kind)
        Column(Modifier.weight(1f)) {
            Text(item.workspace.name, fontSize = 12.sp, color = MnColors.text, maxLines = 1, overflow = TextOverflow.Ellipsis)
            Text(
                listOf(item.workspace.project, (if (item.errored) "Errored: " else "Finished: ") + title)
                    .filter { it.isNotEmpty() }
                    .joinToString(" · "),
                fontSize = 10.5.sp,
                color = MnColors.textMuted,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
        StatusLabel(if (item.errored) "error" else "new", kind)
    }
}

@Composable
private fun PrLabel(number: Long, state: String?) {
    val (foreground, background) = when (state) {
        "ready", "open" -> MnColors.success to MnColors.successSoft
        "conflicts", "closed", "failed" -> MnColors.error to MnColors.errorSoft
        "merged" -> MnColors.accentText to MnColors.accentSoft
        else -> MnColors.textSecondary to MnColors.surfaceRaised
    }
    Text(
        "#$number" + (state?.let { " $it" } ?: ""),
        fontFamily = MonoFamily,
        fontSize = 9.5.sp,
        color = foreground,
        maxLines = 1,
        softWrap = false,
        modifier = Modifier.clip(RoundedCornerShape(4.dp)).background(background).padding(horizontal = 6.dp, vertical = 1.dp),
    )
}

@Composable
private fun WorkspaceRow(
    workspace: TeamWorkspace,
    finished: Boolean,
    expanded: Boolean,
    onOpen: (String) -> Unit,
    onToggle: () -> Unit,
) {
    Column(modifier = Modifier.fillMaxWidth()) {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(10.dp),
            modifier = Modifier.fillMaxWidth().clickable(onClick = onToggle).padding(horizontal = 14.dp, vertical = 6.dp),
        ) {
            StatusSquare(statusKind(workspace, finished))
            Column(Modifier.weight(1f)) {
                Text(
                    workspace.name,
                    fontSize = 13.sp,
                    color = if (workspace.agentState == TeamAgentState.Idle && !finished) MnColors.textSecondary else MnColors.text,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Text(rowDetail(workspace, finished), fontSize = 10.5.sp, color = MnColors.textMuted, maxLines = 1, overflow = TextOverflow.Ellipsis)
            }
            workspace.prNumber?.let { PrLabel(it, workspace.prState) }
        }
        if (expanded) {
            for (chat in workspace.chats) {
                Text(
                    chat.title.ifEmpty { "Untitled chat" } + "  " + chat.agent,
                    fontSize = 12.sp,
                    color = if (chat.open) MnColors.textSoft else MnColors.textMuted,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier
                        .fillMaxWidth()
                        .clickable(enabled = chat.open) { onOpen(chat.id) }
                        .padding(start = 46.dp, end = 14.dp, top = 7.dp, bottom = 7.dp),
                )
            }
        }
    }
}
