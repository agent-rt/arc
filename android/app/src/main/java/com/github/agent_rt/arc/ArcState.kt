package com.github.agent_rt.arc

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue

/**
 * Live bootstrap status, shared between [PairingService] (which drives the state
 * machine on a worker thread) and the Compose UI (which observes it). Backed by a
 * Compose snapshot state, so writes from any thread recompose the UI. The service
 * also mirrors these to the notification shade for when the app isn't open.
 */
object ArcState {
    enum class Phase {
        /** Nothing running yet. */
        Idle,

        /** A step is in progress (checking / pairing / connecting / pushing). */
        Working,

        /** Runner is up — nothing to do. */
        RunnerUp,

        /** Paired, but the connect port is unreachable — Wireless debugging is off. */
        NeedWireless,

        /** The last attempt failed. */
        Failed,
    }

    data class Status(val phase: Phase, val message: String)

    var status: Status by mutableStateOf(Status(Phase.Idle, "Starting…"))
        private set

    fun set(phase: Phase, message: String) {
        status = Status(phase, message)
    }
}
