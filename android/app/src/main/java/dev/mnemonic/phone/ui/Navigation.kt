package dev.mnemonic.phone.ui

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.lifecycle.viewmodel.compose.viewModel
import androidx.navigation.NavHostController
import androidx.navigation.NavType
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.navigation.compose.rememberNavController
import androidx.navigation.navArgument
import androidx.navigation.navDeepLink
import dev.mnemonic.phone.AppState
import dev.mnemonic.phone.api.MacAddress
import dev.mnemonic.phone.api.MacClient
import dev.mnemonic.phone.connect.ConnectScreen
import dev.mnemonic.phone.home.HomeScreen
import dev.mnemonic.phone.home.HomeViewModel
import dev.mnemonic.phone.settings.SettingsScreen
import kotlinx.coroutines.launch
import okhttp3.OkHttpClient

object Routes {
    const val CONNECT = "connect"
    const val HOME = "home"
    const val CHAT = "chat/{chatId}"
    const val NEW_TASK = "newTask"
    const val DIFF = "diff/{workspaceId}"
    const val SETTINGS = "settings"

    fun chat(chatId: String) = "chat/$chatId"
    fun diff(workspaceId: String) = "diff/$workspaceId"
}

@Composable
fun MnemonicNavigation(
    savedAddress: String?,
    appState: AppState,
    http: OkHttpClient,
    navController: NavHostController = rememberNavController(),
) {
    val scope = rememberCoroutineScope()
    val macAddress = savedAddress?.let { MacAddress.parse(it) }
    val client = remember(macAddress) { macAddress?.let { MacClient(it, http) } }

    NavHost(
        navController = navController,
        startDestination = if (client == null) Routes.CONNECT else Routes.HOME,
    ) {
        composable(Routes.CONNECT) {
            ConnectScreen(http) { address ->
                scope.launch {
                    appState.saveAddress(address)
                    navController.navigate(Routes.HOME) {
                        popUpTo(Routes.CONNECT) { inclusive = true }
                    }
                }
            }
        }
        composable(Routes.HOME) {
            if (client != null) {
                val viewModel: HomeViewModel = viewModel(key = savedAddress) { HomeViewModel(client, appState) }
                HomeScreen(
                    viewModel = viewModel,
                    onOpenChat = { navController.navigate(Routes.chat(it)) },
                    onNewTask = { navController.navigate(Routes.NEW_TASK) },
                    onSettings = { navController.navigate(Routes.SETTINGS) },
                )
            }
        }
        composable(
            Routes.CHAT,
            arguments = listOf(navArgument("chatId") { type = NavType.StringType }),
            deepLinks = listOf(navDeepLink { uriPattern = "mnemonic://chat/{chatId}" }),
        ) { entry ->
            Placeholder("Chat " + entry.arguments?.getString("chatId").orEmpty())
        }
        composable(Routes.NEW_TASK) { Placeholder("New task") }
        composable(
            Routes.DIFF,
            arguments = listOf(navArgument("workspaceId") { type = NavType.StringType }),
        ) { entry ->
            Placeholder("Diff " + entry.arguments?.getString("workspaceId").orEmpty())
        }
        composable(Routes.SETTINGS) {
            SettingsScreen(
                address = savedAddress.orEmpty(),
                onChangeAddress = {
                    navController.navigate(Routes.CONNECT) {
                        popUpTo(Routes.HOME) { inclusive = true }
                    }
                },
                onBack = { navController.popBackStack() },
            )
        }
    }
}

@Composable
private fun Placeholder(text: String) {
    Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) { Text(text) }
}
