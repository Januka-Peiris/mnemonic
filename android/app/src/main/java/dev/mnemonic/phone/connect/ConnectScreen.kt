package dev.mnemonic.phone.connect

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.systemBarsPadding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.mnemonic.phone.api.MacAddress
import dev.mnemonic.phone.api.MacClient
import dev.mnemonic.phone.api.MacResult
import dev.mnemonic.phone.ui.MnColors
import dev.mnemonic.phone.ui.PrimaryButton
import kotlinx.coroutines.launch
import okhttp3.OkHttpClient

@Composable
fun ConnectScreen(http: OkHttpClient, onConnected: (String) -> Unit) {
    var text by remember { mutableStateOf("https://") }
    var error by remember { mutableStateOf<String?>(null) }
    var busy by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()

    fun connect() {
        val address = MacAddress.parse(text)
        if (address == null) {
            error = "That isn't a valid address."
            return
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
    }

    Column(
        modifier = Modifier.fillMaxSize().systemBarsPadding().imePadding().padding(horizontal = 24.dp),
        verticalArrangement = Arrangement.Center,
    ) {
        Text("Mnemonic", color = MnColors.text, fontSize = 17.sp, lineHeight = 22.sp, fontWeight = FontWeight.SemiBold)
        Text(
            "Enter the address of the Mac running Mnemonic.",
            color = MnColors.textSecondary,
            fontSize = 13.sp,
            lineHeight = 19.sp,
            modifier = Modifier.padding(top = 4.dp, bottom = 16.dp),
        )
        OutlinedTextField(
            value = text,
            onValueChange = { text = it },
            modifier = Modifier.fillMaxWidth(),
            singleLine = true,
            textStyle = TextStyle(color = MnColors.text, fontSize = 13.sp),
            shape = RoundedCornerShape(7.dp),
            placeholder = { Text("your-mac.tailnet.ts.net:47232", color = MnColors.textMuted, fontSize = 13.sp) },
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
            isError = error != null,
            colors = OutlinedTextFieldDefaults.colors(
                focusedBorderColor = MnColors.accent,
                unfocusedBorderColor = MnColors.border,
                errorBorderColor = MnColors.error,
                cursorColor = MnColors.accentText,
            ),
        )
        error?.let { message ->
            Text(message, color = MnColors.error, fontSize = 11.sp, lineHeight = 15.sp, modifier = Modifier.padding(top = 6.dp))
        }
        PrimaryButton(
            text = if (busy) "Connecting..." else "Connect",
            onClick = ::connect,
            enabled = !busy,
            modifier = Modifier.fillMaxWidth().padding(top = 14.dp),
        )
    }
}
