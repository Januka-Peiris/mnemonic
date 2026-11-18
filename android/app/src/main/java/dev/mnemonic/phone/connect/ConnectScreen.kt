package dev.mnemonic.phone.connect

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.systemBarsPadding
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.compose.ui.text.input.KeyboardType
import dev.mnemonic.phone.api.MacAddress
import dev.mnemonic.phone.api.MacClient
import dev.mnemonic.phone.api.MacResult
import kotlinx.coroutines.launch
import okhttp3.OkHttpClient

@Composable
fun ConnectScreen(http: OkHttpClient, onConnected: (String) -> Unit) {
    var text by remember { mutableStateOf("https://") }
    var error by remember { mutableStateOf<String?>(null) }
    var busy by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()

    Column(
        modifier = Modifier.fillMaxSize().systemBarsPadding().imePadding().padding(24.dp),
        verticalArrangement = Arrangement.Center,
    ) {
        Text("Connect to your Mac", style = MaterialTheme.typography.headlineSmall)
        Text(
            "Enter the address of the Mac running Mnemonic.",
            style = MaterialTheme.typography.bodyMedium,
            modifier = Modifier.padding(top = 8.dp, bottom = 16.dp),
        )
        OutlinedTextField(
            value = text,
            onValueChange = { text = it },
            modifier = Modifier.fillMaxWidth(),
            singleLine = true,
            placeholder = { Text("your-mac.tailnet.ts.net:47232") },
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
            isError = error != null,
            supportingText = error?.let { message -> { Text(message) } },
        )
        Button(
            onClick = {
                val address = MacAddress.parse(text)
                if (address == null) {
                    error = "That isn't a valid address."
                    return@Button
                }
                busy = true
                error = null
                scope.launch {
                    when (val result = MacClient(address, http).workspaces()) {
                        is MacResult.Ok -> onConnected(address.baseUrl)
                        is MacResult.Refused -> error = result.reason
                        is MacResult.Unreachable -> error =
                            if (result.addressUnusable) {
                                "Use the full address shown in the Mac's Team window, like https://your-mac.tailnet.ts.net:47232"
                            } else {
                                "Can't reach the Mac. Is Tailscale on?"
                            }
                    }
                    busy = false
                }
            },
            enabled = !busy,
            modifier = Modifier.padding(top = 16.dp),
        ) {
            Text(if (busy) "Connecting..." else "Connect")
        }
    }
}
