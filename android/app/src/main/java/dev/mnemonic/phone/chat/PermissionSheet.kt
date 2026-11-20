package dev.mnemonic.phone.chat

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.mnemonic.phone.api.PermissionOption
import dev.mnemonic.phone.api.TeamPermissionRequest
import dev.mnemonic.phone.ui.MnColors
import dev.mnemonic.phone.ui.MonoFamily
import dev.mnemonic.phone.ui.PrimaryButton
import dev.mnemonic.phone.ui.SecondaryButton
import dev.mnemonic.phone.ui.StatusKind
import dev.mnemonic.phone.ui.StatusLabel

fun permissionTitle(kind: String?): String = when (kind) {
    "execute" -> "Run this command?"
    "edit" -> "Edit this file?"
    else -> "Allow this?"
}

fun permissionButtons(options: List<PermissionOption>): Pair<PermissionOption?, List<PermissionOption>> {
    val primary = options.firstOrNull { it.kind == "allow_once" }
    return primary to options.filter { it !== primary }
}

fun sheetPermission(pending: List<TeamPermissionRequest>, dismissed: Set<String>): TeamPermissionRequest? =
    pending.firstOrNull { it.toolCallId !in dismissed }

internal fun findTool(items: List<ChatItem>, toolCallId: String): ChatItem.Tool? {
    for (item in items) {
        when (item) {
            is ChatItem.Tool -> if (item.id == toolCallId) return item
            is ChatItem.FoldedTools -> item.tools.firstOrNull { it.id == toolCallId }?.let { return it }
            else -> Unit
        }
    }
    return null
}

@OptIn(ExperimentalMaterial3Api::class, ExperimentalLayoutApi::class)
@Composable
fun PermissionSheet(
    request: TeamPermissionRequest,
    tool: ChatItem.Tool?,
    enabled: Boolean,
    onChoose: (String) -> Unit,
    onDismiss: () -> Unit,
) {
    val (primary, others) = permissionButtons(request.options)
    ModalBottomSheet(
        onDismissRequest = onDismiss,
        containerColor = MnColors.surfaceRaised,
        scrimColor = Color.Black.copy(alpha = 0.35f),
        shape = RoundedCornerShape(topStart = 14.dp, topEnd = 14.dp),
    ) {
        Column(Modifier.fillMaxWidth().navigationBarsPadding().padding(start = 14.dp, end = 14.dp, bottom = 14.dp)) {
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically) {
                StatusLabel("needs you", StatusKind.NeedsYou)
                Text(permissionTitle(tool?.kind), fontSize = 12.sp, color = MnColors.text, fontWeight = FontWeight.SemiBold)
            }
            val shape = RoundedCornerShape(7.dp)
            val command = tool?.title?.takeIf { it.isNotEmpty() }
            if (command != null) {
                Text(
                    if (tool.kind == "execute") "$ $command" else command,
                    Modifier.padding(top = 8.dp).fillMaxWidth().heightIn(max = 160.dp).background(MnColors.background, shape).border(1.dp, MnColors.pillBorder, shape).verticalScroll(rememberScrollState()).padding(horizontal = 9.dp, vertical = 7.dp),
                    fontFamily = MonoFamily,
                    fontSize = 10.5.sp,
                    color = MnColors.textSoft,
                )
            }
            if (request.options.isEmpty()) {
                Text("Answer this on the Mac", Modifier.padding(top = 10.dp), fontSize = 11.sp, color = MnColors.textSecondary)
            } else {
                FlowRow(
                    Modifier.padding(top = 10.dp).fillMaxWidth(),
                    horizontalArrangement = Arrangement.spacedBy(6.dp),
                    verticalArrangement = Arrangement.spacedBy(6.dp),
                ) {
                    primary?.let { PrimaryButton(it.name, onClick = { onChoose(it.optionId) }, enabled = enabled) }
                    for (option in others) SecondaryButton(option.name, onClick = { onChoose(option.optionId) }, enabled = enabled)
                }
            }
        }
    }
}

@Composable
fun NeedsOkBar(onReview: () -> Unit) {
    Row(
        Modifier.fillMaxWidth().background(MnColors.warningSoft).clickable(onClick = onReview).padding(horizontal = 14.dp, vertical = 7.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text("Needs your OK · Review", fontSize = 11.sp, color = MnColors.warning)
    }
}
