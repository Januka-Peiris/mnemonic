package dev.mnemonic.phone.push

import android.Manifest
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import androidx.core.app.NotificationCompat
import androidx.compose.ui.graphics.toArgb
import androidx.core.content.ContextCompat
import dev.mnemonic.phone.MainActivity
import dev.mnemonic.phone.R
import dev.mnemonic.phone.ui.MnColors
import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json

@Serializable
data class AlertPermission(
    @SerialName("tool_call_id") val toolCallId: String,
    @SerialName("allow_option_id") val allowOptionId: String,
    @SerialName("deny_option_id") val denyOptionId: String,
)

@Serializable
data class AlertPayload(
    val title: String,
    val body: String,
    @SerialName("chat_id") val chatId: String = "",
    val url: String = "",
    val permission: AlertPermission? = null,
) {
    companion object {
        private val json = Json { ignoreUnknownKeys = true }

        fun parse(plaintext: ByteArray): AlertPayload = json.decodeFromString(serializer(), plaintext.decodeToString())
    }
}

object Notifications {
    const val PERMISSION_CHANNEL = "permission"
    const val FINISHED_CHANNEL = "finished"

    fun notificationId(chatId: String): Int = chatId.hashCode()

    fun canPost(context: Context): Boolean =
        Build.VERSION.SDK_INT < 33 ||
            ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED

    fun ensureChannels(context: Context) {
        val manager = context.getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(PERMISSION_CHANNEL, "Needs permission", NotificationManager.IMPORTANCE_HIGH),
        )
        manager.createNotificationChannel(
            NotificationChannel(FINISHED_CHANNEL, "Finished", NotificationManager.IMPORTANCE_DEFAULT),
        )
    }

    fun show(context: Context, payload: AlertPayload) {
        val permission = payload.permission
        val actions = permission != null && permission.allowOptionId.isNotEmpty() && permission.denyOptionId.isNotEmpty()
        post(
            context = context,
            chatId = payload.chatId,
            title = payload.title,
            text = payload.body,
            channel = if (permission != null) PERMISSION_CHANNEL else FINISHED_CHANNEL,
            permission = permission.takeIf { actions },
        )
    }

    fun showReply(context: Context, chatId: String, title: String, text: String) {
        post(context, chatId, title, text, PERMISSION_CHANNEL, permission = null)
    }

    private fun post(
        context: Context,
        chatId: String,
        title: String,
        text: String,
        channel: String,
        permission: AlertPermission?,
    ) {
        if (!canPost(context)) return
        ensureChannels(context)
        val id = notificationId(chatId)
        val builder = NotificationCompat.Builder(context, channel)
            .setSmallIcon(R.drawable.ic_notification)
            .setColor(MnColors.accent.toArgb())
            .setContentTitle(title)
            .setContentText(text)
            .setAutoCancel(true)
            .setOnlyAlertOnce(true)
            .setContentIntent(openChat(context, chatId, id))
        if (permission != null) {
            builder.addAction(0, "Allow once", reply(context, chatId, title, permission, allow = true))
            builder.addAction(0, "Deny", reply(context, chatId, title, permission, allow = false))
        }
        context.getSystemService(NotificationManager::class.java).notify(id, builder.build())
    }

    private fun openChat(context: Context, chatId: String, id: Int): PendingIntent {
        val intent = if (chatId.isEmpty()) {
            Intent(context, MainActivity::class.java)
        } else {
            Intent(Intent.ACTION_VIEW, Uri.parse("mnemonic://chat/" + Uri.encode(chatId)), context, MainActivity::class.java)
        }
        return PendingIntent.getActivity(context, id, intent, PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
    }

    private fun reply(
        context: Context,
        chatId: String,
        title: String,
        permission: AlertPermission,
        allow: Boolean,
    ): PendingIntent {
        val intent = PermissionReplyReceiver.intent(context, chatId, title, permission, allow)
        val requestCode = 31 * notificationId(chatId) + if (allow) 1 else 2
        return PendingIntent.getBroadcast(context, requestCode, intent, PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
    }
}
