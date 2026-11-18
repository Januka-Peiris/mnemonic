package dev.mnemonic.phone.ui

import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
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
import dev.mnemonic.phone.chat.ChatScreen
import dev.mnemonic.phone.chat.ChatViewModel
import dev.mnemonic.phone.connect.ConnectScreen
import dev.mnemonic.phone.diff.DiffScreen
import dev.mnemonic.phone.task.NewTaskScreen
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
            val chatId = entry.arguments?.getString("chatId").orEmpty()
            if (client != null) {
                val viewModel: ChatViewModel = viewModel(key = "$savedAddress $chatId") {
                    ChatViewModel(chatId, client, appState)
                }
                ChatScreen(
                    viewModel = viewModel,
                    onBack = { navigateBack(navController) },
                    onOpenChat = { navController.navigate(Routes.chat(it)) },
                    onDiff = { navController.navigate(Routes.diff(it)) },
                )
            }
        }
        composable(Routes.NEW_TASK) {
            if (client != null) {
                NewTaskScreen(
                    client = client,
                    onBack = { navController.popBackStack() },
                    onStarted = { chatId ->
                        navController.navigate(Routes.chat(chatId)) { popUpTo(Routes.NEW_TASK) { inclusive = true } }
                    },
                    onNoChat = { navController.popBackStack() },
                )
            }
        }
        composable(
            Routes.DIFF,
            arguments = listOf(navArgument("workspaceId") { type = NavType.StringType }),
        ) { entry ->
            if (client != null) {
                DiffScreen(
                    workspaceId = entry.arguments?.getString("workspaceId").orEmpty(),
                    client = client,
                    onBack = { navController.popBackStack() },
                )
            }
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

private fun navigateBack(navController: NavHostController) {
    if (!navController.popBackStack()) {
        navController.navigate(Routes.HOME) { popUpTo(0) }
    }
}
