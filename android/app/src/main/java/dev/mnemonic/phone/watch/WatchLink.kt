package dev.mnemonic.phone.watch

import android.content.Context
import android.util.Log
import com.garmin.android.connectiq.ConnectIQ
import com.garmin.android.connectiq.IQApp
import com.garmin.android.connectiq.IQDevice
import com.garmin.android.connectiq.exception.InvalidStateException
import com.garmin.android.connectiq.exception.ServiceUnavailableException

/** Talks to the Mnemonic app on the watch through the Garmin Connect app. */
class WatchLink(
    private val context: Context,
    private val onRequest: (WatchRequest) -> Unit,
    private val onStatus: (String) -> Unit,
) {
    private val connectIQ = ConnectIQ.getInstance(context, ConnectIQ.IQConnectType.WIRELESS)
    private val app = IQApp(WATCH_APP_ID)
    private var devices: List<IQDevice> = emptyList()
    private var ready = false

    fun start() {
        connectIQ.initialize(context, false, object : ConnectIQ.ConnectIQListener {
            override fun onSdkReady() {
                ready = true
                linkDevices()
            }

            override fun onInitializeError(status: ConnectIQ.IQSdkErrorStatus) {
                onStatus(
                    when (status) {
                        ConnectIQ.IQSdkErrorStatus.GCM_NOT_INSTALLED -> "Install Garmin Connect on this phone"
                        ConnectIQ.IQSdkErrorStatus.GCM_UPGRADE_NEEDED -> "Update Garmin Connect on this phone"
                        else -> "Garmin Connect couldn't be reached"
                    },
                )
            }

            override fun onSdkShutDown() {
                ready = false
            }
        })
    }

    fun stop() {
        if (!ready) return
        ready = false
        try {
            connectIQ.unregisterAllForEvents()
            connectIQ.shutdown(context)
        } catch (error: InvalidStateException) {
            Log.w(TAG, "Garmin Connect was already shut down: ${error.message}")
        }
    }

    fun send(message: Map<String, Any>) {
        if (!ready) return
        for (device in devices) {
            try {
                if (connectIQ.getDeviceStatus(device) != IQDevice.IQDeviceStatus.CONNECTED) continue
                connectIQ.sendMessage(device, app, message) { _, _, status ->
                    if (status != ConnectIQ.IQMessageStatus.SUCCESS) Log.w(TAG, "The watch didn't take a message: $status")
                }
            } catch (error: InvalidStateException) {
                Log.w(TAG, "Couldn't message the watch: ${error.message}")
            } catch (error: ServiceUnavailableException) {
                onStatus("Garmin Connect isn't running")
            }
        }
    }

    private fun linkDevices() {
        try {
            devices = connectIQ.knownDevices.orEmpty()
            if (devices.isEmpty()) {
                onStatus("No watch is paired in Garmin Connect")
                return
            }
            for (device in devices) {
                connectIQ.registerForDeviceEvents(device) { changed, status ->
                    if (status == IQDevice.IQDeviceStatus.CONNECTED) onStatus("Linked to ${changed.friendlyName}")
                }
                connectIQ.registerForAppEvents(device, app) { _, _, messages, status ->
                    if (status == ConnectIQ.IQMessageStatus.SUCCESS) {
                        messages.mapNotNull { parseWatchRequest(it) }.forEach(onRequest)
                    }
                }
            }
            onStatus("Linked to " + devices.joinToString { it.friendlyName })
        } catch (error: InvalidStateException) {
            onStatus("Garmin Connect isn't ready: ${error.message}")
        } catch (error: ServiceUnavailableException) {
            onStatus("Garmin Connect isn't running")
        }
    }

    private companion object {
        const val TAG = "MnemonicWatch"
    }
}
