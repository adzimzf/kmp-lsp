package io.github.hessesian.debugserver

import com.microsoft.java.debug.core.DebugSettings
import com.microsoft.java.debug.core.adapter.ProtocolServer
import java.io.PrintStream
import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket
import java.util.concurrent.Executors
import java.util.logging.Level
import java.util.logging.Logger
import kotlin.system.exitProcess

private val logger: Logger = Logger.getLogger("kmp-debug-server")

/**
 * kmp-lsp's counterpart of jdtls' `JavaDebugServer`: one loopback TCP port,
 * one java-debug [ProtocolServer] per editor connection.
 */
fun main() {
    // stdout carries the control channel; anything else that prints
    // (libraries, a stray println) must not corrupt it.
    val controlOutput = PrintStream(System.out, true, Charsets.UTF_8)
    System.setOut(System.err)

    val bridge = LspBridge(System.`in`, controlOutput)
    bridge.start(
        onNotification = { method, params ->
            if (method == "updateDebugSettings") {
                val settings = params?.asJsonObject?.get("settings")?.asString
                if (settings != null) {
                    runCatching { DebugSettings.getCurrent().updateSettings(settings) }
                        .onFailure { logger.log(Level.WARNING, "invalid debug settings: $settings", it) }
                }
            }
        },
        // kmp-lsp exited: nobody can reach this server any more.
        onClosed = { exitProcess(0) },
    )

    val serverSocket = ServerSocket(0, 50, InetAddress.getLoopbackAddress())
    bridge.announceStarted(serverSocket.localPort)

    val connections = Executors.newCachedThreadPool { runnable ->
        Thread(runnable, "kmp-debug-session").apply { isDaemon = true }
    }
    while (true) {
        val connection = serverSocket.accept()
        connections.submit { serve(connection, bridge) }
    }
}

private fun serve(connection: Socket, bridge: LspBridge) {
    connection.use {
        try {
            ProtocolServer(it.getInputStream(), it.getOutputStream(), createProviderContext(bridge)).run()
        } catch (error: Exception) {
            logger.log(Level.SEVERE, "debug session failed", error)
        } finally {
            logger.info("debug connection closed")
        }
    }
}
