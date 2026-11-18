package dev.mnemonic.phone.push

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Log
import java.io.File
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

interface KeySealer {
    fun seal(plain: ByteArray): ByteArray
    fun open(sealed: ByteArray): ByteArray
}

class PushKeyStore internal constructor(private val file: File, private val sealer: KeySealer) {
    constructor(context: Context) : this(File(context.applicationContext.filesDir, "push_keys.bin"), KeystoreSealer)

    /** Reads the saved keys only. Null, with a log line, when they are missing or unreadable. */
    fun load(): PushKeys? = synchronized(LOCK) { read() }

    /** Only registration may create keys, because it tells the Mac about them straight after. */
    fun loadOrCreate(): PushKeys = synchronized(LOCK) {
        read() ?: PushCrypto.generateKeys().also { file.writeBytes(sealer.seal(encode(it))) }
    }

    private fun read(): PushKeys? {
        if (!file.exists()) return null
        return try {
            decode(sealer.open(file.readBytes()))
        } catch (error: Exception) {
            Log.w(TAG, "The saved push keys couldn't be read: ${error.message}")
            null
        }
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

    internal companion object {
        val LOCK = Any()
        const val TAG = "MnemonicPush"
        const val PRIVATE_LENGTH = 32
        const val PUBLIC_LENGTH = 65
        const val AUTH_LENGTH = 16
    }
}

private object KeystoreSealer : KeySealer {
    override fun seal(plain: ByteArray): ByteArray {
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.ENCRYPT_MODE, secretKey())
        return cipher.iv + cipher.doFinal(plain)
    }

    override fun open(sealed: ByteArray): ByteArray {
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

    private const val ANDROID_KEYSTORE = "AndroidKeyStore"
    private const val ALIAS = "mnemonic_push"
    private const val TRANSFORMATION = "AES/GCM/NoPadding"
    private const val IV_LENGTH = 12
}
