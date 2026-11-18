package dev.mnemonic.phone.push

import java.util.Base64
import kotlinx.serialization.json.Json
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class AlertPayloadTest {
    private fun decode(text: String): ByteArray = Base64.getUrlDecoder().decode(text)

    private fun field(json: String, name: String): String =
        Regex("\"$name\"\\s*:\\s*\"([^\"]*)\"").find(json)?.groupValues?.get(1)
            ?: throw AssertionError("fixture has no $name")

    @Test
    fun decodesWhatTheMacEncrypts() {
        val json = javaClass.getResource("/mac_push_fixture.json")!!.readText()
        val keys = PushKeys(decode(field(json, "receiverPrivate")), decode(field(json, "receiverPublic")), decode(field(json, "auth")))

        val payload = AlertPayload.parse(PushCrypto.decrypt(decode(field(json, "body")), keys))

        assertEquals("mnemonic / fixture", payload.title)
        assertNull(payload.permission)
    }

    @Test
    fun decodesPermissionIds() {
        val text = """{"title":"t","body":"b","chat_id":"c","url":"/u","permission":{"tool_call_id":"t","allow_option_id":"a","deny_option_id":"d"}}"""

        val payload = AlertPayload.parse(text.toByteArray())

        assertEquals("c", payload.chatId)
        assertEquals(AlertPermission("t", "a", "d"), payload.permission)
    }
}
