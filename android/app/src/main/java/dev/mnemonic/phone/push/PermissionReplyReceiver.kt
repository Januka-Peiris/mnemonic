package dev.mnemonic.phone.push

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import dev.mnemonic.phone.AppState
import dev.mnemonic.phone.AppState.Companion.address
import dev.mnemonic.phone.api.MacAddress
import dev.mnemonic.phone.api.MacClient
import dev.mnemonic.phone.api.MacResult
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeoutOrNull
import okhttp3.OkHttpClient

class PermissionReplyReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val chatId = intent.getStringExtra(CHAT_ID).orEmpty()
        val toolCallId = intent.getStringExtra(TOOL_CALL_ID).orEmpty()
        val optionId = intent.getStringExtra(OPTION_ID).orEmpty()
        val title = intent.getStringExtra(TITLE).orEmpty()
        val allowed = intent.getBooleanExtra(ALLOWED, false)
        val appContext = context.applicationContext
        val pending = goAsync()
        CoroutineScope(SupervisorJob() + Dispatchers.IO).launch {
            try {
                val result = withTimeoutOrNull(TIMEOUT_MILLIS) {
                    val address = AppState(appContext).preferences.first().address()?.let { MacAddress.parse(it) }
                    if (address == null) {
                        MacResult.Unreachable("No Mac address is saved")
                    } else {
                        MacClient(address, OkHttpClient()).answerPermission(chatId, toolCallId, optionId)
                    }
                } ?: MacResult.Unreachable("The Mac didn't answer in time")
                Notifications.showReply(appContext, chatId, title, replyResultText(result, allowed))
            } finally {
                pending.finish()
            }
        }
    }

    companion object {
        private const val CHAT_ID = "chat_id"
        private const val TOOL_CALL_ID = "tool_call_id"
        private const val OPTION_ID = "option_id"
        private const val TITLE = "title"
        private const val ALLOWED = "allowed"
        private const val TIMEOUT_MILLIS = 10_000L

        fun intent(context: Context, chatId: String, title: String, permission: AlertPermission, allow: Boolean): Intent =
            Intent(context, PermissionReplyReceiver::class.java)
                .putExtra(CHAT_ID, chatId)
                .putExtra(TOOL_CALL_ID, permission.toolCallId)
                .putExtra(OPTION_ID, if (allow) permission.allowOptionId else permission.denyOptionId)
                .putExtra(TITLE, title)
                .putExtra(ALLOWED, allow)
    }
}
