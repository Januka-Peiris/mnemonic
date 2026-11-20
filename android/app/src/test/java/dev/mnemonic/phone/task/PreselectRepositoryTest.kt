package dev.mnemonic.phone.task

import dev.mnemonic.phone.api.PhoneRepository
import org.junit.Test
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull

class PreselectRepositoryTest {
    private val repositories = listOf(PhoneRepository("a", "alpha"), PhoneRepository("b", "beta"))

    @Test
    fun usesTheLastRepositoryWhenStillPresent() {
        assertEquals("b", preselectRepository(repositories, "b")?.id)
    }

    @Test
    fun fallsBackToTheFirstWhenTheLastIsGone() {
        assertEquals("a", preselectRepository(repositories, "gone")?.id)
        assertEquals("a", preselectRepository(repositories, null)?.id)
    }

    @Test
    fun isNullForNoRepositories() {
        assertNull(preselectRepository(emptyList(), "a"))
    }
}
