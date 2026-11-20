package dev.mnemonic.phone.settings

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.systemBarsPadding
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.mnemonic.phone.BuildConfig
import dev.mnemonic.phone.api.MacClient
import dev.mnemonic.phone.api.MacResult
import dev.mnemonic.phone.push.AlertStatus
import dev.mnemonic.phone.push.PushRegistration
import dev.mnemonic.phone.push.message
import dev.mnemonic.phone.push.rememberTurnOnAlerts
import dev.mnemonic.phone.ui.MnColors
import dev.mnemonic.phone.ui.PrimaryButton
import dev.mnemonic.phone.ui.SecondaryButton
import dev.mnemonic.phone.ui.SectionLabel
import dev.mnemonic.phone.ui.TopBar
import kotlinx.coroutines.launch
import okhttp3.OkHttpClient

@Composable
fun SettingsScreen(
    address: String,
    client: MacClient?,
    http: OkHttpClient,
    onChangeAddress: () -> Unit,
    onBack: () -> Unit,
) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val alertStatus by PushRegistration.status.collectAsState()
    val turnOnAlerts = rememberTurnOnAlerts(context, http)
    var testResult by remember { mutableStateOf<String?>(null) }
    var testing by remember { mutableStateOf(false) }
    Column(Modifier.fillMaxSize().systemBarsPadding()) {
        TopBar(title = "Settings", subtitle = null, onBack = onBack)
        SectionLabel("Mac")
        Row(
            Modifier.fillMaxWidth().padding(horizontal = 14.dp, vertical = 6.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Text(address, color = MnColors.text, fontSize = 13.sp, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f))
            SecondaryButton("Change", onClick = onChangeAddress)
        }
        Divider()
        SectionLabel("Alerts")
        Text(
            alertStatus.message(),
            color = MnColors.textSoft,
            fontSize = 13.sp,
            lineHeight = 19.sp,
            modifier = Modifier.fillMaxWidth().padding(horizontal = 14.dp, vertical = 4.dp),
        )
        if (BuildConfig.HAS_FIREBASE) {
            Row(
                Modifier.fillMaxWidth().padding(horizontal = 14.dp, vertical = 6.dp),
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                if (alertStatus != AlertStatus.On) {
                    PrimaryButton("Turn on alerts", onClick = turnOnAlerts)
                }
                SecondaryButton(
                    "Send test alert",
                    enabled = client != null && !testing,
                    onClick = {
                        testing = true
                        scope.launch {
                            testResult = when (val result = client?.sendTestAlert()) {
                                is MacResult.Ok -> "Test alert sent"
                                is MacResult.Refused -> result.reason
                                is MacResult.Unreachable -> result.cause
                                null -> "Connect to your Mac first"
                            }
                            testing = false
                        }
                    },
                )
            }
            testResult?.let {
                Text(it, color = MnColors.textSecondary, fontSize = 11.sp, lineHeight = 15.sp, modifier = Modifier.padding(horizontal = 14.dp, vertical = 2.dp))
            }
        }
        Divider()
        SectionLabel("Version")
        Text(
            BuildConfig.VERSION_NAME,
            color = MnColors.textSoft,
            fontSize = 13.sp,
            modifier = Modifier.fillMaxWidth().padding(horizontal = 14.dp, vertical = 4.dp),
        )
    }
}

@Composable
private fun Divider() {
    Box(Modifier.padding(top = 8.dp).fillMaxWidth().height(1.dp).background(MnColors.divider))
}
