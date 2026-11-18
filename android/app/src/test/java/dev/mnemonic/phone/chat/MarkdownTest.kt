package dev.mnemonic.phone.chat

import org.junit.Assert.assertEquals
import org.junit.Test

class MarkdownTest {
    @Test
    fun codeSpanIsSeparatedFromSurroundingText() {
        assertEquals(
            listOf(Inline.Text("run "), Inline.Code("ls"), Inline.Text(" now")),
            parseInline("run `ls` now"),
        )
    }

    @Test
    fun bareUrlLinkExcludesTrailingFullStop() {
        assertEquals(
            listOf(Inline.Text("see "), Inline.Link("https://x.dev/a", "https://x.dev/a"), Inline.Text(".")),
            parseInline("see https://x.dev/a."),
        )
    }
}
