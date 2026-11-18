package dev.mnemonic.phone.settings

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import dev.mnemonic.phone.BuildConfig

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun SettingsScreen(address: String, onChangeAddress: () -> Unit, onBack: () -> Unit) {
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
            Text("Alerts aren't set up yet", modifier = Modifier.padding(top = 4.dp))
            HorizontalDivider(modifier = Modifier.padding(vertical = 12.dp))
            Text("Version", style = MaterialTheme.typography.labelLarge)
            Text(BuildConfig.VERSION_NAME, modifier = Modifier.padding(top = 4.dp))
        }
    }
}
