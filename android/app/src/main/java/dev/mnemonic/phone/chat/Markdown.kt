package dev.mnemonic.phone.chat

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.background
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.LinkAnnotation
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextLinkStyles
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.text.withLink
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import java.net.URI

sealed interface Inline {
    data class Text(val text: String) : Inline
    data class Code(val text: String) : Inline
    data class Bold(val children: List<Inline>) : Inline
    data class Link(val text: String, val url: String) : Inline
}

sealed interface Block {
    data class Paragraph(val inlines: List<Inline>) : Block
    data class CodeBlock(val text: String) : Block
    data class ListBlock(val ordered: Boolean, val items: List<List<Inline>>) : Block
}

private val INLINE_PATTERN = Regex(
    "`([^`\\n]+)`|\\*\\*([^*\\n](?:[^\\n]*?[^*\\n])?)\\*\\*|\\[([^\\]\\n]+)\\]\\(([^)\\s]+)\\)|(https?://[^\\s<>()]+)",
)
private val TRAILING_PUNCTUATION = Regex("[.,;:!?'\"]+$")
private val BULLET = Regex("^\\s{0,3}[-*+] +(.*)$")
private val NUMBERED = Regex("^\\s{0,3}\\d+[.)] +(.*)$")
private val FENCE = Regex("^ {0,3}(```+|~~~+)")

private fun isWebUrl(text: String): Boolean =
    try {
        val uri = URI(text)
        (uri.scheme == "http" || uri.scheme == "https") && !uri.host.isNullOrEmpty()
    } catch (error: java.net.URISyntaxException) {
        false
    }

fun parseInline(text: String): List<Inline> {
    val parts = mutableListOf<Inline>()
    var position = 0
    for (match in INLINE_PATTERN.findAll(text)) {
        if (match.range.first > position) parts.add(Inline.Text(text.substring(position, match.range.first)))
        val groups = match.groups
        val code = groups[1]?.value
        val bold = groups[2]?.value
        val linkText = groups[3]?.value
        val linkUrl = groups[4]?.value
        val bareUrl = groups[5]?.value
        when {
            code != null -> parts.add(Inline.Code(code))
            bold != null -> parts.add(Inline.Bold(parseInline(bold)))
            linkText != null && linkUrl != null ->
                parts.add(if (isWebUrl(linkUrl)) Inline.Link(linkText, linkUrl) else Inline.Text(match.value))
            bareUrl != null -> {
                val trimmed = bareUrl.replace(TRAILING_PUNCTUATION, "")
                parts.add(if (isWebUrl(trimmed)) Inline.Link(trimmed, trimmed) else Inline.Text(trimmed))
                if (trimmed.length < bareUrl.length) parts.add(Inline.Text(bareUrl.substring(trimmed.length)))
            }
        }
        position = match.range.last + 1
    }
    if (position < text.length) parts.add(Inline.Text(text.substring(position)))
    return parts
}

fun parseBlocks(text: String): List<Block> {
    val blocks = mutableListOf<Block>()
    val lines = text.split("\n")
    var paragraph = mutableListOf<String>()
    fun flushParagraph() {
        if (paragraph.isNotEmpty()) {
            blocks.add(Block.Paragraph(parseInline(paragraph.joinToString("\n"))))
            paragraph = mutableListOf()
        }
    }
    var index = 0
    while (index < lines.size) {
        val line = lines[index]
        val fence = FENCE.find(line)
        if (fence != null) {
            flushParagraph()
            val marker = fence.groupValues[1]
            val code = mutableListOf<String>()
            index++
            while (index < lines.size && !lines[index].trimStart().startsWith(marker)) {
                code.add(lines[index])
                index++
            }
            blocks.add(Block.CodeBlock(code.joinToString("\n")))
        } else if (line.isBlank()) {
            flushParagraph()
        } else if (BULLET.matches(line) || NUMBERED.matches(line)) {
            flushParagraph()
            val ordered = NUMBERED.matches(line)
            val marker = if (ordered) NUMBERED else BULLET
            val items = mutableListOf<List<Inline>>()
            while (index < lines.size) {
                val item = marker.find(lines[index]) ?: break
                items.add(parseInline(item.groupValues[1]))
                index++
            }
            index--
            blocks.add(Block.ListBlock(ordered, items))
        } else {
            paragraph.add(line)
        }
        index++
    }
    flushParagraph()
    return blocks
}

private fun AnnotatedString.Builder.appendInlines(items: List<Inline>, codeBackground: Color, linkColor: Color) {
    for (item in items) {
        when (item) {
            is Inline.Text -> append(item.text)
            is Inline.Code -> withStyle(
                SpanStyle(fontFamily = FontFamily.Monospace, background = codeBackground),
            ) { append(item.text) }
            is Inline.Bold -> withStyle(SpanStyle(fontWeight = FontWeight.Bold)) {
                appendInlines(item.children, codeBackground, linkColor)
            }
            is Inline.Link -> withLink(
                LinkAnnotation.Url(
                    item.url,
                    TextLinkStyles(SpanStyle(color = linkColor, textDecoration = TextDecoration.Underline)),
                ),
            ) { append(item.text) }
        }
    }
}

@Composable
fun inlineText(inlines: List<Inline>): AnnotatedString {
    val codeBackground = MaterialTheme.colorScheme.surfaceVariant
    val linkColor = MaterialTheme.colorScheme.primary
    return buildAnnotatedString { appendInlines(inlines, codeBackground, linkColor) }
}

@Composable
fun MarkdownText(text: String, modifier: Modifier = Modifier) {
    Column(modifier, verticalArrangement = Arrangement.spacedBy(6.dp)) {
        for (block in parseBlocks(text)) {
            when (block) {
                is Block.Paragraph -> Text(inlineText(block.inlines), style = MaterialTheme.typography.bodyMedium)
                is Block.ListBlock -> Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
                    block.items.forEachIndexed { position, item ->
                        Row {
                            Text(
                                if (block.ordered) "${position + 1}. " else "\u2022 ",
                                style = MaterialTheme.typography.bodyMedium,
                            )
                            Text(inlineText(item), style = MaterialTheme.typography.bodyMedium)
                        }
                    }
                }
                is Block.CodeBlock -> Text(
                    block.text,
                    modifier = Modifier
                        .fillMaxWidth()
                        .background(MaterialTheme.colorScheme.surfaceVariant)
                        .horizontalScroll(rememberScrollState())
                        .padding(8.dp),
                    fontFamily = FontFamily.Monospace,
                    fontSize = 13.sp,
                    softWrap = false,
                )
            }
        }
    }
}
