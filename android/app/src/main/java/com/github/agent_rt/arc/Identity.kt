package com.github.agent_rt.arc

import android.content.Context

/**
 * The app's persistent adb identity (PKCS#8 PEM), generated once via the native
 * engine and stored in SharedPreferences. Shared by the UI and the broadcast
 * receiver so both pair/connect with the same key.
 */
fun adbKey(ctx: Context): String {
    val prefs = ctx.getSharedPreferences("arc", Context.MODE_PRIVATE)
    prefs.getString("adbkey", null)?.let { return it }
    val pem = AdbNative.generateKey()
    prefs.edit().putString("adbkey", pem).apply()
    return pem
}
