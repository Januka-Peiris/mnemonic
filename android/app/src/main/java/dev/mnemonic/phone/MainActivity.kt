package dev.mnemonic.phone

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.produceState
import android.content.Intent
import androidx.compose.runtime.getValue
import androidx.lifecycle.lifecycleScope
import androidx.navigation.NavHostController
import androidx.navigation.compose.rememberNavController
import dev.mnemonic.phone.push.PushRegistration
import kotlinx.coroutines.flow.distinctUntilChanged
import dev.mnemonic.phone.AppState.Companion.address
import dev.mnemonic.phone.ui.MnemonicNavigation
import dev.mnemonic.phone.ui.MnemonicTheme
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.launch
import okhttp3.OkHttpClient

class MainActivity : ComponentActivity() {
    private val http = OkHttpClient()
    private var navController: NavHostController? = null

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        navController?.handleDeepLink(intent)
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        val appState = AppState(this)
        lifecycleScope.launch { appState.ensureInstalled() }
        lifecycleScope.launch {
            appState.preferences.map { it.address() }.distinctUntilChanged().collect { address ->
                if (address != null) PushRegistration.register(this@MainActivity, http)
            }
        }
        setContent {
            val controller = rememberNavController()
            DisposableEffect(controller) {
                navController = controller
                onDispose { if (navController === controller) navController = null }
            }
            MnemonicTheme {
                val loaded by produceState<Loaded>(Loaded.Pending) {
                    appState.preferences.map { Loaded.Ready(it.address()) }.collect { value = it }
                }
                when (val current = loaded) {
                    Loaded.Pending -> Unit
                    is Loaded.Ready -> MnemonicNavigation(current.address, appState, http, controller)
                }
            }
        }
    }

    private sealed interface Loaded {
        data object Pending : Loaded
        data class Ready(val address: String?) : Loaded
    }
}
