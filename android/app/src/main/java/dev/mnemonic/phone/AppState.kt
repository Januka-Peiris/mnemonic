package dev.mnemonic.phone

import android.content.Context
import androidx.datastore.core.DataStore
import androidx.datastore.preferences.core.Preferences
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.longPreferencesKey
import androidx.datastore.preferences.core.stringPreferencesKey
import androidx.datastore.preferences.preferencesDataStore
import java.util.UUID
import kotlinx.coroutines.flow.Flow

private val Context.dataStore: DataStore<Preferences> by preferencesDataStore(name = "mnemonic_app_state")

class AppState(context: Context) {
    private val store = context.applicationContext.dataStore

    val preferences: Flow<Preferences> = store.data

    suspend fun saveAddress(address: String) {
        store.edit { it[ADDRESS] = address }
    }

    suspend fun markOpened(chatId: String, endedAt: Long) {
        store.edit { it[openedKey(chatId)] = maxOf(System.currentTimeMillis(), endedAt) }
    }

    suspend fun ensureInstalled() {
        store.edit {
            if (it[INSTALLED_AT] == null) it[INSTALLED_AT] = System.currentTimeMillis()
            if (it[INSTALL_ID] == null) it[INSTALL_ID] = UUID.randomUUID().toString()
        }
    }

    companion object {
        private val ADDRESS = stringPreferencesKey("address")
        private val INSTALLED_AT = longPreferencesKey("installed_at")
        private val INSTALL_ID = stringPreferencesKey("install_id")

        private fun openedKey(chatId: String) = longPreferencesKey("opened_$chatId")

        fun Preferences.address(): String? = this[ADDRESS]
        fun Preferences.installedAt(): Long = this[INSTALLED_AT] ?: 0L
        fun Preferences.installId(): String? = this[INSTALL_ID]
        fun Preferences.openedAt(chatId: String): Long = this[openedKey(chatId)] ?: 0L
    }
}
