package dev.mnemonic.phone.diff

import org.junit.Assert.assertEquals
import org.junit.Test

class DiffLinesTest {
    @Test
    fun classifiesEachKind() {
        assertEquals(DiffLineKind.File, diffLineKind("diff --git a/x b/x"))
        assertEquals(DiffLineKind.Hunk, diffLineKind("@@ -1,2 +1,2 @@ fn main"))
        assertEquals(DiffLineKind.Added, diffLineKind("+let a = 1;"))
        assertEquals(DiffLineKind.Removed, diffLineKind("-let a = 0;"))
        assertEquals(DiffLineKind.Context, diffLineKind(" unchanged"))
        assertEquals(DiffLineKind.Context, diffLineKind(""))
    }

    @Test
    fun fileHeadersAreNotAddedOrRemoved() {
        assertEquals(DiffLineKind.File, diffLineKind("+++ b/x"))
        assertEquals(DiffLineKind.File, diffLineKind("--- a/x"))
    }
}
