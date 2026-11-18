package dev.mnemonic.phone.task

import android.widget.Toast
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
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
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import dev.mnemonic.phone.api.MacClient
import dev.mnemonic.phone.api.MacResult
import dev.mnemonic.phone.api.PhoneAgent
import dev.mnemonic.phone.api.PhoneRepository
import dev.mnemonic.phone.home.HomeViewModel
import dev.mnemonic.phone.voice.MicButton
import dev.mnemonic.phone.voice.appendSpoken
import kotlinx.coroutines.launch

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun NewTaskScreen(
    client: MacClient,
    onBack: () -> Unit,
    onStarted: (chatId: String) -> Unit,
    onNoChat: () -> Unit,
) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    var repositories by remember { mutableStateOf<List<PhoneRepository>>(emptyList()) }
    var agents by remember { mutableStateOf<List<PhoneAgent>>(emptyList()) }
    var repositoryId by rememberSaveable { mutableStateOf<String?>(null) }
    var agentId by rememberSaveable { mutableStateOf<String?>(null) }
    var text by rememberSaveable { mutableStateOf("") }
    var error by remember { mutableStateOf<String?>(null) }
    var starting by remember { mutableStateOf(false) }

    LaunchedEffect(client) {
        when (val result = client.repositories()) {
            is MacResult.Ok -> {
                repositories = result.value.repositories
                if (repositories.isEmpty()) error = "No repositories are added in Mnemonic on the Mac"
            }
            is MacResult.Refused -> error = result.reason
            is MacResult.Unreachable -> error = HomeViewModel.CANT_REACH
        }
        when (val result = client.agents()) {
            is MacResult.Ok -> {
                agents = result.value.agents
                if (agentId == null) agentId = agents.firstOrNull()?.id
                if (agents.isEmpty() && error == null) error = "No agents are enabled on the Mac"
            }
            is MacResult.Refused -> error = result.reason
            is MacResult.Unreachable -> error = HomeViewModel.CANT_REACH
        }
    }

    val repository = repositories.firstOrNull { it.id == repositoryId }
    val agent = agents.firstOrNull { it.id == agentId }
    val canStart = repository != null && agent != null && text.isNotBlank() && !starting

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("New task") },
                navigationIcon = { TextButton(onClick = onBack) { Text("Back") } },
            )
        },
    ) { padding ->
        Column(
            Modifier
                .fillMaxSize()
                .padding(padding)
                .imePadding()
                .verticalScroll(rememberScrollState())
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Picker("Repository", repositories.map { it.id to it.name }, repositoryId) { repositoryId = it }
            Row(verticalAlignment = Alignment.Bottom, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedTextField(
                    value = text,
                    onValueChange = { text = it },
                    modifier = Modifier.weight(1f),
                    label = { Text("What should the agent do?") },
                    minLines = 4,
                )
                MicButton(onSpoken = { text = appendSpoken(text, it) })
            }
            Picker("Agent", agents.map { it.id to it.label }, agentId) { agentId = it }
            error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
            Button(
                onClick = {
                    val chosenRepository = repository ?: return@Button
                    val chosenAgent = agent ?: return@Button
                    starting = true
                    error = null
                    scope.launch {
                        val result = client.startTask(chosenRepository.id, chosenAgent.id, text)
                        starting = false
                        when (result) {
                            is MacResult.Ok -> {
                                val chatId = result.value.chatId
                                if (chatId != null) {
                                    onStarted(chatId)
                                } else {
                                    result.value.message?.let { Toast.makeText(context, it, Toast.LENGTH_LONG).show() }
                                    onNoChat()
                                }
                            }
                            is MacResult.Refused -> error = result.reason
                            is MacResult.Unreachable -> error = HomeViewModel.CANT_REACH
                        }
                    }
                },
                enabled = canStart,
                modifier = Modifier.fillMaxWidth(),
            ) { Text(if (starting) "Starting..." else "Start") }
        }
    }
}

@Composable
private fun Picker(label: String, options: List<Pair<String, String>>, selectedId: String?, onSelect: (String) -> Unit) {
    var open by remember { mutableStateOf(false) }
    Column {
        Text(label, style = MaterialTheme.typography.labelMedium)
        OutlinedButton(onClick = { open = true }, modifier = Modifier.fillMaxWidth()) {
            Text(options.firstOrNull { it.first == selectedId }?.second ?: "Choose")
        }
        DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
            for ((id, name) in options) {
                DropdownMenuItem(
                    text = { Text(name) },
                    onClick = {
                        onSelect(id)
                        open = false
                    },
                )
            }
        }
    }
}
