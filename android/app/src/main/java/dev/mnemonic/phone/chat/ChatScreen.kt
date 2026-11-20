package dev.mnemonic.phone.chat

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.ui.draw.alpha
import androidx.compose.foundation.gestures.scrollBy
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListState
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.Add
import androidx.compose.material.icons.rounded.Archive
import androidx.compose.material.icons.rounded.CallMerge
import androidx.compose.material.icons.rounded.CheckCircle
import androidx.compose.material.icons.rounded.Difference
import androidx.compose.material.icons.rounded.OpenInNew
import androidx.compose.material.icons.rounded.Stop
import androidx.compose.material.icons.rounded.Upload
import androidx.compose.material.icons.rounded.Close
import androidx.compose.material.icons.rounded.Contrast
import androidx.compose.material.icons.rounded.MoreHoriz
import androidx.compose.material.icons.rounded.RadioButtonUnchecked
import androidx.compose.material.icons.rounded.Settings
import androidx.compose.material.icons.rounded.Check
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshots.Snapshot
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalUriHandler
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.repeatOnLifecycle
import dev.mnemonic.phone.api.MacResult
import dev.mnemonic.phone.api.TeamPermissionRequest
import dev.mnemonic.phone.api.PhoneAgent
import dev.mnemonic.phone.api.PlanEntry
import dev.mnemonic.phone.ui.ArchiveDialog
import dev.mnemonic.phone.ui.Composer
import dev.mnemonic.phone.ui.MenuRow
import dev.mnemonic.phone.ui.MnColors
import dev.mnemonic.phone.ui.MnMenu
import dev.mnemonic.phone.ui.MonoFamily
import dev.mnemonic.phone.ui.Pill
import dev.mnemonic.phone.ui.PrimaryButton
import dev.mnemonic.phone.ui.ReplyChip
import dev.mnemonic.phone.ui.SecondaryButton
import dev.mnemonic.phone.ui.StatusKind
import dev.mnemonic.phone.ui.StatusLabel
import dev.mnemonic.phone.ui.TopBar
import dev.mnemonic.phone.voice.appendSpoken
import dev.mnemonic.phone.voice.rememberDictation
import kotlinx.coroutines.launch

private const val NEAR_BOTTOM_PX = 240

private fun nearBottom(state: LazyListState): Boolean {
    val info = state.layoutInfo
    val last = info.visibleItemsInfo.lastOrNull() ?: return true
    return last.index >= info.totalItemsCount - 1 && last.offset + last.size - info.viewportEndOffset < NEAR_BOTTOM_PX
}

private enum class MenuPage { Main, Agents }

@Composable
fun ChatScreen(
    viewModel: ChatViewModel,
    onBack: () -> Unit,
    onOpenChat: (String) -> Unit,
    onDiff: (String) -> Unit,
) {
    val state by viewModel.state.collectAsState()
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    LaunchedEffect(viewModel, lifecycle) {
        lifecycle.repeatOnLifecycle(Lifecycle.State.RESUMED) {
            launch { viewModel.follow() }
            launch { viewModel.watch() }
        }
    }
    DisposableEffect(viewModel) { onDispose { viewModel.markOpened() } }

    var draft by rememberSaveable { mutableStateOf("") }
    var confirmingMerge by remember { mutableStateOf<PrAction.Merge?>(null) }
    var confirmingArchive by remember { mutableStateOf(false) }
    var forceBottom by remember { mutableStateOf(false) }
    val dictate = rememberDictation { draft = appendSpoken(draft, it) }

    val rows = state.items
    val listState = rememberLazyListState()
    val rowCount = rows.size + state.pending.size
    val pinned = remember(rows, state.pending) { Snapshot.withoutReadObservation { nearBottom(listState) } }
    LaunchedEffect(rows, state.pending, forceBottom) {
        if ((pinned || forceBottom) && rowCount > 0) {
            listState.scrollToItem(rowCount - 1)
            listState.scrollBy(100_000_000f)
            forceBottom = false
        }
    }

    val workspace = state.workspace
    val chat = workspace?.chats?.firstOrNull { it.id == viewModel.chatId }
    val title = chat?.title?.takeIf { it.isNotEmpty() } ?: workspace?.name ?: "Chat"
    val subtitle = workspace?.let { listOfNotNull(it.project.takeIf { project -> project.isNotEmpty() }, chat?.agent?.takeIf { agent -> agent.isNotEmpty() }).joinToString(" · ") }
        ?.takeIf { it.isNotEmpty() }
    val pending = remember(state.permissions, state.answered) {
        state.permissions.filterKeys { it !in state.answered }.map { TeamPermissionRequest(it.key, it.value) }
    }
    val pendingPermission = pending.isNotEmpty()
    val sheet = sheetPermission(pending, state.dismissed)
    val status = chatStatus(state.generating, pendingPermission, workspace?.agentState)

    Column(Modifier.fillMaxSize().background(MnColors.background).statusBarsPadding()) {
        TopBar(
            title = title,
            subtitle = subtitle,
            onBack = onBack,
            actions = {
                when (status) {
                    ChatStatus.Working -> StatusLabel("working", StatusKind.Working)
                    ChatStatus.NeedsYou -> StatusLabel("needs you", StatusKind.NeedsYou)
                    ChatStatus.Error -> StatusLabel("error", StatusKind.Error)
                    ChatStatus.Idle -> StatusLabel("idle", StatusKind.Idle)
                }
                ChatMenu(state, viewModel, onDiff, onOpenChat, onMerge = { confirmingMerge = it }, onArchive = { confirmingArchive = true })
            },
        )
        state.status?.let {
            Text(it, Modifier.padding(horizontal = 14.dp, vertical = 4.dp), style = MaterialTheme.typography.bodySmall, color = MnColors.textMuted)
        }
        (state.error ?: state.notice)?.let { message ->
            MessageBar(message, error = state.error != null, onDismiss = viewModel::dismissMessages)
        }
        LazyColumn(
            state = listState,
            modifier = Modifier.weight(1f).fillMaxWidth(),
            contentPadding = PaddingValues(horizontal = 14.dp, vertical = 10.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            items(rows.size) { index -> ItemView(rows[index], state, viewModel) }
            items(state.pending.size, key = { state.pending[it].id }) { index ->
                PendingBubble(state.pending[index], onRetry = { viewModel.retrySend(state.pending[index].id) })
            }
        }
        Column(Modifier.windowInsetsPadding(WindowInsets.navigationBars).imePadding()) {
            if (pendingPermission && sheet == null) NeedsOkBar(viewModel::reopenPermissions)
            Row(
                Modifier.fillMaxWidth().horizontalScroll(rememberScrollState()).padding(horizontal = 10.dp),
                horizontalArrangement = Arrangement.spacedBy(6.dp),
            ) {
                for (reply in QUICK_REPLIES) {
                    ReplyChip(reply, enabled = true) { forceBottom = true
                        viewModel.send(reply)
                    }
                }
            }
            Composer(
                value = draft,
                onValueChange = { draft = it },
                placeholder = "Reply...",
                onSend = {
                    forceBottom = true
                    viewModel.send(draft) { draft = "" }
                },
                onMic = dictate,
                sendEnabled = draft.isNotBlank(),
            )
        }
    }

    sheet?.let { request ->
        key(request.toolCallId) {
            PermissionSheet(
                request = request,
                tool = findTool(state.items, request.toolCallId),
                enabled = request.toolCallId !in state.answering,
                onChoose = { viewModel.answer(request.toolCallId, it) },
                onDismiss = { viewModel.dismissPermission(request.toolCallId) },
            )
        }
    }

    if (confirmingArchive) {
        ArchiveDialog(
            name = workspace?.name ?: "this workspace",
            onConfirm = {
                confirmingArchive = false
                viewModel.archive(onArchived = onBack)
            },
            onCancel = { confirmingArchive = false },
        )
    }

    confirmingMerge?.let { merge ->
        AlertDialog(
            onDismissRequest = { confirmingMerge = null },
            containerColor = MnColors.surfaceRaised,
            shape = RoundedCornerShape(12.dp),
            title = { Text("Merge #${merge.number}: ${merge.title}", style = MaterialTheme.typography.titleMedium, color = MnColors.text) },
            text = {
                Text(
                    "Merge method: ${merge.method}\nHead commit: ${merge.shortHead}",
                    style = MaterialTheme.typography.bodySmall,
                    color = MnColors.textSecondary,
                )
            },
            confirmButton = {
                PrimaryButton("Merge", onClick = {
                    confirmingMerge = null
                    viewModel.merge(merge)
                })
            },
            dismissButton = { SecondaryButton("Cancel", onClick = { confirmingMerge = null }) },
        )
    }
}

@Composable
private fun MessageBar(text: String, error: Boolean, onDismiss: () -> Unit) {
    Row(
        Modifier.fillMaxWidth().background(if (error) MnColors.errorSoft else MnColors.surface).padding(start = 14.dp, end = 6.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            text,
            Modifier.weight(1f).padding(vertical = 6.dp),
            color = if (error) MnColors.error else MnColors.textSoft,
            fontSize = 11.sp,
        )
        Text(
            "OK",
            Modifier.clickable(onClick = onDismiss).padding(horizontal = 10.dp, vertical = 8.dp),
            color = MnColors.textSecondary,
            fontSize = 11.sp,
        )
    }
}

@Composable
private fun ChatMenu(
    state: ChatUiState,
    viewModel: ChatViewModel,
    onDiff: (String) -> Unit,
    onOpenChat: (String) -> Unit,
    onMerge: (PrAction.Merge) -> Unit,
    onArchive: () -> Unit,
) {
    val uriHandler = LocalUriHandler.current
    var open by remember { mutableStateOf(false) }
    var page by remember { mutableStateOf(MenuPage.Main) }
    fun close() {
        open = false
        page = MenuPage.Main
    }
    Box {
    IconButton(onClick = { open = true }) {
        Icon(Icons.Rounded.MoreHoriz, contentDescription = "More", tint = MnColors.textSecondary)
    }
    MnMenu(expanded = open, onDismissRequest = ::close) {
        if (page == MenuPage.Agents) {
            val result by produceState<MacResult<List<PhoneAgent>>?>(null) { value = viewModel.agents() }
            when (val current = result) {
                null -> MenuRow("Loading...", onClick = {}, enabled = false)
                is MacResult.Refused -> LaunchedEffect(current) {
                    close()
                    viewModel.reportError(current.reason)
                }
                is MacResult.Unreachable -> LaunchedEffect(current) {
                    close()
                    viewModel.reportError(dev.mnemonic.phone.home.HomeViewModel.CANT_REACH)
                }
                is MacResult.Ok -> {
                    if (current.value.isEmpty()) MenuRow("No agents are enabled on the Mac", onClick = {}, enabled = false)
                    for (agent in current.value) {
                        MenuRow("New chat with ${agent.label}", icon = Icons.Rounded.Add, onClick = {
                            close()
                            viewModel.newChat(agent.id, onOpenChat)
                        })
                    }
                }
            }
        } else {
            if (state.generating) {
                MenuRow("Stop agent", icon = Icons.Rounded.Stop, onClick = {
                    close()
                    viewModel.stop()
                })
            }
            MenuRow("View diff", icon = Icons.Rounded.Difference, onClick = {
                close()
                val workspace = state.workspace
                if (workspace != null) onDiff(workspace.id) else viewModel.reportMissingWorkspace()
            })
            PrMenuRow(state, viewModel, onClose = ::close, onMerge = onMerge, onOpenUrl = {
                close()
                uriHandler.openUri(it)
            })
            MenuRow("New chat", icon = Icons.Rounded.Add, onClick = { page = MenuPage.Agents })
            MenuRow("Archive workspace", icon = Icons.Rounded.Archive, onClick = {
                close()
                onArchive()
            })
        }
    }
    }
}

@Composable
private fun PrMenuRow(
    state: ChatUiState,
    viewModel: ChatViewModel,
    onClose: () -> Unit,
    onMerge: (PrAction.Merge) -> Unit,
    onOpenUrl: (String) -> Unit,
) {
    val pr = state.pr
    val busy = state.prBusy
    when {
        pr == null && state.prLoading -> MenuRow("PR...", icon = Icons.Rounded.Upload, onClick = {}, enabled = false)
        pr == null && state.prFailed -> MenuRow("Retry PR status", icon = Icons.Rounded.Upload, onClick = {
            onClose()
            viewModel.retryPr()
        })
        state.workspace == null -> MenuRow("PR", icon = Icons.Rounded.Upload, onClick = {}, enabled = false)
        else -> when (val action = state.prAction) {
            PrAction.CreatePr -> MenuRow("Create PR", icon = Icons.Rounded.Upload, onClick = {
                onClose()
                viewModel.createPr()
            }, enabled = !busy, color = MnColors.accentText)
            PrAction.Waiting -> MenuRow("Waiting for PR...", icon = Icons.Rounded.Upload, onClick = {}, enabled = false)
            is PrAction.Merge -> MenuRow("Merge #${action.number}", icon = Icons.Rounded.CallMerge, onClick = {
                onClose()
                onMerge(action)
            }, enabled = !busy, color = MnColors.accentText)
            PrAction.None -> MenuRow(
                openPrLabel(pr, state.workspace.prState),
                icon = Icons.Rounded.OpenInNew,
                onClick = { pr?.url?.takeIf { it.startsWith("https://") || it.startsWith("http://") }?.let(onOpenUrl) },
            )
        }
    }
}

private fun openPrLabel(pr: dev.mnemonic.phone.api.PhonePrStatus?, workspaceState: String?): String {
    val name = "PR #${pr?.number}"
    val description = if (pr?.isDraft == true) "draft" else workspaceState ?: pr?.state.orEmpty().lowercase()
    return if (description.isEmpty()) name else "$name $description"
}

@Composable
private fun ToolStatusIcon(status: String, waiting: Boolean) {
    val (icon, tint) = when {
        waiting -> Icons.Rounded.RadioButtonUnchecked to MnColors.warning
        status == "completed" -> Icons.Rounded.Check to MnColors.success
        status == "failed" -> Icons.Rounded.Close to MnColors.error
        status == "in_progress" -> Icons.Rounded.Contrast to MnColors.accentText
        else -> Icons.Rounded.RadioButtonUnchecked to MnColors.textMuted
    }
    Icon(icon, contentDescription = status, tint = tint, modifier = Modifier.size(12.dp))
}

@Composable
private fun ToolLine(tool: ChatItem.Tool, waiting: Boolean) {
    Row(horizontalArrangement = Arrangement.spacedBy(6.dp), verticalAlignment = Alignment.CenterVertically) {
        ToolStatusIcon(tool.status, waiting)
        Text(
            tool.title.ifEmpty { tool.kind },
            Modifier.weight(1f),
            fontFamily = MonoFamily,
            fontSize = 10.5.sp,
            color = MnColors.textSecondary,
            maxLines = 2,
        )
        if (waiting) Text("waiting", fontSize = 10.sp, color = MnColors.warning)
    }
}

@Composable
private fun PendingBubble(message: PendingMessage, onRetry: () -> Unit) {
    Column(Modifier.fillMaxWidth(), horizontalAlignment = Alignment.End, verticalArrangement = Arrangement.spacedBy(2.dp)) {
        UserBubble(message.text, Modifier.alpha(0.6f))
        message.error?.let { error ->
            Text(error, style = MaterialTheme.typography.bodySmall, color = MnColors.error)
            Text(
                "Retry",
                Modifier.clickable(onClick = onRetry).padding(vertical = 2.dp),
                style = MaterialTheme.typography.labelMedium,
                color = MnColors.accentText,
            )
        }
    }
}

@Composable
private fun UserBubble(text: String, modifier: Modifier = Modifier) {
    BoxWithConstraints(modifier.fillMaxWidth(), contentAlignment = Alignment.CenterEnd) {
        Text(
            text,
            Modifier
                .widthIn(max = maxWidth * 0.8f)
                .background(MnColors.userBubble, RoundedCornerShape(topStart = 16.dp, topEnd = 16.dp, bottomEnd = 4.dp, bottomStart = 16.dp))
                .padding(horizontal = 11.dp, vertical = 7.dp),
            style = MaterialTheme.typography.bodyMedium,
            color = MnColors.userBubbleText,
        )
    }
}

@Composable
private fun ItemView(item: ChatItem, state: ChatUiState, viewModel: ChatViewModel) {
    when (item) {
        is ChatItem.UserMessage -> UserBubble(item.text)
        is ChatItem.AgentMessage -> MarkdownText(item.text, Modifier.fillMaxWidth())
        is ChatItem.Thought -> {
            var open by rememberSaveable(item.text.hashCode()) { mutableStateOf(false) }
            Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                Pill(AnnotatedString(if (open) "Thinking \u25BE" else "Thinking \u203A"), onClick = { open = !open })
                if (open) MarkdownText(item.text, Modifier.padding(start = 4.dp))
            }
        }
        is ChatItem.Tool -> {
            val waiting = item.waiting && item.id !in state.answered
            when {
                waiting -> ToolLine(item, waiting)
                item.status == "in_progress" -> Pill(
                    text = buildAnnotatedString {
                        append("Running ")
                        withStyle(SpanStyle(fontFamily = MonoFamily, color = MnColors.textSoft)) { append(item.title.ifEmpty { item.kind }) }
                    },
                    accent = true,
                    leading = { CircularProgressIndicator(Modifier.size(10.dp), color = MnColors.accentText, strokeWidth = 1.5.dp) },
                )
                else -> ToolLine(item, waiting = false)
            }
        }
        is ChatItem.FoldedTools -> {
            val key = item.tools.first().id
            val expanded = key in state.expandedFolds
            val failed = toolRunFailed(item.tools)
            Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                Pill(
                    text = buildAnnotatedString {
                        append(item.label)
                        if (failed > 0) {
                            append(" \u00B7 ")
                            withStyle(SpanStyle(color = MnColors.error)) { append("$failed failed") }
                        }
                        append(if (expanded) " \u25BE" else " \u203A")
                    },
                    leading = { Icon(Icons.Rounded.Settings, contentDescription = null, tint = MnColors.textSecondary, modifier = Modifier.size(11.dp)) },
                    onClick = { viewModel.toggleFold(key) },
                )
                if (expanded) {
                    Column(Modifier.padding(start = 6.dp), verticalArrangement = Arrangement.spacedBy(3.dp)) {
                        item.tools.forEach { ToolLine(it, waiting = false) }
                    }
                }
            }
        }
        is ChatItem.Plan -> PlanBox(if (item.done) "Plan done" else "Plan", item.entries)
        is ChatItem.Compaction -> Text(
            "Earlier messages were summarised",
            fontSize = 10.5.sp,
            color = MnColors.textMuted,
        )
    }
}

@Composable
private fun PlanBox(title: String, entries: List<PlanEntry>) {
    val shape = RoundedCornerShape(10.dp)
    Column(
        Modifier.fillMaxWidth().background(MnColors.surface, shape).border(1.dp, MnColors.pillBorder, shape).padding(horizontal = 10.dp, vertical = 8.dp),
        verticalArrangement = Arrangement.spacedBy(5.dp),
    ) {
        Text(title, style = MaterialTheme.typography.titleSmall, color = MnColors.text)
        for (entry in entries) {
            Row(horizontalArrangement = Arrangement.spacedBy(7.dp), verticalAlignment = Alignment.Top) {
                val (icon, tint) = when (entry.status) {
                    "completed" -> Icons.Rounded.CheckCircle to MnColors.success
                    "in_progress" -> Icons.Rounded.Contrast to MnColors.accentText
                    else -> Icons.Rounded.RadioButtonUnchecked to MnColors.textMuted
                }
                Icon(icon, contentDescription = entry.status, tint = tint, modifier = Modifier.padding(top = 2.dp).size(13.dp))
                Text(entry.content, Modifier.weight(1f), style = MaterialTheme.typography.bodySmall, color = MnColors.textSoft)
            }
        }
    }
}
