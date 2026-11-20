package dev.mnemonic.phone.chat

import dev.mnemonic.phone.api.PermissionOption
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class PermissionButtonsTest {
    private fun option(kind: String) = PermissionOption(optionId = kind, name = kind, kind = kind)

    @Test
    fun allowOnceIsPrimary() {
        val (primary, rest) = permissionButtons(listOf(option("reject_once"), option("allow_once"), option("allow_always")))
        assertEquals("allow_once", primary?.kind)
        assertEquals(listOf("reject_once", "allow_always"), rest.map { it.kind })
    }

    @Test
    fun noAllowOnceMeansAllSecondary() {
        val (primary, rest) = permissionButtons(listOf(option("allow_always"), option("reject_once")))
        assertNull(primary)
        assertEquals(listOf("allow_always", "reject_once"), rest.map { it.kind })
    }

    @Test
    fun fiveOptionsKeepOrder() {
        val options = listOf("a", "reject_once", "allow_once", "b", "allow_always").map { PermissionOption(it, it, if (it.startsWith("allow") || it.startsWith("reject")) it else "other") }
        val (primary, rest) = permissionButtons(options)
        assertEquals("allow_once", primary?.optionId)
        assertEquals(listOf("a", "reject_once", "b", "allow_always"), rest.map { it.optionId })
    }

    @Test
    fun titles() {
        assertEquals("Run this command?", permissionTitle("execute"))
        assertEquals("Edit this file?", permissionTitle("edit"))
        assertEquals("Allow this?", permissionTitle(null))
    }
}
