package io.github.hessesian.debugserver

import com.google.gson.JsonParser
import com.microsoft.java.debug.core.DebugUtility
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertNull
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import org.junit.jupiter.api.assertThrows
import java.io.ByteArrayOutputStream
import java.io.PipedInputStream
import java.io.PipedOutputStream
import java.io.PrintStream
import java.util.concurrent.CompletableFuture
import java.util.concurrent.TimeUnit

class LspBridgeTest {
    @Test
    fun `answers are matched to their requests by id`() {
        val toServer = PipedOutputStream()
        val serverInput = PipedInputStream(toServer)
        val captured = ByteArrayOutputStream()
        val bridge = LspBridge(serverInput, PrintStream(captured, true))
        bridge.start(onNotification = { _, _ -> }, onClosed = {})

        val answer = CompletableFuture.supplyAsync {
            bridge.request("breakpointClasses", mapOf("uri" to "file:///a/Main.kt", "lines" to listOf(3)))
        }
        // Wait for the request line, then answer it as kmp-lsp would.
        val requestLine = waitForLine(captured)
        val request = JsonParser.parseString(requestLine).asJsonObject
        assertEquals("breakpointClasses", request["method"].asString)
        assertEquals(3, request["params"].asJsonObject["lines"].asJsonArray[0].asInt)
        val id = request["id"].asLong
        toServer.write("{\"id\":$id,\"result\":{\"classNames\":[\"a.MainKt\"]}}\n".toByteArray())
        toServer.flush()

        val result = answer.get(5, TimeUnit.SECONDS)!!.asJsonObject
        assertEquals("a.MainKt", result["classNames"].asJsonArray[0].asString)
    }

    @Test
    fun `notifications reach the handler and closing fails pending requests`() {
        val toServer = PipedOutputStream()
        val serverInput = PipedInputStream(toServer)
        val captured = ByteArrayOutputStream()
        val notified = CompletableFuture<String>()
        val closed = CompletableFuture<Unit>()
        val bridge = LspBridge(serverInput, PrintStream(captured, true))
        bridge.start(
            onNotification = { method, params ->
                notified.complete("$method:${params!!.asJsonObject["settings"].asString}")
            },
            onClosed = { closed.complete(Unit) },
        )
        toServer.write("{\"method\":\"updateDebugSettings\",\"params\":{\"settings\":\"{}\"}}\n".toByteArray())
        toServer.flush()
        assertEquals("updateDebugSettings:{}", notified.get(5, TimeUnit.SECONDS))

        val pending = CompletableFuture.supplyAsync { bridge.request("sourceUri", mapOf<String, String>()) }
        waitForLine(captured)
        toServer.close()
        closed.get(5, TimeUnit.SECONDS)
        assertNull(pending.get(5, TimeUnit.SECONDS))
    }

    @Test
    fun `started event carries the port`() {
        val captured = ByteArrayOutputStream()
        LspBridge(PipedInputStream(PipedOutputStream()), PrintStream(captured, true)).announceStarted(4711)
        assertEquals("{\"event\":\"started\",\"port\":4711}\n", captured.toString())
    }

    private fun waitForLine(captured: ByteArrayOutputStream): String {
        repeat(500) {
            val text = captured.toString()
            if (text.contains('\n')) return text.substringBefore('\n')
            Thread.sleep(10)
        }
        error("no line written")
    }
}

class PathEvaluatorTest {
    @Test
    fun `dotted paths are split into identifiers`() {
        assertEquals(listOf("this", "state", "items", "length"), PathEvaluator.parse(" this.state.items.length "))
        assertEquals(listOf("\$this\$apply"), PathEvaluator.parse("\$this\$apply"))
    }

    @Test
    fun `anything needing a compiler is refused with a reason`() {
        for (expression in listOf("a + 1", "list.size()", "a[0]", "", "a..b")) {
            val error = assertThrows<UnsupportedOperationException> { PathEvaluator.parse(expression) }
            assertTrue(error.message!!.contains("only variable and field paths"))
        }
    }
}

class KmpVirtualMachineManagerTest {
    @Test
    fun `only the cwd-aware launcher is offered`() {
        // java-debug picks the JDK's SunCommandLineLauncher whenever it is
        // listed, and that one ignores a launch configuration's cwd and env.
        val launchers = KmpVirtualMachineManager().launchingConnectors()
        assertEquals(1, launchers.size)
        val arguments = launchers[0].defaultArguments()
        assertTrue(arguments.containsKey(DebugUtility.CWD))
        assertTrue(arguments.containsKey(DebugUtility.ENV))
    }
}
