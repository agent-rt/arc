package com.github.agent_rt.arc

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import java.io.IOException
import java.net.InetSocketAddress
import java.net.NetworkInterface
import java.net.ServerSocket

/**
 * Discovers the local adb wireless service by mDNS (adapted from Shizuku's
 * AdbMdns). `_adb-tls-pairing._tcp` appears only while the pairing dialog is
 * open; resolving it gives the ephemeral port the user would otherwise have to
 * read — so the user only types the 6-digit code. [onPort] fires with the port
 * (>0) when a live local service is found.
 */
class AdbMdns(
    context: Context,
    private val serviceType: String,
    private val onPort: (Int) -> Unit,
) {
    private val nsd: NsdManager = context.getSystemService(NsdManager::class.java)
    private var running = false

    private val listener = object : NsdManager.DiscoveryListener {
        override fun onDiscoveryStarted(t: String) {}
        override fun onStartDiscoveryFailed(t: String, e: Int) {}
        override fun onDiscoveryStopped(t: String) {}
        override fun onStopDiscoveryFailed(t: String, e: Int) {}
        override fun onServiceLost(info: NsdServiceInfo) {}
        override fun onServiceFound(info: NsdServiceInfo) {
            nsd.resolveService(info, object : NsdManager.ResolveListener {
                override fun onResolveFailed(i: NsdServiceInfo, e: Int) {}
                override fun onServiceResolved(r: NsdServiceInfo) = onResolved(r)
            })
        }
    }

    fun start() {
        if (running) return
        running = true
        nsd.discoverServices(serviceType, NsdManager.PROTOCOL_DNS_SD, listener)
    }

    fun stop() {
        if (!running) return
        running = false
        runCatching { nsd.stopServiceDiscovery(listener) }
    }

    /** Accept only a service on one of our own interfaces whose port is live. */
    private fun onResolved(r: NsdServiceInfo) {
        if (!running) return
        val isLocal = NetworkInterface.getNetworkInterfaces().asSequence().any { ni ->
            ni.inetAddresses.asSequence().any { r.host?.hostAddress == it.hostAddress }
        }
        if (isLocal && portInUse(r.port)) onPort(r.port)
    }

    private fun portInUse(port: Int): Boolean = try {
        ServerSocket().use { it.bind(InetSocketAddress("127.0.0.1", port), 1); false }
    } catch (_: IOException) {
        true
    }

    companion object {
        const val TLS_PAIRING = "_adb-tls-pairing._tcp"
        const val TLS_CONNECT = "_adb-tls-connect._tcp"
    }
}
