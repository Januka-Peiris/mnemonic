package dev.mnemonic.phone.push

import java.io.File
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder

class PushKeyStoreTest {
    @get:Rule
    val folder = TemporaryFolder()

    private val plain = object : KeySealer {
        override fun seal(plain: ByteArray) = plain
        override fun open(sealed: ByteArray) = sealed
    }

    @Test
    fun concurrentFirstRegistrationsAllGetTheSameKeys() {
        val file = File(folder.root, "push_keys.bin")
        val start = CountDownLatch(1)
        val pool = Executors.newFixedThreadPool(8)
        val results = (1..8).map {
            pool.submit<PushKeys> {
                start.await()
                PushKeyStore(file, plain).loadOrCreate()
            }
        }
        start.countDown()
        val keys = results.map { it.get() }
        pool.shutdown()
        keys.forEach { assertArrayEquals(keys.first().publicKey, it.publicKey) }
        assertEquals(1, keys.map { it.auth.toList() }.toSet().size)
    }

    @Test
    fun loadNeverCreatesKeysWhenTheyAreMissingOrUnreadable() {
        val file = File(folder.root, "push_keys.bin")
        val store = PushKeyStore(file, plain)
        assertNull(store.load())
        assertFalse(file.exists())

        file.writeBytes(byteArrayOf(1, 2, 3))
        assertNull(store.load())
        assertArrayEquals(byteArrayOf(1, 2, 3), file.readBytes())
    }
}
