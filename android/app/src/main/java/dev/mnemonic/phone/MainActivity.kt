package dev.mnemonic.phone

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.runtime.getValue
import androidx.compose.runtime.produceState
import androidx.lifecycle.lifecycleScope
import dev.mnemonic.phone.AppState.Companion.address
import dev.mnemonic.phone.ui.MnemonicNavigation
import dev.mnemonic.phone.ui.MnemonicTheme
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.launch
import okhttp3.OkHttpClient

class MainActivity : ComponentActivity() {
    private val http = OkHttpClient()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        val appState = AppState(this)
        lifecycleScope.launch { appState.ensureInstalled() }
        setContent {
            MnemonicTheme {
                val loaded by produceState<Loaded>(Loaded.Pending) {
                    appState.preferences.map { Loaded.Ready(it.address()) }.collect { value = it }
                }
                when (val current = loaded) {
                    Loaded.Pending -> Unit
                    is Loaded.Ready -> MnemonicNavigation(current.address, appState, http)
                }
            }
        }
    }

    private sealed interface Loaded {
        data object Pending : Loaded
        data class Ready(val address: String?) : Loaded
    }
}
