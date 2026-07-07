package com.github.agent_rt.arc

import android.content.Intent
import android.os.Bundle
import android.provider.Settings
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp

/**
 * One screen that reflects the live bootstrap [ArcState] and re-drives it on tap.
 * On open it auto-runs the state machine (so after a reboot, opening the app
 * reconnects and starts the runner); the button re-runs it, and its label + the
 * status line track the phase in real time.
 */
class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent {
            MaterialTheme {
                Surface(modifier = Modifier.fillMaxSize()) {
                    BootstrapScreen()
                }
            }
        }
    }
}

@Composable
private fun BootstrapScreen() {
    val ctx = LocalContext.current
    val state = ArcState.status // Compose snapshot state → recomposes on change

    // Auto-run when the app opens (reboot → reconnect + start).
    LaunchedEffect(Unit) { PairingService.start(ctx) }

    Column(
        modifier = Modifier.fillMaxSize().padding(24.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        Text("arc runner bootstrap", style = MaterialTheme.typography.titleLarge)
        Text(state.message, style = MaterialTheme.typography.bodyLarge)

        Button(
            enabled = state.phase != ArcState.Phase.Working,
            onClick = { PairingService.start(ctx) },
        ) {
            Text(
                when (state.phase) {
                    ArcState.Phase.Working -> "Working…"
                    ArcState.Phase.RunnerUp -> "Restart runner"
                    else -> "Start"
                },
            )
        }

        if (state.phase == ArcState.Phase.NeedWireless) {
            Button(onClick = {
                ctx.startActivity(Intent(Settings.ACTION_APPLICATION_DEVELOPMENT_SETTINGS))
            }) { Text("Open Wireless debugging") }
        }

        Text(
            "First run pairs once — enter the code from the pairing dialog in the arc " +
                "notification. After that (and after a reboot) this reconnects and starts " +
                "the runner automatically.",
            style = MaterialTheme.typography.bodySmall,
        )
    }
}
