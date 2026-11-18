package dev.mnemonic.phone.push

import dev.mnemonic.phone.api.MacResult
import org.junit.Assert.assertEquals
import org.junit.Test

class PermissionReplyTest {
    @Test
    fun okSaysWhichWayItWent() {
        val ok = MacResult.Ok(Unit)
        assertEquals("Allowed", replyResultText(ok, allowed = true))
        assertEquals("Denied", replyResultText(ok, allowed = false))
    }

    @Test
    fun conflictMeansAlreadyAnswered() {
        assertEquals("Already answered", replyResultText(MacResult.Refused(409, "The agent is no longer waiting"), true))
    }

    @Test
    fun unreachableAndServerErrorsSayTheMacCouldNotBeReached() {
        assertEquals("Couldn't reach the Mac", replyResultText(MacResult.Unreachable("timeout"), true))
        assertEquals("Couldn't reach the Mac", replyResultText(MacResult.Refused(500, "boom"), false))
    }
}
