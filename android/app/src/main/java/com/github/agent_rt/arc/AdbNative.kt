package com.github.agent_rt.arc

/**
 * JNI bridge to the Rust adb engine (`libarc_adb.so`, crate `arc-adb`).
 *
 * These run at the app's own uid, connecting to the device's own `adbd` over
 * localhost wireless-debugging — the shell they spawn on the device runs at the
 * shell uid, which is where the arc runner needs to live. All four calls block,
 * so invoke them off the main thread; they throw [RuntimeException] on failure.
 */
object AdbNative {
    init {
        System.loadLibrary("arc_adb")
    }

    /** Generates a fresh adb RSA-2048 identity, returned as PKCS#8 PEM to persist. */
    external fun generateKey(): String

    /** One-time pairing with `adbd`'s pairing endpoint using the 6-digit code. */
    external fun pair(hostPort: String, code: String, keyPem: String, name: String)

    /** Pushes `data` to `remotePath` (mode e.g. 0o755) via the adb sync service. */
    external fun pushFile(
        hostPort: String,
        keyPem: String,
        data: ByteArray,
        remotePath: String,
        mode: Int,
        mtime: Int,
    )

    /** Connects and runs `shell:<command>`, returning its raw combined output. */
    external fun runShell(hostPort: String, keyPem: String, command: String): ByteArray
}
