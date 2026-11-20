package dev.mnemonic.phone.ui

import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.ColorScheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.sp

object MnColors {
    val background = Color(0xFF0B0D10)
    val surface = Color(0xFF14171C)
    val surfaceRaised = Color(0xFF171A20)
    val border = Color(0xFF262B33)
    val pillBorder = Color(0xFF1F242C)
    val idleSquare = Color(0xFF3B4250)
    val textSoft = Color(0xFFC3C8D0)
    val divider = Color(0xFF161A20)
    val text = Color(0xFFD7DAE0)
    val textSecondary = Color(0xFF8B93A1)
    val textMuted = Color(0xFF6B7280)
    val accent = Color(0xFF8B5CF6)
    val accentText = Color(0xFFA78BFA)
    val accentSoft = Color(0xFF1E1638)
    val userBubble = Color(0xFF2A2147)
    val userBubbleText = Color(0xFFEBE6FF)
    val success = Color(0xFF22C55E)
    val successSoft = Color(0xFF0F2A18)
    val warning = Color(0xFFF5A524)
    val warningSoft = Color(0xFF3A2A08)
    val warningCard = Color(0xFF15130C)
    val error = Color(0xFFF87171)
    val errorSoft = Color(0xFF2A1212)
}

val MonoFamily: FontFamily = FontFamily.Monospace

private val MnColorScheme: ColorScheme = darkColorScheme(
    primary = MnColors.accent,
    onPrimary = Color.White,
    primaryContainer = MnColors.accentSoft,
    onPrimaryContainer = MnColors.accentText,
    secondary = MnColors.accentText,
    onSecondary = Color.White,
    background = MnColors.background,
    onBackground = MnColors.text,
    surface = MnColors.background,
    onSurface = MnColors.text,
    surfaceVariant = MnColors.surface,
    onSurfaceVariant = MnColors.textSecondary,
    surfaceContainer = MnColors.surface,
    surfaceContainerHigh = MnColors.surfaceRaised,
    surfaceContainerHighest = MnColors.surfaceRaised,
    outline = MnColors.border,
    outlineVariant = MnColors.divider,
    error = MnColors.error,
    onError = Color.White,
    errorContainer = MnColors.errorSoft,
    onErrorContainer = MnColors.error,
)

private val MnTypography = Typography(
    titleLarge = TextStyle(fontSize = 17.sp, lineHeight = 22.sp, fontWeight = FontWeight.SemiBold),
    titleMedium = TextStyle(fontSize = 13.sp, lineHeight = 18.sp, fontWeight = FontWeight.SemiBold),
    titleSmall = TextStyle(fontSize = 13.sp, lineHeight = 18.sp, fontWeight = FontWeight.SemiBold),
    bodyLarge = TextStyle(fontSize = 13.sp, lineHeight = 19.sp),
    bodyMedium = TextStyle(fontSize = 13.sp, lineHeight = 19.sp),
    bodySmall = TextStyle(fontSize = 11.sp, lineHeight = 15.sp),
    labelLarge = TextStyle(fontSize = 11.sp, lineHeight = 15.sp, fontWeight = FontWeight.Medium),
    labelMedium = TextStyle(fontSize = 11.sp, lineHeight = 15.sp),
    labelSmall = TextStyle(fontSize = 10.sp, lineHeight = 14.sp, letterSpacing = 0.8.sp),
)

@Composable
fun MnemonicTheme(content: @Composable () -> Unit) {
    MaterialTheme(colorScheme = MnColorScheme, typography = MnTypography) {
        Surface(modifier = Modifier.fillMaxSize(), color = MnColors.background, content = content)
    }
}
