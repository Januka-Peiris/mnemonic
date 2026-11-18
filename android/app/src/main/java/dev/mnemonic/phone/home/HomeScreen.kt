package dev.mnemonic.phone.home

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.Button
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExtendedFloatingActionButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.pulltorefresh.PullToRefreshBox
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.repeatOnLifecycle
import dev.mnemonic.phone.api.TeamAgentState
import dev.mnemonic.phone.api.TeamWorkspace

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

fun workspaceTitle(workspace: TeamWorkspace): String =
    if (workspace.project.isNotEmpty()) "${workspace.project} / ${workspace.name}" else workspace.name

@Composable
private fun stateColor(state: TeamAgentState): Color =
    when (state) {
        TeamAgentState.WaitingForApproval -> MaterialTheme.colorScheme.tertiary
        TeamAgentState.Error -> MaterialTheme.colorScheme.error
        TeamAgentState.Working -> MaterialTheme.colorScheme.primary
        TeamAgentState.Idle -> MaterialTheme.colorScheme.onSurfaceVariant
    }

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun HomeScreen(
    viewModel: HomeViewModel,
    onOpenChat: (String) -> Unit,
    onNewTask: () -> Unit,
    onSettings: () -> Unit,
) {
    val state by viewModel.state.collectAsState()
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    LaunchedEffect(viewModel, lifecycle) {
        lifecycle.repeatOnLifecycle(Lifecycle.State.RESUMED) { viewModel.poll() }
    }
    var menuOpen by remember { mutableStateOf(false) }
    val expanded = rememberSaveable { mutableStateOf(setOf<String>()) }

    fun open(chatId: String) {
        val ended = state.workspaces.flatMap { it.chats }.firstOrNull { it.id == chatId }?.lastTurnEndedAt ?: 0L
        viewModel.markOpened(chatId, ended)
        onOpenChat(chatId)
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("Mnemonic") },
                actions = {
                    TextButton(onClick = { menuOpen = true }) { Text("Menu") }
                    DropdownMenu(expanded = menuOpen, onDismissRequest = { menuOpen = false }) {
                        DropdownMenuItem(
                            text = { Text("Settings") },
                            onClick = {
                                menuOpen = false
                                onSettings()
                            },
                        )
                    }
                },
            )
        },
        floatingActionButton = {
            ExtendedFloatingActionButton(onClick = onNewTask) { Text("New task") }
        },
    ) { padding ->
        PullToRefreshBox(
            isRefreshing = state.refreshing,
            onRefresh = viewModel::refresh,
            modifier = Modifier.fillMaxSize().padding(padding),
        ) {
            LazyColumn(
                modifier = Modifier.fillMaxSize(),
                contentPadding = PaddingValues(bottom = 88.dp),
            ) {
                state.banner?.let { banner ->
                    item {
                        Text(
                            banner,
                            color = MaterialTheme.colorScheme.onErrorContainer,
                            modifier = Modifier
                                .fillMaxWidth()
                                .padding(horizontal = 16.dp, vertical = 4.dp)
                                .background(MaterialTheme.colorScheme.errorContainer)
                                .padding(12.dp),
                        )
                    }
                }
                if (state.needsYou.isNotEmpty()) {
                    item { SectionHeading("Needs you") }
                    items(state.needsYou, key = { "need-" + it.chat.id }) { item ->
                        when (item) {
                            is NeedsYouItem.Waiting -> WaitingCard(item, state, viewModel, ::open)
                            is NeedsYouItem.Finished -> FinishedCard(item, ::open)
                        }
                        HorizontalDivider()
                    }
                }
                if (!state.loaded) {
                    item { Text("Loading...", modifier = Modifier.padding(16.dp)) }
                } else if (state.workspaces.isEmpty()) {
                    item { Text("No workspaces", modifier = Modifier.padding(16.dp)) }
                }
                for ((project, rows) in groupWorkspaces(state.workspaces)) {
                    item(key = "project-$project") { SectionHeading(project.ifEmpty { "Other" }) }
                    items(rows, key = { "ws-" + it.id }) { workspace ->
                        val isExpanded = workspace.id in expanded.value
                        WorkspaceRow(workspace, isExpanded, ::open) {
                            expanded.value = if (isExpanded) expanded.value - workspace.id else expanded.value + workspace.id
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun SectionHeading(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.titleSmall,
        color = MaterialTheme.colorScheme.primary,
        modifier = Modifier.padding(start = 16.dp, end = 16.dp, top = 16.dp, bottom = 4.dp),
    )
}

@Composable
private fun CardLine(marker: String, color: Color, title: String) {
    Row(verticalAlignment = Alignment.CenterVertically) {
        Text(marker, color = color, fontWeight = FontWeight.Bold, modifier = Modifier.padding(end = 8.dp))
        Text(title, maxLines = 1, overflow = TextOverflow.Ellipsis, fontWeight = FontWeight.Medium)
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
    Column(modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 10.dp)) {
        CardLine("!", MaterialTheme.colorScheme.tertiary, workspaceTitle(item.workspace))
        Text(
            if (answered) "answered" else permissionLine(prompt?.toolKind, prompt?.toolTitle != null),
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
        prompt?.toolTitle?.let {
            Text(
                it,
                style = MaterialTheme.typography.bodySmall,
                fontFamily = FontFamily.Monospace,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp), modifier = Modifier.padding(top = 8.dp)) {
            if (request != null && allow != null && deny != null) {
                Button(enabled = !busy, onClick = { viewModel.answer(chatId, request.toolCallId, allow.optionId) }) {
                    Text("Allow once")
                }
                OutlinedButton(enabled = !busy, onClick = { viewModel.answer(chatId, request.toolCallId, deny.optionId) }) {
                    Text("Deny")
                }
            }
            OutlinedButton(onClick = { onOpen(chatId) }) { Text("Open") }
        }
    }
}

@Composable
private fun FinishedCard(item: NeedsYouItem.Finished, onOpen: (String) -> Unit) {
    val title = item.chat.title.ifEmpty { "Untitled chat" }
    Column(modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 10.dp)) {
        CardLine(
            if (item.errored) "x" else "✓",
            if (item.errored) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.primary,
            workspaceTitle(item.workspace),
        )
        Text(
            if (item.errored) "errored: $title" else "finished: $title",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
        OutlinedButton(onClick = { onOpen(item.chat.id) }, modifier = Modifier.padding(top = 8.dp)) { Text("Open") }
    }
}

@Composable
private fun WorkspaceRow(
    workspace: TeamWorkspace,
    expanded: Boolean,
    onOpen: (String) -> Unit,
    onToggle: () -> Unit,
) {
    Column(modifier = Modifier.fillMaxWidth()) {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            modifier = Modifier.fillMaxWidth().clickable(onClick = onToggle).padding(horizontal = 16.dp, vertical = 12.dp),
        ) {
            Text(
                workspace.name,
                modifier = Modifier.weight(1f),
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Text(
                stateLabel(workspace),
                style = MaterialTheme.typography.labelMedium,
                color = stateColor(workspace.agentState),
                maxLines = 1,
                softWrap = false,
                modifier = Modifier.padding(start = 8.dp),
            )
            workspace.prNumber?.let { number ->
                Text(
                    "#$number" + (workspace.prState?.let { " $it" } ?: ""),
                    style = MaterialTheme.typography.labelMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 1,
                    softWrap = false,
                    modifier = Modifier.padding(start = 8.dp),
                )
            }
        }
        if (expanded) {
            for (chat in workspace.chats) {
                TextButton(
                    enabled = chat.open,
                    onClick = { onOpen(chat.id) },
                    modifier = Modifier.padding(start = 16.dp),
                ) {
                    Text(chat.title.ifEmpty { "Untitled chat" } + "  " + chat.agent, maxLines = 1, overflow = TextOverflow.Ellipsis)
                }
            }
        }
        HorizontalDivider()
    }
}
