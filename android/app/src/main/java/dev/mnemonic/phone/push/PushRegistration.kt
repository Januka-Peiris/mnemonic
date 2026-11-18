package dev.mnemonic.phone.push

import android.Manifest
import android.content.Context
import android.os.Build
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.runtime.Composable
import androidx.compose.runtime.rememberCoroutineScope
import com.google.firebase.messaging.FirebaseMessaging
import dev.mnemonic.phone.AppState
import dev.mnemonic.phone.AppState.Companion.address
import dev.mnemonic.phone.AppState.Companion.installId
import dev.mnemonic.phone.BuildConfig
import dev.mnemonic.phone.api.MacAddress
import dev.mnemonic.phone.api.MacClient
import dev.mnemonic.phone.api.MacResult
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.suspendCancellableCoroutine
import okhttp3.OkHttpClient

sealed interface AlertStatus {
    data object NeedsFirebase : AlertStatus
    data object NeedsPermission : AlertStatus
    data object Working : AlertStatus
    data object On : AlertStatus
    data class Failed(val reason: String) : AlertStatus
}

fun AlertStatus.message(): String =
    when (this) {
        AlertStatus.NeedsFirebase -> "Alerts need the Firebase setup (google-services.json)"
        AlertStatus.NeedsPermission -> "Alerts need notification permission"
        AlertStatus.Working -> "Turning on alerts..."
        AlertStatus.On -> "Alerts are on"
        is AlertStatus.Failed -> reason
    }

object PushRegistration {
    private val mutableStatus = MutableStateFlow<AlertStatus>(
        if (BuildConfig.HAS_FIREBASE) AlertStatus.Working else AlertStatus.NeedsFirebase,
    )
    val status: StateFlow<AlertStatus> = mutableStatus

    suspend fun register(context: Context, http: OkHttpClient) = withContext(Dispatchers.IO) {
        val appContext = context.applicationContext
        if (!BuildConfig.HAS_FIREBASE) {
            mutableStatus.value = AlertStatus.NeedsFirebase
            return@withContext
        }
        if (!Notifications.canPost(appContext)) {
            mutableStatus.value = AlertStatus.NeedsPermission
            return@withContext
        }
        mutableStatus.value = AlertStatus.Working
        mutableStatus.value = try {
            val preferences = AppState(appContext).preferences.first { it.installId() != null }
            val address = preferences.address()?.let { MacAddress.parse(it) }
            if (address == null) {
                AlertStatus.Failed("Connect to your Mac first")
            } else {
                val keys = PushKeyStore(appContext).loadOrCreate()
                val result = MacClient(address, http).registerDevice(
                    installId = preferences.installId().orEmpty(),
                    token = fetchToken(),
                    p256dh = keys.p256dhBase64Url(),
                    auth = keys.authBase64Url(),
                    name = "${Build.MANUFACTURER} ${Build.MODEL}",
                )
                when (result) {
                    is MacResult.Ok -> AlertStatus.On
                    is MacResult.Refused -> AlertStatus.Failed(result.reason)
                    is MacResult.Unreachable -> AlertStatus.Failed(result.cause)
                }
            }
        } catch (error: CancellationException) {
            throw error
        } catch (error: Exception) {
            AlertStatus.Failed(error.message ?: "Couldn't register this phone for alerts")
        }
    }

    private suspend fun fetchToken(): String = suspendCancellableCoroutine { continuation ->
        FirebaseMessaging.getInstance().token
            .addOnSuccessListener { continuation.resume(it) }
            .addOnFailureListener { continuation.resumeWithException(it) }
    }
}

@Composable
fun rememberTurnOnAlerts(context: Context, http: OkHttpClient): () -> Unit {
    val scope = rememberCoroutineScope()
    val launcher = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) {
        scope.launch { PushRegistration.register(context, http) }
    }
    return {
        if (Build.VERSION.SDK_INT >= 33 && !Notifications.canPost(context)) {
            launcher.launch(Manifest.permission.POST_NOTIFICATIONS)
        } else {
            scope.launch { PushRegistration.register(context, http) }
        }
    }
}
