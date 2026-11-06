package io.github.hessesian.debugserver

import com.google.gson.Gson
import com.google.gson.JsonElement
import com.google.gson.JsonObject
import com.google.gson.JsonParser
import java.io.BufferedReader
import java.io.InputStream
import java.io.InputStreamReader
import java.io.PrintStream
import java.nio.charset.StandardCharsets
import java.util.concurrent.CompletableFuture
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicLong
import java.util.logging.Level
import java.util.logging.Logger

/**
 * The control channel to kmp-lsp: newline-delimited JSON on this process's
 * stdio (the Debug Adapter Protocol itself runs on a TCP socket).
 *
 * The debug server asks (`{"id":1,"method":…,"params":…}`) and kmp-lsp answers
 * (`{"id":1,"result":…}`); kmp-lsp also sends notifications without an id
 * (`{"method":"updateDebugSettings",…}`). This is the channel jdtls doesn't
 * need, because there java-debug runs inside the language server.
 */
class LspBridge(input: InputStream, private val output: PrintStream) {
    private val reader = BufferedReader(InputStreamReader(input, StandardCharsets.UTF_8))
    private val gson = Gson()
    private val nextId = AtomicLong(1)
    private val pending = ConcurrentHashMap<Long, CompletableFuture<JsonElement?>>()

    /** Read kmp-lsp's messages on a daemon thread; [onClosed] runs when kmp-lsp goes away. */
    fun start(onNotification: (method: String, params: JsonElement?) -> Unit, onClosed: () -> Unit) {
        val thread = Thread({
            try {
                while (true) {
                    val line = reader.readLine() ?: break
                    dispatch(line, onNotification)
                }
            } catch (error: Exception) {
                logger.log(Level.WARNING, "kmp-lsp control channel failed", error)
            }
            pending.values.forEach { it.complete(null) }
            onClosed()
        }, "kmp-lsp-bridge")
        thread.isDaemon = true
        thread.start()
    }

    private fun dispatch(line: String, onNotification: (String, JsonElement?) -> Unit) {
        val message = runCatching { JsonParser.parseString(line).asJsonObject }.getOrNull() ?: return
        val id = message.get("id")
        if (id != null && !id.isJsonNull) {
            pending.remove(id.asLong)?.complete(message.get("result")?.takeUnless { it.isJsonNull })
            return
        }
        val method = message.get("method")?.asString ?: return
        onNotification(method, message.get("params"))
    }

    /** Tell kmp-lsp the Debug Adapter Protocol port. */
    fun announceStarted(port: Int) {
        val event = JsonObject()
        event.addProperty("event", "started")
        event.addProperty("port", port)
        write(event)
    }

    /**
     * Ask kmp-lsp; `null` when it has no answer, doesn't answer within
     * [timeoutSeconds], or is gone.
     */
    fun request(method: String, params: Any, timeoutSeconds: Long = 10): JsonElement? {
        val id = nextId.getAndIncrement()
        val answer = CompletableFuture<JsonElement?>()
        pending[id] = answer
        val message = JsonObject()
        message.addProperty("id", id)
        message.addProperty("method", method)
        message.add("params", gson.toJsonTree(params))
        write(message)
        return try {
            answer.get(timeoutSeconds, TimeUnit.SECONDS)
        } catch (error: Exception) {
            logger.log(Level.WARNING, "kmp-lsp did not answer $method", error)
            null
        } finally {
            pending.remove(id)
        }
    }

    private fun write(message: JsonObject) {
        val line = gson.toJson(message)
        synchronized(output) {
            output.print(line)
            output.print('\n')
            output.flush()
        }
    }

    private companion object {
        val logger: Logger = Logger.getLogger("kmp-debug-server")
    }
}
