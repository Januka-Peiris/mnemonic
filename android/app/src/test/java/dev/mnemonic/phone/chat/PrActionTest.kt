package dev.mnemonic.phone.chat

import dev.mnemonic.phone.api.PhonePrStatus
import org.junit.Assert.assertEquals
import org.junit.Test

class PrActionTest {
    private fun openPr(canMerge: Boolean) = PhonePrStatus(
        found = true,
        number = 212,
        title = "Make retry backoff exponential",
        state = "OPEN",
        headRefOid = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678",
        canMerge = canMerge,
        mergeMethod = "squash",
    )

    @Test
    fun noPullRequestOffersCreate() {
        assertEquals(PrAction.CreatePr, prAction(null, agentWorking = false, requestHeld = false))
        assertEquals(PrAction.CreatePr, prAction(PhonePrStatus(), agentWorking = false, requestHeld = false))
    }

    @Test
    fun mergeablePullRequestOffersMergeWithSevenCharacterHead() {
        assertEquals(
            PrAction.Merge(212, "Make retry backoff exponential", "squash", "a1b2c3d"),
            prAction(openPr(canMerge = true), agentWorking = false, requestHeld = false),
        )
    }

    @Test
    fun openPullRequestThatCannotMergeOffersNothing() {
        assertEquals(PrAction.None, prAction(openPr(canMerge = false), agentWorking = false, requestHeld = false))
    }

    @Test
    fun mergedOrClosedPullRequestOffersCreate() {
        for (state in listOf("MERGED", "CLOSED")) {
            val status = openPr(canMerge = false).copy(state = state)
            assertEquals(PrAction.CreatePr, prAction(status, agentWorking = false, requestHeld = false))
        }
    }

    @Test
    fun heldRequestWithNoPullRequestWaits() {
        assertEquals(PrAction.Waiting, prAction(null, agentWorking = true, requestHeld = true))
        assertEquals(PrAction.Waiting, prAction(PhonePrStatus(), agentWorking = false, requestHeld = true))
    }
}
