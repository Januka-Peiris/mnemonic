package dev.mnemonic.phone.ui

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.rounded.ArrowBack
import androidx.compose.material.icons.rounded.ArrowUpward
import androidx.compose.material.icons.rounded.Mic
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

enum class StatusKind { NeedsYou, Working, Finished, Idle, Error }

private data class KindStyle(val glyph: String, val foreground: Color, val background: Color, val label: String)

private fun styleOf(kind: StatusKind): KindStyle =
    when (kind) {
        StatusKind.NeedsYou -> KindStyle("!", MnColors.warning, MnColors.warningSoft, "needs you")
        StatusKind.Working -> KindStyle("◐", MnColors.accentText, MnColors.accentSoft, "working")
        StatusKind.Finished -> KindStyle("✓", MnColors.success, MnColors.successSoft, "new")
        StatusKind.Idle -> KindStyle("○", Color(0xFF3B4250), MnColors.surface, "idle")
        StatusKind.Error -> KindStyle("✕", MnColors.error, MnColors.errorSoft, "error")
    }

@Composable
fun StatusSquare(kind: StatusKind) {
    val style = styleOf(kind)
    Box(
        Modifier.size(22.dp).clip(RoundedCornerShape(6.dp)).background(style.background),
        contentAlignment = Alignment.Center,
    ) {
        Text(style.glyph, color = style.foreground, fontSize = 11.sp)
    }
}

@Composable
fun StatusLabel(text: String, kind: StatusKind) {
    val style = styleOf(kind)
    Text(
        text,
        color = style.foreground,
        fontSize = 10.sp,
        maxLines = 1,
        modifier = Modifier.clip(RoundedCornerShape(4.dp)).background(style.background).padding(horizontal = 6.dp, vertical = 1.dp),
    )
}

@Composable
fun TopBar(
    title: String,
    subtitle: String?,
    onBack: (() -> Unit)?,
    actions: @Composable RowScope.() -> Unit = {},
) {
    Column {
        Row(
            Modifier.fillMaxWidth().padding(horizontal = if (onBack == null) 14.dp else 4.dp, vertical = 6.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            if (onBack != null) {
                IconButton(onClick = onBack) {
                    Icon(Icons.AutoMirrored.Rounded.ArrowBack, contentDescription = "Back", tint = MnColors.textSecondary)
                }
            }
            Column(Modifier.weight(1f)) {
                Text(
                    title,
                    style = if (onBack == null) MaterialTheme.typography.titleLarge else MaterialTheme.typography.titleMedium,
                    color = MnColors.text,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                if (subtitle != null) {
                    Text(
                        subtitle,
                        style = MaterialTheme.typography.bodySmall,
                        color = MnColors.textMuted,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
            }
            actions()
        }
        Box(Modifier.fillMaxWidth().size(width = 0.dp, height = 1.dp).background(MnColors.divider))
    }
}

@Composable
fun Pill(
    text: AnnotatedString,
    accent: Boolean = false,
    leading: @Composable (() -> Unit)? = null,
    onClick: (() -> Unit)? = null,
) {
    val shape = RoundedCornerShape(999.dp)
    Row(
        Modifier
            .clip(shape)
            .background(if (accent) MnColors.accentSoft else MnColors.surface)
            .border(1.dp, if (accent) MnColors.accent else Color(0xFF1F242C), shape)
            .then(if (onClick != null) Modifier.clickable(onClick = onClick) else Modifier)
            .padding(horizontal = 9.dp, vertical = 3.dp),
        horizontalArrangement = Arrangement.spacedBy(6.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        leading?.invoke()
        Text(
            text,
            fontSize = 10.5.sp,
            color = if (accent) MnColors.accentText else MnColors.textSecondary,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
    }
}

@Composable
fun ReplyChip(text: String, onClick: () -> Unit) {
    val shape = RoundedCornerShape(999.dp)
    Text(
        text,
        fontSize = 10.5.sp,
        color = Color(0xFFC3C8D0),
        maxLines = 1,
        modifier = Modifier
            .clip(shape)
            .background(MnColors.surface)
            .border(1.dp, MnColors.border, shape)
            .clickable(onClick = onClick)
            .padding(horizontal = 9.dp, vertical = 3.dp),
    )
}

@Composable
fun Composer(
    value: String,
    onValueChange: (String) -> Unit,
    placeholder: String,
    onSend: () -> Unit,
    onMic: (() -> Unit)?,
    sendEnabled: Boolean,
    onFocused: (() -> Unit)? = null,
) {
    val shape = RoundedCornerShape(999.dp)
    Row(
        Modifier
            .fillMaxWidth()
            .padding(horizontal = 10.dp, vertical = 6.dp)
            .clip(shape)
            .background(MnColors.surface)
            .border(1.dp, MnColors.border, shape)
            .padding(start = 14.dp, top = 5.dp, bottom = 5.dp, end = 5.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Box(Modifier.weight(1f)) {
            if (value.isEmpty()) Text(placeholder, color = MnColors.textMuted, fontSize = 13.sp, maxLines = 1)
            BasicTextField(
                value = value,
                onValueChange = onValueChange,
                textStyle = TextStyle(color = MnColors.text, fontSize = 13.sp),
                cursorBrush = SolidColor(MnColors.accentText),
                maxLines = 4,
                modifier = Modifier.fillMaxWidth().onFocusChanged { if (it.isFocused) onFocused?.invoke() },
            )
        }
        if (onMic != null) {
            IconButton(onClick = onMic, modifier = Modifier.size(28.dp)) {
                Icon(Icons.Rounded.Mic, contentDescription = "Dictate", tint = MnColors.textSecondary, modifier = Modifier.size(18.dp))
            }
        }
        IconButton(
            onClick = onSend,
            enabled = sendEnabled,
            modifier = Modifier
                .size(28.dp)
                .clip(CircleShape)
                .background(if (sendEnabled) MnColors.accent else MnColors.surfaceRaised),
        ) {
            Icon(
                Icons.Rounded.ArrowUpward,
                contentDescription = "Send",
                tint = if (sendEnabled) Color.White else MnColors.textMuted,
                modifier = Modifier.size(16.dp),
            )
        }
    }
}

@Composable
fun PrimaryButton(text: String, onClick: () -> Unit, enabled: Boolean = true, modifier: Modifier = Modifier) {
    val shape = RoundedCornerShape(7.dp)
    Text(
        text,
        color = if (enabled) Color.White else MnColors.textMuted,
        fontSize = 11.sp,
        fontWeight = FontWeight.SemiBold,
        textAlign = TextAlign.Center,
        maxLines = 1,
        overflow = TextOverflow.Ellipsis,
        modifier = modifier
            .clip(shape)
            .background(if (enabled) MnColors.accent else MnColors.surfaceRaised)
            .clickable(enabled = enabled, onClick = onClick)
            .padding(horizontal = 10.dp, vertical = 6.dp),
    )
}

@Composable
fun SecondaryButton(text: String, onClick: () -> Unit, enabled: Boolean = true, modifier: Modifier = Modifier) {
    val shape = RoundedCornerShape(7.dp)
    Text(
        text,
        color = if (enabled) Color(0xFFC3C8D0) else MnColors.textMuted,
        fontSize = 11.sp,
        textAlign = TextAlign.Center,
        maxLines = 1,
        overflow = TextOverflow.Ellipsis,
        modifier = modifier
            .clip(shape)
            .background(MnColors.surfaceRaised)
            .border(BorderStroke(1.dp, MnColors.border), shape)
            .clickable(enabled = enabled, onClick = onClick)
            .padding(horizontal = 10.dp, vertical = 6.dp),
    )
}

@Composable
fun ErrorBar(text: String) {
    Text(
        text,
        color = MnColors.error,
        fontSize = 11.sp,
        modifier = Modifier.fillMaxWidth().background(MnColors.errorSoft).padding(horizontal = 14.dp, vertical = 6.dp),
    )
}

@Composable
fun SectionLabel(text: String) {
    Text(
        text.uppercase(),
        color = MnColors.textMuted,
        fontSize = 10.sp,
        letterSpacing = 0.8.sp,
        maxLines = 1,
        overflow = TextOverflow.Ellipsis,
        modifier = Modifier.fillMaxWidth().padding(start = 14.dp, end = 14.dp, top = 10.dp, bottom = 2.dp),
    )
}
