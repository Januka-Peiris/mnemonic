package dev.mnemonic.phone.diff

import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.produceState
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.mnemonic.phone.api.MacClient
import dev.mnemonic.phone.api.MacResult
import dev.mnemonic.phone.home.HomeViewModel

private sealed interface DiffState {
    data object Loading : DiffState
    data class Failed(val reason: String) : DiffState
    data class Loaded(val text: String) : DiffState
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun DiffScreen(workspaceId: String, client: MacClient, onBack: () -> Unit) {
    val state by produceState<DiffState>(DiffState.Loading, workspaceId) {
        value = when (val result = client.diff(workspaceId)) {
            is MacResult.Ok -> DiffState.Loaded(result.value)
            is MacResult.Refused -> DiffState.Failed(result.reason)
            is MacResult.Unreachable -> DiffState.Failed(HomeViewModel.CANT_REACH)
        }
    }
    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("Diff") },
                navigationIcon = { TextButton(onClick = onBack) { Text("Back") } },
            )
        },
    ) { padding ->
        Box(Modifier.fillMaxSize().padding(padding)) {
            when (val current = state) {
                DiffState.Loading -> Text("Loading...", Modifier.padding(16.dp))
                is DiffState.Failed -> Text(
                    current.reason,
                    Modifier.padding(16.dp),
                    color = MaterialTheme.colorScheme.error,
                )
                is DiffState.Loaded -> Text(
                    current.text.ifBlank { "No changes" },
                    modifier = Modifier
                        .fillMaxSize()
                        .verticalScroll(rememberScrollState())
                        .horizontalScroll(rememberScrollState())
                        .padding(12.dp),
                    fontFamily = FontFamily.Monospace,
                    fontSize = 12.sp,
                    softWrap = false,
                )
            }
        }
    }
}
