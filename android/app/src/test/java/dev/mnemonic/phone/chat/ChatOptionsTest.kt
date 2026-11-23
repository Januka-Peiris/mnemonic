package dev.mnemonic.phone.chat

import dev.mnemonic.phone.api.PhoneChatOption
import dev.mnemonic.phone.api.PhoneOptionChoice
import org.junit.Assert.assertEquals
import org.junit.Test

class ChatOptionsTest {
    @Test
    fun modelIsSetBeforeEffort() {
        val effort = PhoneChatOption(id = "effort", name = "Effort", kind = "effort")
        val model = PhoneChatOption(id = "model", name = "Model", kind = "model")
        assertEquals(
            listOf(PhoneOptionChoice("model", "sonnet"), PhoneOptionChoice("effort", "max")),
            orderedChoices(listOf(effort, model), mapOf("effort" to "max", "model" to "sonnet")),
        )
    }
}
