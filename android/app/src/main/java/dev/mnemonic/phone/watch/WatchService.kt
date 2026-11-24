package dev.mnemonic.phone.watch

import android.Manifest
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.util.Log
import androidx.compose.ui.graphics.toArgb
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat
import dev.mnemonic.phone.AppState
import dev.mnemonic.phone.AppState.Companion.address
import dev.mnemonic.phone.AppState.Companion.installedAt
import dev.mnemonic.phone.AppState.Companion.openedAt
import dev.mnemonic.phone.MainActivity
import dev.mnemonic.phone.R
import dev.mnemonic.phone.api.MacAddress
import dev.mnemonic.phone.api.MacClient
import dev.mnemonic.phone.api.MacResult
import dev.mnemonic.phone.api.PhoneActionOutcome
import dev.mnemonic.phone.api.TeamWorkspace
import dev.mnemonic.phone.chat.ChatState
import dev.mnemonic.phone.ui.MnColors
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import okhttp3.OkHttpClient

/** Keeps the watch app fed while watch mode is on, and records when the watch asks. */
class WatchService : Service() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private val http = OkHttpClient()
    private lateinit var appState: AppState
    private lateinit var link: WatchLink
    private var workspaces: List<TeamWorkspace> = emptyList()
    private var lastList: Map<String, Any>? = null
    private var lastChat: Map<String, Any>? = null
    private var following: Job? = null
    private var recording: Recording? = null
    private var transcribing: Job? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        appState = AppState(this)
        mutableRunning.value = true
        startInForeground("Starting...")
        link = WatchLink(this, onRequest = { request -> scope.launch { handle(request) } }, onStatus = ::showStatus)
        link.start()
        scope.launch { poll() }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) stopSelf()
        // Android won't let a service restarted in the background use the microphone.
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        recording?.cancel()
        recording = null
        scope.cancel()
        link.stop()
        mutableRunning.value = false
        mutableStatus.value = ""
        super.onDestroy()
    }

    private suspend fun client(): MacClient? =
        appState.preferences.first().address()?.let { MacAddress.parse(it) }?.let { MacClient(it, http) }

    private suspend fun poll() {
        while (scope.isActive) {
            sendList(force = false)
            delay(POLL_MILLIS)
        }
    }

    private suspend fun sendList(force: Boolean) {
        val client = client()
        val message = if (client == null) {
            listMessage(emptyList(), "Connect the phone to your Mac first")
        } else {
            when (val result = client.workspaces()) {
                is MacResult.Ok -> {
                    workspaces = result.value.workspaces
                    val preferences = appState.preferences.first()
                    listMessage(watchRows(workspaces, { preferences.openedAt(it) }, preferences.installedAt()), null)
                }
                is MacResult.Refused -> listMessage(rows(), result.reason)
                is MacResult.Unreachable -> listMessage(rows(), "Can't reach the Mac")
            }
        }
        if (force || message != lastList) {
            lastList = message
            link.send(message)
        }
    }

    private suspend fun rows(): List<WatchRow> {
        val preferences = appState.preferences.first()
        return watchRows(workspaces, { preferences.openedAt(it) }, preferences.installedAt())
    }

    private suspend fun handle(request: WatchRequest) {
        val client = client()
        when (request) {
            WatchRequest.Hello -> {
                sendList(force = true)
                lastChat?.let { link.send(it) }
            }
            is WatchRequest.Open -> if (client != null) follow(client, request.chatId)
            WatchRequest.Close -> {
                following?.cancel()
                following = null
                lastChat = null
            }
            is WatchRequest.Answer -> reply(request.chatId, "Answered") {
                client?.answerPermission(request.chatId, request.toolCallId, request.optionId)
            }
            is WatchRequest.Send -> reply(request.chatId, "Sent") { client?.sendMessage(request.chatId, request.text) }
            is WatchRequest.Stop -> reply(request.chatId, "Stopped") { client?.stop(request.chatId) }
            is WatchRequest.Listen -> listen(request.chatId)
            is WatchRequest.FinishListening -> finishListening(request.chatId)
            WatchRequest.CancelListening -> {
                recording?.cancel()
                recording = null
            }
        }
    }

    private fun follow(client: MacClient, chatId: String) {
        following?.cancel()
        lastChat = null
        following = scope.launch {
            val found = workspaces.firstNotNullOfOrNull { workspace ->
                workspace.chats.firstOrNull { it.id == chatId }?.let { workspace to it }
            }
            val title = found?.let { (workspace, chat) -> workspace.name + ": " + chat.title } ?: "Chat"
            found?.second?.let { appState.markOpened(chatId, it.lastTurnEndedAt ?: 0L) }
            val state = ChatState()
            while (isActive) {
                val message = when (val result = client.chatUpdate(chatId, state.cursor, CHAT_WAIT_SECONDS)) {
                    is MacResult.Ok -> {
                        state.apply(result.value)
                        chatMessage(chatId, title, state)
                    }
                    is MacResult.Refused -> chatErrorMessage(chatId, title, result.reason)
                    is MacResult.Unreachable -> chatErrorMessage(chatId, title, "Can't reach the Mac")
                }
                if (message != lastChat) {
                    lastChat = message
                    link.send(message)
                }
                if (message["error"] != "") delay(RETRY_MILLIS)
            }
        }
    }

    private suspend fun reply(chatId: String, done: String, action: suspend () -> MacResult<PhoneActionOutcome>?) {
        val text = when (val result = action()) {
            null -> "Connect the phone to your Mac first"
            is MacResult.Ok -> result.value.message?.takeIf { it.isNotBlank() } ?: done
            is MacResult.Refused -> result.reason.ifBlank { "The Mac refused (${result.status})" }
            is MacResult.Unreachable -> "Can't reach the Mac"
        }
        link.send(doneMessage(chatId, text))
    }

    private fun listen(chatId: String) {
        recording?.cancel()
        recording = null
        if (!canRecord(this)) {
            link.send(heardMessage(chatId, "", "Turn on the microphone for Mnemonic on the phone"))
            return
        }
        recording = try {
            Recording.start(this) { scope.launch { finishListening(chatId) } }
        } catch (error: Exception) {
            Log.w(TAG, "Couldn't start recording", error)
            link.send(heardMessage(chatId, "", "The phone's microphone is busy"))
            return
        }
    }

    private fun finishListening(chatId: String) {
        val finished = recording ?: return
        recording = null
        val audio = finished.finish()
        if (audio == null) {
            link.send(heardMessage(chatId, "", "Nothing was recorded"))
            return
        }
        transcribing?.cancel()
        transcribing = scope.launch {
            val message = when (val result = client()?.transcribe(audio)) {
                null -> heardMessage(chatId, "", "Connect the phone to your Mac first")
                is MacResult.Ok -> heardMessage(chatId, result.value, "")
                is MacResult.Refused -> heardMessage(chatId, "", result.reason.ifBlank { "The Mac couldn't transcribe it" })
                is MacResult.Unreachable -> heardMessage(chatId, "", "Can't reach the Mac")
            }
            link.send(message)
        }
    }

    private fun showStatus(text: String) {
        mutableStatus.value = text
        getSystemService(NotificationManager::class.java).notify(NOTIFICATION_ID, notification(text))
    }

    private fun startInForeground(text: String) {
        val types = ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE or
            if (canRecord(this) && Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE else 0
        ServiceCompat.startForeground(this, NOTIFICATION_ID, notification(text), types)
    }

    private fun notification(text: String): Notification {
        val manager = getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(NotificationChannel(CHANNEL, "Watch", NotificationManager.IMPORTANCE_LOW))
        val open = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val stop = PendingIntent.getService(
            this,
            1,
            Intent(this, WatchService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        return NotificationCompat.Builder(this, CHANNEL)
            .setSmallIcon(R.drawable.ic_notification)
            .setColor(MnColors.accent.toArgb())
            .setContentTitle("Watch mode")
            .setContentText(text)
            .setOngoing(true)
            .setContentIntent(open)
            .addAction(0, "Turn off", stop)
            .build()
    }

    companion object {
        private const val TAG = "WatchService"
        private const val CHANNEL = "watch"
        private const val NOTIFICATION_ID = 7301
        private const val ACTION_STOP = "dev.mnemonic.phone.watch.STOP"
        private const val POLL_MILLIS = 5_000L
        private const val RETRY_MILLIS = 3_000L
        private const val CHAT_WAIT_SECONDS = 25

        private val mutableRunning = MutableStateFlow(false)
        private val mutableStatus = MutableStateFlow("")
        val running: StateFlow<Boolean> = mutableRunning
        val status: StateFlow<String> = mutableStatus

        fun canRecord(context: Context): Boolean =
            ContextCompat.checkSelfPermission(context, Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED

        /** Call while the app is on screen: Android only grants the microphone to a service started then. */
        fun start(context: Context) {
            ContextCompat.startForegroundService(context, Intent(context, WatchService::class.java))
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, WatchService::class.java))
        }
    }
}
