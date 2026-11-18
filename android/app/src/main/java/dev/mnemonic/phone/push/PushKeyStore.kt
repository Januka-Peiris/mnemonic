package dev.mnemonic.phone.push

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.io.File
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

class PushKeyStore(context: Context) {
    private val file = File(context.applicationContext.filesDir, "push_keys.bin")

    @Synchronized
    fun keys(): PushKeys {
        if (file.exists()) {
            runCatching { return decode(open(file.readBytes())) }
        }
        val fresh = PushCrypto.generateKeys()
        file.writeBytes(seal(encode(fresh)))
        return fresh
    }

    private fun encode(keys: PushKeys): ByteArray = keys.privateKey + keys.publicKey + keys.auth

    private fun decode(bytes: ByteArray): PushKeys {
        require(bytes.size == PRIVATE_LENGTH + PUBLIC_LENGTH + AUTH_LENGTH) { "The stored push keys are the wrong size" }
        return PushKeys(
            privateKey = bytes.copyOfRange(0, PRIVATE_LENGTH),
            publicKey = bytes.copyOfRange(PRIVATE_LENGTH, PRIVATE_LENGTH + PUBLIC_LENGTH),
            auth = bytes.copyOfRange(PRIVATE_LENGTH + PUBLIC_LENGTH, bytes.size),
        )
    }

    private fun seal(plain: ByteArray): ByteArray {
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.ENCRYPT_MODE, secretKey())
        return cipher.iv + cipher.doFinal(plain)
    }

    private fun open(sealed: ByteArray): ByteArray {
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.DECRYPT_MODE, secretKey(), GCMParameterSpec(128, sealed, 0, IV_LENGTH))
        return cipher.doFinal(sealed, IV_LENGTH, sealed.size - IV_LENGTH)
    }

    private fun secretKey(): SecretKey {
        val store = KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }
        (store.getKey(ALIAS, null) as? SecretKey)?.let { return it }
        val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, ANDROID_KEYSTORE)
        generator.init(
            KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build(),
        )
        return generator.generateKey()
    }

    private companion object {
        const val ANDROID_KEYSTORE = "AndroidKeyStore"
        const val ALIAS = "mnemonic_push"
        const val TRANSFORMATION = "AES/GCM/NoPadding"
        const val IV_LENGTH = 12
        const val PRIVATE_LENGTH = 32
        const val PUBLIC_LENGTH = 65
        const val AUTH_LENGTH = 16
    }
}
