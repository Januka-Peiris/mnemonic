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

    @Test
    fun bulletListBecomesListBlock() {
        val blocks = parseBlocks("- a\n- **b**")
        val list = blocks.single() as Block.ListBlock
        assertEquals(false, list.ordered)
        assertEquals(2, list.items.size)
        assertEquals(Inline.Bold(listOf(Inline.Text("b"))), list.items[1].single())
    }

    @Test
    fun numberedList() {
        val list = parseBlocks("3. x\n4. y").single() as Block.ListBlock
        assertEquals(true, list.ordered)
        assertEquals(3, list.start)
        assertEquals(listOf(Inline.Text("x")), list.items[0])
    }

    @Test
    fun dashWithoutSpaceIsParagraph() {
        assertEquals(listOf<Block>(Block.Paragraph(listOf(Inline.Text("-not a list")))), parseBlocks("-not a list"))
    }
}
