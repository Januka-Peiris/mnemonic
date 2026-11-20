package dev.mnemonic.phone.diff

import androidx.compose.foundation.background
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.IntrinsicSize
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.produceState
import androidx.compose.foundation.layout.systemBarsPadding
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.mnemonic.phone.api.MacClient
import dev.mnemonic.phone.api.MacResult
import dev.mnemonic.phone.home.HomeViewModel
import dev.mnemonic.phone.ui.ErrorBar
import dev.mnemonic.phone.ui.MnColors
import dev.mnemonic.phone.ui.MonoFamily
import dev.mnemonic.phone.ui.TopBar

enum class DiffLineKind { File, Hunk, Added, Removed, Context }

fun diffLineKind(line: String): DiffLineKind =
    when {
        line.startsWith("diff --git") || line.startsWith("+++") || line.startsWith("---") -> DiffLineKind.File
        line.startsWith("@@") -> DiffLineKind.Hunk
        line.startsWith("+") -> DiffLineKind.Added
        line.startsWith("-") -> DiffLineKind.Removed
        else -> DiffLineKind.Context
    }

private sealed interface DiffState {
    data object Loading : DiffState
    data class Failed(val reason: String) : DiffState
    data class Loaded(val text: String) : DiffState
}

private val AddedBackground = MnColors.success.copy(alpha = 0.10f)
private val RemovedBackground = MnColors.error.copy(alpha = 0.10f)

@Composable
fun DiffScreen(workspaceId: String, client: MacClient, onBack: () -> Unit) {
    val state by produceState<DiffState>(DiffState.Loading, workspaceId) {
        value = when (val result = client.diff(workspaceId)) {
            is MacResult.Ok -> DiffState.Loaded(result.value)
            is MacResult.Refused -> DiffState.Failed(result.reason)
            is MacResult.Unreachable -> DiffState.Failed(HomeViewModel.CANT_REACH)
        }
    }
    val workspaceName by produceState<String?>(null, workspaceId) {
        val result = client.workspaces()
        value = (result as? MacResult.Ok)?.value?.workspaces?.firstOrNull { it.id == workspaceId }?.name
    }
    Column(Modifier.fillMaxSize().systemBarsPadding()) {
        TopBar(title = "Diff", subtitle = workspaceName, onBack = onBack)
        when (val current = state) {
            DiffState.Loading -> Text("Loading...", Modifier.padding(14.dp), color = MnColors.textMuted, fontSize = 13.sp)
            is DiffState.Failed -> ErrorBar(current.reason)
            is DiffState.Loaded -> {
                if (current.text.isBlank()) {
                    Text("No changes", Modifier.padding(14.dp), color = MnColors.textMuted, fontSize = 13.sp)
                } else {
                    DiffBody(current.text)
                }
            }
        }
    }
}

@Composable
private fun DiffBody(text: String) {
    val lines = text.trimEnd('\n').split('\n')
    Box(Modifier.fillMaxSize().verticalScroll(rememberScrollState()).horizontalScroll(rememberScrollState())) {
        Column(Modifier.width(IntrinsicSize.Max).padding(bottom = 12.dp)) {
            lines.forEach { line -> DiffLine(line) }
        }
    }
}

@Composable
private fun DiffLine(line: String) {
    val kind = diffLineKind(line)
    val background = when (kind) {
        DiffLineKind.File -> MnColors.surface
        DiffLineKind.Added -> AddedBackground
        DiffLineKind.Removed -> RemovedBackground
        else -> Color.Transparent
    }
    val color = when (kind) {
        DiffLineKind.File -> MnColors.text
        DiffLineKind.Hunk -> MnColors.textMuted
        DiffLineKind.Added -> MnColors.success
        DiffLineKind.Removed -> MnColors.error
        DiffLineKind.Context -> MnColors.textSoft
    }
    Text(
        line.ifEmpty { " " },
        color = color,
        fontFamily = MonoFamily,
        fontSize = 11.sp,
        lineHeight = 15.sp,
        softWrap = false,
        modifier = Modifier.fillMaxWidth().background(background).padding(horizontal = 14.dp, vertical = if (kind == DiffLineKind.File) 3.dp else 0.dp),
    )
}
