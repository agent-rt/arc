package com.github.agent_rt.arc

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.RemoteInput
import android.app.Service
import android.content.Context
import android.content.Intent
import android.os.IBinder
import android.provider.Settings
import android.util.Log
import java.net.InetSocketAddress
import java.net.Socket

/**
 * The no-PC bootstrap orchestrator (path 2), modelled on Shizuku's pairing
 * service. One `START` runs a state machine and reports every step to the
 * notification shade:
 *
 * 1. Runner already up (a TCP probe of its own port)?  → done, nothing to do.
 * 2. Not paired (no stored key)?  → mDNS the pairing port, collect the 6-digit
 *    code via a notification RemoteInput, pair once. Pairing is one-time.
 * 3. Paired → connect: find the connect port (cached, else a localhost probe in
 *    the native engine — mDNS-free), push the runner and start it supervised. If
 *    the port can't be found the device's Wireless debugging is off — guide the
 *    user to enable it (no re-pairing needed).
 *
 * Overlays are avoided on purpose: the pairing Settings screen force-hides
 * non-system overlays (verified via dumpsys); the notification shade doesn't, and
 * doesn't background the dialog either, so the pairing port stays open.
 */
class PairingService : Service() {
    private var pairMdns: AdbMdns? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_STOP -> {
                stopEverything(); stopForeground(STOP_FOREGROUND_REMOVE); stopSelf()
                return START_NOT_STICKY
            }
            ACTION_REPLY -> {
                ArcState.set(ArcState.Phase.Working, getString(R.string.pair_working))
                startForeground(NOTIF_ID, notif(getString(R.string.pair_working)))
                val code = RemoteInput.getResultsFromIntent(intent)
                    ?.getCharSequence(KEY_CODE)?.toString()?.trim().orEmpty()
                val port = intent.getIntExtra(KEY_PORT, -1)
                Thread { onPairReply(code, port) }.start()
            }
            else -> {
                ArcState.set(ArcState.Phase.Working, getString(R.string.state_checking))
                startForeground(NOTIF_ID, notif(getString(R.string.state_checking)))
                Thread { orchestrate() }.start()
            }
        }
        return START_REDELIVER_INTENT
    }

    /** The state machine (runs on a worker thread). */
    private fun orchestrate() {
        if (tcpAlive(RUNNER_PORT.toInt())) {
            finish(true, getString(R.string.runner_already_running, RUNNER_PORT))
            return
        }
        if (adbKeyIfPaired() == null) {
            // Not paired yet — wait for the pairing dialog, prompt for the code.
            startPairingSearch()
        } else {
            connectAndStart()
        }
    }

    // --- pairing (one-time) ---

    private fun startPairingSearch() {
        pairMdns?.stop()
        ArcState.set(ArcState.Phase.Working, getString(R.string.pair_open_dialog))
        pairMdns = AdbMdns(this, AdbMdns.TLS_PAIRING) { port ->
            if (port > 0) {
                Log.i(TAG, "pairing port found: $port")
                ArcState.set(ArcState.Phase.Working, getString(R.string.pair_enter_in_notif))
                nm().notify(NOTIF_ID, inputNotification(port))
            }
        }.also { it.start() }
    }

    private fun onPairReply(code: String, port: Int) {
        if (port <= 0 || code.isEmpty()) { startPairingSearch(); return }
        try {
            AdbNative.pair("127.0.0.1:$port", code, adbKey(this), "arc@android")
            Log.i(TAG, "PAIR=ok port=$port")
        } catch (e: Throwable) {
            Log.e(TAG, "PAIR=err ${e.message}")
            finish(false, getString(R.string.pair_failed) + ": " + (e.message ?: ""))
            return
        }
        pairMdns?.stop()
        connectAndStart()
    }

    // --- connect + start (every time the runner isn't up) ---

    private fun connectAndStart() {
        ArcState.set(ArcState.Phase.Working, getString(R.string.state_connecting))
        nm().notify(NOTIF_ID, notif(getString(R.string.state_connecting)))
        val port = connectPort()
        if (port == null) {
            // Paired but unreachable — Wireless debugging is off (reboot resets it).
            ArcState.set(ArcState.Phase.NeedWireless, getString(R.string.enable_wireless_text))
            nm().notify(NOTIF_ID, enableWirelessNotification())
            stopForeground(STOP_FOREGROUND_DETACH); stopSelf()
            return
        }
        val (ok, msg) = try {
            val key = adbKey(this)
            val runner = assets.open(RUNNER_ASSET).use { it.readBytes() }
            val mtime = (System.currentTimeMillis() / 1000).toInt()
            AdbNative.pushFile("127.0.0.1:$port", key, runner, RUNNER_REMOTE, 0x81ED, mtime)
            // The trailing `sleep` keeps the launching shell alive long enough for
            // setsid to move the runner into a new session before adbd SIGHUPs the
            // shell's process group — without it the runner is killed before it
            // detaches (verified: no sleep → never binds; with sleep → binds).
            AdbNative.runShell(
                "127.0.0.1:$port", key,
                "chmod 755 $RUNNER_REMOTE; setsid $RUNNER_REMOTE --supervise " +
                    "0.0.0.0:$RUNNER_PORT $RUNNER_CODE </dev/null >/data/local/tmp/arc-runner.log 2>&1 & sleep 2",
            )
            // Confirm the server actually bound the port — the supervisor process
            // stays alive even if the child crash-loops (e.g. bad args), so pidof
            // isn't proof. Poll the runner's own port.
            var up = false
            for (i in 0 until 10) {
                if (tcpAlive(RUNNER_PORT.toInt())) { up = true; break }
                Thread.sleep(500)
            }
            if (up) {
                Log.i(TAG, "RUNNER=ok listening on $RUNNER_PORT")
                true to getString(R.string.runner_started, RUNNER_PORT)
            } else {
                Log.e(TAG, "RUNNER not listening on $RUNNER_PORT")
                false to getString(R.string.runner_no_bind, RUNNER_PORT)
            }
        } catch (e: Throwable) {
            Log.e(TAG, "RUNNER=err ${e.message}")
            false to (e.message ?: "error")
        }
        finish(ok, msg)
    }

    /** The connect port: try the cached one (fast TCP probe), else find it by
     *  probing localhost in the native engine (mDNS-free — mDNS discovery of the
     *  connect service is unreliable on noisy multicast, but the port is a stable
     *  localhost-reachable socket). The port is stable until reboot / re-toggle. */
    private fun connectPort(): Int? {
        val prefs = getSharedPreferences("arc", Context.MODE_PRIVATE)
        prefs.getInt(KEY_CONNECT_PORT, 0).takeIf { it > 0 && tcpAlive(it) }?.let { return it }
        // Fast path: mDNS — instant when the connect service is advertised, which
        // it is right after enabling Wireless debugging / pairing (the common
        // bootstrap moment). Fallback: a localhost scan in the engine — reliable
        // and immune to mDNS noise, but ~20-30s (≈28k connects bounded by cores),
        // so it's a last resort. Either way the result is cached, so a session
        // pays the slow path at most once.
        val found = discoverConnectMdns(6000) ?: AdbNative.findConnectPort().takeIf { it > 0 }
        if (found != null) prefs.edit().putInt(KEY_CONNECT_PORT, found).apply()
        return found
    }

    /** Short mDNS discovery of the connect port; null if not advertised in time. */
    private fun discoverConnectMdns(timeoutMs: Long): Int? {
        var found: Int? = null
        val latch = java.util.concurrent.CountDownLatch(1)
        val m = AdbMdns(this, AdbMdns.TLS_CONNECT) { p ->
            if (p > 0 && found == null) { found = p; latch.countDown() }
        }
        m.start()
        latch.await(timeoutMs, java.util.concurrent.TimeUnit.MILLISECONDS)
        m.stop()
        return found
    }

    // --- helpers ---

    private fun tcpAlive(port: Int): Boolean = try {
        Socket().use { it.connect(InetSocketAddress("127.0.0.1", port), 600); true }
    } catch (_: Exception) {
        false
    }

    /** The stored key, or null if never paired. */
    private fun adbKeyIfPaired(): String? =
        getSharedPreferences("arc", Context.MODE_PRIVATE).getString("adbkey", null)

    private fun stopEverything() {
        pairMdns?.stop(); pairMdns = null
    }

    private fun finish(ok: Boolean, msg: String) {
        stopEverything()
        ArcState.set(if (ok) ArcState.Phase.RunnerUp else ArcState.Phase.Failed, msg)
        nm().notify(
            NOTIF_ID,
            base(if (ok) getString(R.string.bootstrap_ok) else getString(R.string.bootstrap_failed))
                .setContentText(msg).build(),
        )
        stopForeground(STOP_FOREGROUND_DETACH)
        stopSelf()
    }

    override fun onDestroy() {
        stopEverything()
        super.onDestroy()
    }

    // --- notifications ---

    private fun nm() = getSystemService(NotificationManager::class.java)

    private fun base(title: String): Notification.Builder {
        nm().createNotificationChannel(
            NotificationChannel(CHANNEL, "arc pairing", NotificationManager.IMPORTANCE_HIGH)
                .apply { setSound(null, null); setShowBadge(false) },
        )
        return Notification.Builder(this, CHANNEL)
            .setSmallIcon(android.R.drawable.ic_menu_manage)
            .setContentTitle(title)
    }

    private fun notif(title: String) = base(title).build()

    /** Inline-reply notification for the 6-digit code; the port is bound in. */
    private fun inputNotification(port: Int): Notification {
        val remoteInput = RemoteInput.Builder(KEY_CODE)
            .setLabel(getString(R.string.pair_code_label)).build()
        val pi = PendingIntent.getForegroundService(
            this, 1,
            Intent(this, PairingService::class.java).setAction(ACTION_REPLY).putExtra(KEY_PORT, port),
            PendingIntent.FLAG_MUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val action = Notification.Action.Builder(null, getString(R.string.pair_enter_code), pi)
            .addRemoteInput(remoteInput).build()
        return base(getString(R.string.pair_service_found))
            .setContentText(getString(R.string.pair_enter_code))
            .addAction(action).build()
    }

    /** Guidance when paired but the connect port is unreachable (wireless off). */
    private fun enableWirelessNotification(): Notification {
        val pi = PendingIntent.getActivity(
            this, 2,
            Intent(Settings.ACTION_APPLICATION_DEVELOPMENT_SETTINGS),
            PendingIntent.FLAG_IMMUTABLE,
        )
        return base(getString(R.string.enable_wireless_title))
            .setContentText(getString(R.string.enable_wireless_text))
            .addAction(Notification.Action.Builder(null, getString(R.string.open_settings), pi).build())
            .build()
    }

    companion object {
        const val TAG = "arcpair"
        private const val CHANNEL = "arc_pairing"
        private const val NOTIF_ID = 1
        private const val KEY_CODE = "code"
        private const val KEY_PORT = "port"
        private const val KEY_CONNECT_PORT = "connect_port"
        const val ACTION_STOP = "stop"
        const val ACTION_REPLY = "reply"
        private const val RUNNER_ASSET = "arc-runner-android"
        private const val RUNNER_REMOTE = "/data/local/tmp/arc-runner-android"
        private const val RUNNER_PORT = "8787"
        // Must be a valid arc pairing code (Crockford base32, no I/L/O/U).
        private const val RUNNER_CODE = "ARC0-ARC0"

        fun start(ctx: Context) {
            ctx.startForegroundService(Intent(ctx, PairingService::class.java))
        }
    }
}
