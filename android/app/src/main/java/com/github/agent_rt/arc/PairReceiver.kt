package com.github.agent_rt.arc

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.util.Log

/**
 * Pairs on a broadcast, so a controller (arc via `adb shell am broadcast`) can
 * read the ephemeral code from the wireless-debugging dialog by UI automation
 * and hand it to us — WITHOUT backgrounding that dialog. A broadcast doesn't
 * touch the foreground task, so the pairing port stays open (verified: the port
 * survives an `am broadcast`, unlike HOME). Pairing is fast, so `goAsync` + a
 * thread stays well within the broadcast time budget.
 *
 * ```
 * adb shell am broadcast -n com.github.agent_rt.arc/.PairReceiver \
 *   -a com.github.agent_rt.arc.PAIR --es endpoint 10.0.0.158:PORT --es code 123456
 * ```
 * Result is logged under tag `arcpair` (PAIR=ok / PAIR=err <msg>).
 */
class PairReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val endpoint = intent.getStringExtra("endpoint")
        val code = intent.getStringExtra("code")
        if (endpoint == null || code == null) {
            Log.e(TAG, "PAIR=err missing endpoint/code extras")
            return
        }
        val ctx = context.applicationContext
        val pending = goAsync()
        Thread {
            val (ok, msg) = try {
                AdbNative.pair(endpoint, code, adbKey(ctx), "arc@android")
                Log.i(TAG, "PAIR=ok endpoint=$endpoint")
                true to "paired"
            } catch (e: Throwable) {
                Log.e(TAG, "PAIR=err ${e.message}")
                false to (e.message ?: "error")
            }
            // Report back so an in-app listener (the float window) can show status.
            ctx.sendBroadcast(
                Intent(ACTION_RESULT)
                    .setPackage(ctx.packageName)
                    .putExtra("ok", ok)
                    .putExtra("msg", msg),
            )
            pending.finish()
        }.start()
    }

    companion object {
        const val TAG = "arcpair"
        const val ACTION_PAIR = "com.github.agent_rt.arc.PAIR"
        const val ACTION_RESULT = "com.github.agent_rt.arc.PAIR_RESULT"
    }
}
