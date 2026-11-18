package dev.mnemonic.phone.chat

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListState
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshots.Snapshot
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalUriHandler
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.repeatOnLifecycle
import dev.mnemonic.phone.api.MacResult
import dev.mnemonic.phone.api.PermissionOption
import dev.mnemonic.phone.api.PhoneAgent
import dev.mnemonic.phone.api.PlanEntry
import dev.mnemonic.phone.home.workspaceTitle
import dev.mnemonic.phone.voice.MicButton
import dev.mnemonic.phone.voice.appendSpoken
import kotlinx.coroutines.launch

private const val NEAR_BOTTOM_PX = 240

private sealed interface Row {
    data class Item(val item: ChatItem) : Row
    data class Unplaced(val toolCallId: String, val options: List<PermissionOption>) : Row
}

private fun nearBottom(state: LazyListState): Boolean {
    val info = state.layoutInfo
    val last = info.visibleItemsInfo.lastOrNull() ?: return true
    return last.index >= info.totalItemsCount - 1 && last.offset + last.size - info.viewportEndOffset < NEAR_BOTTOM_PX
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ChatScreen(
    viewModel: ChatViewModel,
    onBack: () -> Unit,
    onOpenChat: (String) -> Unit,
    onDiff: (String) -> Unit,
) {
    val state by viewModel.state.collectAsState()
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    val scope = androidx.compose.runtime.rememberCoroutineScope()
    LaunchedEffect(viewModel, lifecycle) {
        lifecycle.repeatOnLifecycle(Lifecycle.State.RESUMED) {
            launch { viewModel.follow() }
            launch { viewModel.watch() }
        }
    }
    DisposableEffect(viewModel) { onDispose { viewModel.markOpened() } }

    var draft by rememberSaveable { mutableStateOf("") }
    var pickingAgent by remember { mutableStateOf(false) }
    var confirmingMerge by remember { mutableStateOf<PrAction.Merge?>(null) }

    val rows = remember(state.items, state.unplaced) {
        state.items.map<ChatItem, Row> { Row.Item(it) } + state.unplaced.map { Row.Unplaced(it.key, it.value) }
    }
    val listState = rememberLazyListState()
    val pinned = remember(rows) { Snapshot.withoutReadObservation { nearBottom(listState) } }
    LaunchedEffect(rows) {
        if (pinned && rows.isNotEmpty()) listState.scrollToItem(rows.size - 1)
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = {
                    Text(
                        state.workspace?.let { workspaceTitle(it) } ?: "Chat",
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                },
                navigationIcon = { TextButton(onClick = onBack) { Text("Back") } },
                actions = {
                    if (state.generating) TextButton(onClick = viewModel::stop) { Text("Stop") }
                },
            )
        },
        bottomBar = {
            Surface(
                tonalElevation = 3.dp,
                modifier = Modifier.windowInsetsPadding(WindowInsets.navigationBars).imePadding(),
            ) {
                Column(Modifier.padding(horizontal = 12.dp, vertical = 8.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        for (reply in QUICK_REPLIES) {
                            OutlinedButton(onClick = { viewModel.send(reply) }, enabled = !state.sending) { Text(reply) }
                        }
                    }
                    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                        OutlinedTextField(
                            value = draft,
                            onValueChange = { draft = it },
                            modifier = Modifier.weight(1f),
                            placeholder = { Text("Message") },
                            maxLines = 5,
                        )
                        MicButton(onSpoken = { draft = appendSpoken(draft, it) })
                        Button(
                            onClick = { viewModel.send(draft) { draft = "" } },
                            enabled = draft.isNotBlank() && !state.sending,
                        ) { Text("Send") }
                    }
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically) {
                        OutlinedButton(onClick = { pickingAgent = true }) { Text("New chat") }
                        OutlinedButton(
                            onClick = {
                                val workspace = state.workspace
                                if (workspace != null) onDiff(workspace.id) else viewModel.reportMissingWorkspace()
                            },
                        ) { Text("Diff") }
                        PrButton(state, viewModel, onMerge = { confirmingMerge = it })
                    }
                }
            }
        },
    ) { padding ->
        Column(Modifier.padding(padding)) {
            state.status?.let {
                Text(it, Modifier.padding(horizontal = 16.dp, vertical = 4.dp), color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
            (state.error ?: state.notice)?.let { message ->
                Row(
                    Modifier.fillMaxWidth().background(MaterialTheme.colorScheme.secondaryContainer).padding(start = 16.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text(
                        message,
                        Modifier.weight(1f),
                        color = if (state.error != null) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.onSecondaryContainer,
                    )
                    TextButton(onClick = viewModel::dismissMessages) { Text("OK") }
                }
            }
            LazyColumn(
                state = listState,
                modifier = Modifier.weight(1f).fillMaxWidth(),
                contentPadding = androidx.compose.foundation.layout.PaddingValues(12.dp),
                verticalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                items(rows.size) { index ->
                    when (val row = rows[index]) {
                        is Row.Item -> ItemView(row.item, state, viewModel)
                        is Row.Unplaced -> PromptCard(
                            title = "Permission needed",
                            options = row.options,
                            enabled = row.toolCallId !in state.answering,
                            onChoose = { viewModel.answer(row.toolCallId, it) },
                            hidden = row.toolCallId in state.answered,
                        )
                    }
                }
            }
        }
    }

    if (pickingAgent) {
        AgentPicker(
            load = viewModel::agents,
            onDismiss = { pickingAgent = false },
            onStart = { agent ->
                pickingAgent = false
                viewModel.newChat(agent, onOpenChat)
            },
        )
    }

    confirmingMerge?.let { merge ->
        AlertDialog(
            onDismissRequest = { confirmingMerge = null },
            title = { Text("Merge #${merge.number}: ${merge.title}") },
            text = { Text("Merge method: ${merge.method}\nHead commit: ${merge.shortHead}") },
            confirmButton = {
                TextButton(onClick = {
                    confirmingMerge = null
                    viewModel.merge()
                }) { Text("Merge") }
            },
            dismissButton = { TextButton(onClick = { confirmingMerge = null }) { Text("Cancel") } },
        )
    }
}

@Composable
private fun PrButton(state: ChatUiState, viewModel: ChatViewModel, onMerge: (PrAction.Merge) -> Unit) {
    val uriHandler = LocalUriHandler.current
    val pr = state.pr
    val busy = state.prBusy
    when {
        pr == null && state.prLoading -> OutlinedButton(onClick = {}, enabled = false) { Text("PR") }
        pr == null && state.prFailed -> OutlinedButton(onClick = viewModel::retryPr) { Text("PR") }
        state.workspace == null -> OutlinedButton(onClick = {}, enabled = false) { Text("PR") }
        else -> when (val action = state.prAction) {
            PrAction.CreatePr -> OutlinedButton(onClick = viewModel::createPr, enabled = !busy) { Text("Create PR") }
            PrAction.Waiting -> OutlinedButton(onClick = {}, enabled = false) { Text("PR requested") }
            is PrAction.Merge -> Button(onClick = { onMerge(action) }, enabled = !busy) {
                Text("PR #${action.number}: Merge")
            }
            PrAction.None -> OutlinedButton(
                onClick = { pr?.url?.takeIf { it.startsWith("https://") || it.startsWith("http://") }?.let(uriHandler::openUri) },
            ) { Text(openPrLabel(pr, state.workspace.prState)) }
        }
    }
}

private fun openPrLabel(pr: dev.mnemonic.phone.api.PhonePrStatus?, workspaceState: String?): String {
    val name = "PR #${pr?.number}"
    val description = if (pr?.isDraft == true) "draft" else workspaceState ?: pr?.state.orEmpty().lowercase()
    return if (description.isEmpty()) name else "$name $description"
}

@Composable
private fun AgentPicker(
    load: suspend () -> MacResult<List<PhoneAgent>>,
    onDismiss: () -> Unit,
    onStart: (String) -> Unit,
) {
    val result by produceState<MacResult<List<PhoneAgent>>?>(null) { value = load() }
    var chosen by remember { mutableStateOf<String?>(null) }
    val agents = (result as? MacResult.Ok)?.value.orEmpty()
    val selected = chosen ?: agents.firstOrNull()?.id
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("New chat") },
        text = {
            Column {
                when (val current = result) {
                    null -> Text("Loading...")
                    is MacResult.Refused -> Text(current.reason, color = MaterialTheme.colorScheme.error)
                    is MacResult.Unreachable -> Text(dev.mnemonic.phone.home.HomeViewModel.CANT_REACH, color = MaterialTheme.colorScheme.error)
                    is MacResult.Ok -> if (agents.isEmpty()) Text("No agents are enabled on the Mac")
                }
                for (agent in agents) {
                    Row(Modifier.fillMaxWidth().clickable { chosen = agent.id }, verticalAlignment = Alignment.CenterVertically) {
                        RadioButton(selected = selected == agent.id, onClick = { chosen = agent.id })
                        Text(agent.label)
                    }
                }
            }
        },
        confirmButton = {
            TextButton(onClick = { selected?.let(onStart) }, enabled = selected != null) { Text("Start") }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

private fun statusLabel(status: String): String =
    when (status) {
        "pending" -> "pending"
        "in_progress" -> "running"
        "completed" -> "done"
        else -> status
    }

@Composable
private fun ToolLine(tool: ChatItem.Tool, waiting: Boolean) {
    Row(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.Top) {
        Text("[${tool.kind.replace('_', ' ')}]", style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
        Text(tool.title, Modifier.weight(1f), style = MaterialTheme.typography.bodySmall, fontFamily = FontFamily.Monospace)
        Text(
            if (waiting) "waiting" else statusLabel(tool.status),
            style = MaterialTheme.typography.labelSmall,
            color = when {
                waiting -> MaterialTheme.colorScheme.tertiary
                tool.status == "failed" -> MaterialTheme.colorScheme.error
                tool.status == "in_progress" -> MaterialTheme.colorScheme.primary
                else -> MaterialTheme.colorScheme.onSurfaceVariant
            },
        )
    }
}

@Composable
private fun ItemView(item: ChatItem, state: ChatUiState, viewModel: ChatViewModel) {
    when (item) {
        is ChatItem.UserMessage -> Column(
            Modifier.fillMaxWidth().background(MaterialTheme.colorScheme.secondaryContainer).padding(10.dp),
        ) {
            val color = MaterialTheme.colorScheme.onSecondaryContainer
            Text("you", style = MaterialTheme.typography.labelSmall, color = color)
            Text(item.text, style = MaterialTheme.typography.bodyMedium, color = color)
        }
        is ChatItem.AgentMessage -> Column(Modifier.fillMaxWidth()) {
            Text("agent", style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
            MarkdownText(item.text)
        }
        is ChatItem.Thought -> {
            var open by rememberSaveable(item.text.hashCode()) { mutableStateOf(false) }
            Column {
                Text(
                    (if (open) "▾ " else "▸ ") + "Thinking",
                    Modifier.clickable { open = !open },
                    style = MaterialTheme.typography.labelMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                if (open) MarkdownText(item.text)
            }
        }
        is ChatItem.Tool -> {
            val waiting = item.waiting && item.id !in state.answered
            Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
                ToolLine(item, waiting)
                if (waiting) {
                    PromptCard(
                        title = item.title.ifEmpty { "The agent" },
                        options = state.permissions[item.id].orEmpty(),
                        enabled = item.id !in state.answering,
                        onChoose = { viewModel.answer(item.id, it) },
                    )
                }
            }
        }
        is ChatItem.FoldedTools -> {
            val key = item.tools.first().id
            val expanded = key in state.expandedFolds
            Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                Text(
                    (if (expanded) "▾ " else "▸ ") + item.label,
                    Modifier.fillMaxWidth().clickable { viewModel.toggleFold(key) }.padding(vertical = 4.dp),
                    style = MaterialTheme.typography.labelLarge,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                if (expanded) item.tools.forEach { ToolLine(it, waiting = false) }
            }
        }
        is ChatItem.Plan -> PlanBox(if (item.done) "Plan done" else "Plan", item.entries)
        is ChatItem.Compaction -> Text(
            "Earlier messages were summarised",
            style = MaterialTheme.typography.labelMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

@Composable
private fun PromptCard(
    title: String,
    options: List<PermissionOption>,
    enabled: Boolean,
    onChoose: (String) -> Unit,
    hidden: Boolean = false,
) {
    if (hidden) return
    Column(
        Modifier.fillMaxWidth().border(1.dp, MaterialTheme.colorScheme.tertiary).padding(10.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Text("$title?", style = MaterialTheme.typography.titleSmall)
        if (options.isEmpty()) {
            Text("Answer this on the Mac", color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        for (option in options) {
            val allows = option.kind == "allow_once" || option.kind == "allow_always"
            if (allows) {
                Button(onClick = { onChoose(option.optionId) }, enabled = enabled, modifier = Modifier.fillMaxWidth()) { Text(option.name) }
            } else {
                OutlinedButton(onClick = { onChoose(option.optionId) }, enabled = enabled, modifier = Modifier.fillMaxWidth()) { Text(option.name) }
            }
        }
    }
}

@Composable
private fun PlanBox(title: String, entries: List<PlanEntry>) {
    Column(Modifier.fillMaxWidth().background(MaterialTheme.colorScheme.surfaceVariant).padding(10.dp)) {
        Text(title, style = MaterialTheme.typography.titleSmall)
        HorizontalDivider(Modifier.padding(vertical = 4.dp))
        for (entry in entries) {
            val marker = when (entry.status) {
                "completed" -> "[x]"
                "in_progress" -> "[~]"
                else -> "[ ]"
            }
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(marker, fontFamily = FontFamily.Monospace)
                Text(entry.content, Modifier.weight(1f))
            }
        }
    }
}
