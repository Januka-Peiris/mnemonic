package dev.mnemonic.phone.settings

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
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
import dev.mnemonic.phone.BuildConfig
import dev.mnemonic.phone.api.MacClient
import dev.mnemonic.phone.api.MacResult
import dev.mnemonic.phone.push.AlertStatus
import dev.mnemonic.phone.push.PushRegistration
import dev.mnemonic.phone.push.message
import dev.mnemonic.phone.push.rememberTurnOnAlerts
import kotlinx.coroutines.launch
import okhttp3.OkHttpClient

@OptIn(ExperimentalMaterial3Api::class)
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
    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("Settings") },
                navigationIcon = { TextButton(onClick = onBack) { Text("Back") } },
            )
        },
    ) { padding ->
        Column(modifier = Modifier.fillMaxSize().padding(padding).padding(16.dp)) {
            Text("Mac", style = MaterialTheme.typography.labelLarge)
            Row(modifier = Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                Text(
                    address,
                    modifier = Modifier.weight(1f),
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                TextButton(onClick = onChangeAddress) { Text("Change") }
            }
            HorizontalDivider(modifier = Modifier.padding(vertical = 12.dp))
            Text("Alerts", style = MaterialTheme.typography.labelLarge)
            Text(alertStatus.message(), modifier = Modifier.padding(top = 4.dp))
            if (BuildConfig.HAS_FIREBASE) {
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp), modifier = Modifier.padding(top = 8.dp)) {
                    if (alertStatus != AlertStatus.On) {
                        Button(onClick = turnOnAlerts) { Text("Turn on alerts") }
                    }
                    OutlinedButton(
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
                    ) { Text("Send test alert") }
                }
                testResult?.let { Text(it, modifier = Modifier.padding(top = 8.dp)) }
            }
            HorizontalDivider(modifier = Modifier.padding(vertical = 12.dp))
            Text("Version", style = MaterialTheme.typography.labelLarge)
            Text(BuildConfig.VERSION_NAME, modifier = Modifier.padding(top = 4.dp))
        }
    }
}
