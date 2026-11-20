package dev.mnemonic.phone.voice

import android.app.Activity
import android.content.Intent
import android.speech.RecognizerIntent
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.material3.TextButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.platform.LocalContext

fun appendSpoken(existing: String, spoken: String): String =
    if (existing.isBlank()) spoken else existing.trimEnd() + " " + spoken

@Composable
fun MicButton(onSpoken: (String) -> Unit, enabled: Boolean = true) {
    val context = LocalContext.current
    val intent = remember {
        Intent(RecognizerIntent.ACTION_RECOGNIZE_SPEECH)
            .putExtra(RecognizerIntent.EXTRA_LANGUAGE_MODEL, RecognizerIntent.LANGUAGE_MODEL_FREE_FORM)
    }
    val available = remember { context.packageManager.queryIntentActivities(intent, 0).isNotEmpty() }
    val launcher = rememberLauncherForActivityResult(ActivityResultContracts.StartActivityForResult()) { result ->
        if (result.resultCode == Activity.RESULT_OK) {
            val best = result.data?.getStringArrayListExtra(RecognizerIntent.EXTRA_RESULTS)?.firstOrNull()
            if (!best.isNullOrBlank()) onSpoken(best)
        }
    }
    if (!available) return
    TextButton(onClick = { launcher.launch(intent) }, enabled = enabled) { Text("Mic") }
}

@Composable
fun rememberDictation(onSpoken: (String) -> Unit): (() -> Unit)? {
    val context = LocalContext.current
    val intent = remember {
        Intent(RecognizerIntent.ACTION_RECOGNIZE_SPEECH)
            .putExtra(RecognizerIntent.EXTRA_LANGUAGE_MODEL, RecognizerIntent.LANGUAGE_MODEL_FREE_FORM)
    }
    val available = remember { context.packageManager.queryIntentActivities(intent, 0).isNotEmpty() }
    val launcher = rememberLauncherForActivityResult(ActivityResultContracts.StartActivityForResult()) { result ->
        if (result.resultCode == Activity.RESULT_OK) {
            val best = result.data?.getStringArrayListExtra(RecognizerIntent.EXTRA_RESULTS)?.firstOrNull()
            if (!best.isNullOrBlank()) onSpoken(best)
        }
    }
    return if (available) ({ launcher.launch(intent) }) else null
}
