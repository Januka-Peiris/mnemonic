package dev.mnemonic.phone.push

import android.util.Base64
import android.util.Log
import com.google.firebase.messaging.FirebaseMessagingService
import com.google.firebase.messaging.RemoteMessage
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch
import okhttp3.OkHttpClient

class MnemonicMessagingService : FirebaseMessagingService() {
    override fun onNewToken(token: String) {
        val appContext = applicationContext
        CoroutineScope(SupervisorJob() + Dispatchers.IO).launch {
            PushRegistration.register(appContext, OkHttpClient())
        }
    }

    override fun onMessageReceived(message: RemoteMessage) {
        val payload = try {
            val encoded = message.data["payload"] ?: error("The alert has no payload")
            val body = Base64.decode(encoded, Base64.URL_SAFE or Base64.NO_PADDING or Base64.NO_WRAP)
            val keys = PushKeyStore(this).load() ?: error("There are no readable push keys")
            AlertPayload.parse(PushCrypto.decrypt(body, keys))
        } catch (error: Exception) {
            Log.w(TAG, "Dropped an alert that couldn't be read: ${error.message}")
            return
        }
        Notifications.show(this, payload)
    }

    private companion object {
        const val TAG = "MnemonicPush"
    }
}
