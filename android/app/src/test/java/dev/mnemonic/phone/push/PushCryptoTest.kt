package dev.mnemonic.phone.push

import java.util.Base64
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Test

class PushCryptoTest {
    private fun decode(text: String): ByteArray = Base64.getUrlDecoder().decode(text)

    private fun field(json: String, name: String): String =
        Regex("\"$name\"\\s*:\\s*\"([^\"]*)\"").find(json)?.groupValues?.get(1)
            ?: throw AssertionError("fixture has no $name")

    private fun keys(private: String, public: String, auth: String) =
        PushKeys(decode(private), decode(public), decode(auth))

    @Test
    fun decryptsRfc8291Example() {
        val receiver = keys(
            "q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94",
            "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
            "BTBZMqHH6r4Tts7J_aSIgg",
        )
        val body = decode(
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN",
        )

        val plaintext = PushCrypto.decrypt(body, receiver)

        assertEquals("When I grow up, I want to be a watermelon", String(plaintext, Charsets.UTF_8))
    }

    @Test
    fun decryptsWhatTheMacEncrypts() {
        val json = javaClass.getResource("/mac_push_fixture.json")!!.readText()
        val receiver = keys(
            field(json, "receiverPrivate"),
            field(json, "receiverPublic"),
            field(json, "auth"),
        )

        val plaintext = PushCrypto.decrypt(decode(field(json, "body")), receiver)

        assertArrayEquals(decode(field(json, "plaintext")), plaintext)
    }

    @Test
    fun rejectsATamperedBody() {
        val json = javaClass.getResource("/mac_push_fixture.json")!!.readText()
        val receiver = keys(
            field(json, "receiverPrivate"),
            field(json, "receiverPublic"),
            field(json, "auth"),
        )
        val body = decode(field(json, "body"))
        body[body.size - 1] = (body[body.size - 1].toInt() xor 1).toByte()

        assertThrows(IllegalArgumentException::class.java) { PushCrypto.decrypt(body, receiver) }
    }

    @Test
    fun generatedKeysHaveTheRightShape() {
        val generated = PushCrypto.generateKeys()

        assertEquals(32, generated.privateKey.size)
        assertEquals(65, generated.publicKey.size)
        assertEquals(0x04, generated.publicKey[0].toInt())
        assertEquals(16, generated.auth.size)
        assertEquals(87, generated.p256dhBase64Url().length)
        assertEquals(22, generated.authBase64Url().length)
    }
}
