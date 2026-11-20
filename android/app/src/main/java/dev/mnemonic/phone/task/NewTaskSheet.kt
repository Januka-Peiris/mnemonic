package dev.mnemonic.phone.task

import android.widget.Toast
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.ArrowUpward
import androidx.compose.material.icons.rounded.KeyboardArrowDown
import androidx.compose.material.icons.rounded.Mic
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.Text
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.mnemonic.phone.api.MacResult
import dev.mnemonic.phone.api.PhoneAgent
import dev.mnemonic.phone.api.PhoneRepository
import dev.mnemonic.phone.home.HomeViewModel
import dev.mnemonic.phone.ui.ErrorBar
import dev.mnemonic.phone.ui.MenuRow
import dev.mnemonic.phone.ui.MnColors
import dev.mnemonic.phone.ui.MnMenu
import dev.mnemonic.phone.ui.SheetHandle
import dev.mnemonic.phone.voice.appendSpoken
import dev.mnemonic.phone.voice.rememberDictation
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch

fun preselectRepository(repositories: List<PhoneRepository>, lastId: String?): PhoneRepository? =
    repositories.firstOrNull { it.id == lastId } ?: repositories.firstOrNull()

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun NewTaskSheet(
    viewModel: HomeViewModel,
    onDismiss: () -> Unit,
    onStarted: (chatId: String) -> Unit,
) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)
    var repositories by remember { mutableStateOf<List<PhoneRepository>>(emptyList()) }
    var agents by remember { mutableStateOf<List<PhoneAgent>>(emptyList()) }
    var repositoryId by rememberSaveable { mutableStateOf<String?>(null) }
    var agentId by rememberSaveable { mutableStateOf<String?>(null) }
    var text by rememberSaveable { mutableStateOf("") }
    var error by remember { mutableStateOf<String?>(null) }
    var starting by remember { mutableStateOf(false) }
    val focusRequester = remember { FocusRequester() }

    LaunchedEffect(viewModel) {
        when (val result = viewModel.repositories()) {
            is MacResult.Ok -> {
                repositories = result.value.repositories
                if (repositoryId == null) {
                    repositoryId = preselectRepository(repositories, viewModel.lastRepositoryId.first())?.id
                }
                if (repositories.isEmpty()) error = "No repositories are added in Mnemonic on the Mac"
            }
            is MacResult.Refused -> error = result.reason
            is MacResult.Unreachable -> error = HomeViewModel.CANT_REACH
        }
        when (val result = viewModel.agents()) {
            is MacResult.Ok -> {
                agents = result.value.agents
                if (agentId == null) agentId = agents.firstOrNull()?.id
                if (agents.isEmpty() && error == null) error = "No agents are enabled on the Mac"
            }
            is MacResult.Refused -> error = result.reason
            is MacResult.Unreachable -> error = HomeViewModel.CANT_REACH
        }
    }
    LaunchedEffect(Unit) { focusRequester.requestFocus() }

    val repository = repositories.firstOrNull { it.id == repositoryId }
    val agent = agents.firstOrNull { it.id == agentId }
    val canStart = repository != null && agent != null && text.isNotBlank() && !starting
    val dictate = rememberDictation { text = appendSpoken(text, it) }

    fun start() {
        val chosenRepository = repository ?: return
        val chosenAgent = agent ?: return
        starting = true
        error = null
        scope.launch {
            val result = viewModel.startTask(chosenRepository.id, chosenAgent.id, text)
            starting = false
            when (result) {
                is MacResult.Ok -> {
                    val chatId = result.value.chatId
                    if (chatId != null) {
                        onStarted(chatId)
                    } else {
                        result.value.message?.let { Toast.makeText(context, it, Toast.LENGTH_LONG).show() }
                        onDismiss()
                    }
                }
                is MacResult.Refused -> error = result.reason
                is MacResult.Unreachable -> error = HomeViewModel.CANT_REACH
            }
        }
    }

    ModalBottomSheet(
        onDismissRequest = onDismiss,
        sheetState = sheetState,
        containerColor = MnColors.surface,
        shape = RoundedCornerShape(topStart = 16.dp, topEnd = 16.dp),
        dragHandle = { SheetHandle() },
    ) {
        Column(
            Modifier.fillMaxWidth().imePadding().navigationBarsPadding().padding(start = 14.dp, end = 14.dp, bottom = 12.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                DropdownChip(repository?.name ?: "Repository", repositories.map { it.id to it.name }) { repositoryId = it }
                DropdownChip(agent?.label ?: "Agent", agents.map { it.id to it.label }) { agentId = it }
            }
            error?.let { message ->
                Box(Modifier.clip(RoundedCornerShape(6.dp))) { ErrorBar(message) }
            }
            val shape = RoundedCornerShape(12.dp)
            Row(
                Modifier.fillMaxWidth().clip(shape).background(MnColors.background).border(1.dp, MnColors.border, shape)
                    .padding(start = 12.dp, top = 8.dp, bottom = 6.dp, end = 6.dp),
                verticalAlignment = Alignment.Bottom,
                horizontalArrangement = Arrangement.spacedBy(6.dp),
            ) {
                Box(Modifier.weight(1f).padding(bottom = 4.dp)) {
                    if (text.isEmpty()) Text("What should the agent do?", color = MnColors.textMuted, fontSize = 13.sp)
                    BasicTextField(
                        value = text,
                        onValueChange = { text = it },
                        textStyle = TextStyle(color = MnColors.text, fontSize = 13.sp, lineHeight = 19.sp),
                        cursorBrush = SolidColor(MnColors.accentText),
                        minLines = 3,
                        maxLines = 8,
                        modifier = Modifier.fillMaxWidth().focusRequester(focusRequester),
                    )
                }
                if (dictate != null) {
                    IconButton(onClick = dictate, modifier = Modifier.size(28.dp)) {
                        Icon(Icons.Rounded.Mic, contentDescription = "Dictate", tint = MnColors.textSecondary, modifier = Modifier.size(18.dp))
                    }
                }
                Box(
                    Modifier.size(28.dp).clip(CircleShape)
                        .background(if (canStart) MnColors.accent else MnColors.surfaceRaised)
                        .clickable(enabled = canStart, onClick = ::start),
                    contentAlignment = Alignment.Center,
                ) {
                    Icon(
                        Icons.Rounded.ArrowUpward,
                        contentDescription = "Start task",
                        tint = if (canStart) Color.White else MnColors.textMuted,
                        modifier = Modifier.size(16.dp),
                    )
                }
            }
        }
    }
}

@Composable
private fun DropdownChip(label: String, options: List<Pair<String, String>>, onSelect: (String) -> Unit) {
    var open by remember { mutableStateOf(false) }
    val shape = RoundedCornerShape(999.dp)
    Box {
        Row(
            Modifier.clip(shape).background(MnColors.surfaceRaised).border(1.dp, MnColors.border, shape)
                .clickable { open = true }.padding(start = 10.dp, end = 6.dp, top = 4.dp, bottom = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(label, fontSize = 11.sp, color = MnColors.textSoft, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.padding(end = 2.dp))
            Icon(Icons.Rounded.KeyboardArrowDown, contentDescription = null, tint = MnColors.textSecondary, modifier = Modifier.size(14.dp))
        }
        MnMenu(expanded = open, onDismissRequest = { open = false }) {
            for ((id, name) in options) {
                MenuRow(name, onClick = {
                    onSelect(id)
                    open = false
                })
            }
        }
    }
}
