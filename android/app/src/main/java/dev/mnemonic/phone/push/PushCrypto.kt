package dev.mnemonic.phone.push

import java.math.BigInteger
import java.security.AlgorithmParameters
import java.security.GeneralSecurityException
import java.security.KeyFactory
import java.security.KeyPairGenerator
import java.security.SecureRandom
import java.security.interfaces.ECPrivateKey
import java.security.interfaces.ECPublicKey
import java.security.spec.ECGenParameterSpec
import java.security.spec.ECParameterSpec
import java.security.spec.ECPoint
import java.security.spec.ECPrivateKeySpec
import java.security.spec.ECPublicKeySpec
import java.util.Base64
import javax.crypto.Cipher
import javax.crypto.KeyAgreement
import javax.crypto.Mac
import javax.crypto.spec.GCMParameterSpec
import javax.crypto.spec.SecretKeySpec

data class PushKeys(
    val privateKey: ByteArray,
    val publicKey: ByteArray,
    val auth: ByteArray,
)

fun PushKeys.p256dhBase64Url(): String = PushCrypto.base64Url(publicKey)

fun PushKeys.authBase64Url(): String = PushCrypto.base64Url(auth)

object PushCrypto {
    private const val SALT_LENGTH = 16
    private const val HEADER_LENGTH = SALT_LENGTH + 4 + 1
    private const val PUBLIC_KEY_LENGTH = 65
    private const val TAG_LENGTH = 16
    private const val LAST_RECORD_DELIMITER = 2

    private val curve: ECParameterSpec by lazy {
        val parameters = AlgorithmParameters.getInstance("EC")
        parameters.init(ECGenParameterSpec("secp256r1"))
        parameters.getParameterSpec(ECParameterSpec::class.java)
    }

    fun generateKeys(): PushKeys {
        val generator = KeyPairGenerator.getInstance("EC")
        generator.initialize(ECGenParameterSpec("secp256r1"))
        val pair = generator.generateKeyPair()
        val privateKey = pair.private as ECPrivateKey
        val publicKey = pair.public as ECPublicKey
        val auth = ByteArray(16).also { SecureRandom().nextBytes(it) }
        return PushKeys(
            privateKey = fixedLength(privateKey.s, 32),
            publicKey = encodePoint(publicKey.w),
            auth = auth,
        )
    }

    fun decrypt(body: ByteArray, keys: PushKeys): ByteArray {
        require(body.size > HEADER_LENGTH) { "The push body is too short" }
        val salt = body.copyOfRange(0, SALT_LENGTH)
        val recordSize = body.copyOfRange(SALT_LENGTH, SALT_LENGTH + 4)
            .fold(0L) { total, byte -> (total shl 8) or (byte.toLong() and 0xff) }
        val keyIdLength = body[SALT_LENGTH + 4].toInt() and 0xff
        require(keyIdLength == PUBLIC_KEY_LENGTH) { "The push sender key is $keyIdLength bytes" }
        require(body.size > HEADER_LENGTH + keyIdLength) { "The push body has no content" }
        val senderPublic = body.copyOfRange(HEADER_LENGTH, HEADER_LENGTH + keyIdLength)
        val ciphertext = body.copyOfRange(HEADER_LENGTH + keyIdLength, body.size)
        require(ciphertext.size > TAG_LENGTH) { "The push body is too short" }
        require(recordSize >= ciphertext.size) { "The push body has more than one record" }

        try {
            val secret = KeyAgreement.getInstance("ECDH").run {
                init(KeyFactory.getInstance("EC").generatePrivate(ECPrivateKeySpec(BigInteger(1, keys.privateKey), curve)))
                doPhase(KeyFactory.getInstance("EC").generatePublic(ECPublicKeySpec(decodePoint(senderPublic), curve)), true)
                generateSecret()
            }
            val keyInfo = "WebPush: info".toByteArray() + 0 + keys.publicKey + senderPublic
            val inputKey = hkdf(keys.auth, secret, keyInfo, 32)
            val contentKey = hkdf(salt, inputKey, "Content-Encoding: aes128gcm".toByteArray() + 0, 16)
            val nonce = hkdf(salt, inputKey, "Content-Encoding: nonce".toByteArray() + 0, 12)

            val cipher = Cipher.getInstance("AES/GCM/NoPadding")
            cipher.init(Cipher.DECRYPT_MODE, SecretKeySpec(contentKey, "AES"), GCMParameterSpec(TAG_LENGTH * 8, nonce))
            val padded = cipher.doFinal(ciphertext)

            var end = padded.size
            while (end > 0 && padded[end - 1].toInt() == 0) end--
            require(end > 0 && padded[end - 1].toInt() == LAST_RECORD_DELIMITER) {
                "The push body doesn't end with the last-record delimiter"
            }
            return padded.copyOfRange(0, end - 1)
        } catch (error: GeneralSecurityException) {
            throw IllegalArgumentException("The push body couldn't be decrypted: ${error.message}", error)
        }
    }

    fun base64Url(bytes: ByteArray): String = Base64.getUrlEncoder().withoutPadding().encodeToString(bytes)

    private fun hkdf(salt: ByteArray, inputKey: ByteArray, info: ByteArray, length: Int): ByteArray {
        val extract = Mac.getInstance("HmacSHA256")
        extract.init(SecretKeySpec(salt, "HmacSHA256"))
        val pseudoRandomKey = extract.doFinal(inputKey)
        val expand = Mac.getInstance("HmacSHA256")
        expand.init(SecretKeySpec(pseudoRandomKey, "HmacSHA256"))
        var block = ByteArray(0)
        var output = ByteArray(0)
        var counter = 1
        while (output.size < length) {
            expand.update(block)
            expand.update(info)
            expand.update(counter.toByte())
            block = expand.doFinal()
            output += block
            counter++
        }
        return output.copyOfRange(0, length)
    }

    private fun decodePoint(encoded: ByteArray): ECPoint {
        require(encoded.size == PUBLIC_KEY_LENGTH && encoded[0].toInt() == 4) {
            "The push sender key isn't an uncompressed P-256 point"
        }
        return ECPoint(BigInteger(1, encoded.copyOfRange(1, 33)), BigInteger(1, encoded.copyOfRange(33, 65)))
    }

    private fun encodePoint(point: ECPoint): ByteArray =
        byteArrayOf(4) + fixedLength(point.affineX, 32) + fixedLength(point.affineY, 32)

    private fun fixedLength(value: BigInteger, length: Int): ByteArray {
        val bytes = value.toByteArray()
        val trimmed = if (bytes.size > length) bytes.copyOfRange(bytes.size - length, bytes.size) else bytes
        return ByteArray(length - trimmed.size) + trimmed
    }
}
