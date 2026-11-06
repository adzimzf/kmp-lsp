package io.github.hessesian.debugserver

import com.microsoft.java.debug.core.DebugUtility
import com.sun.jdi.Bootstrap
import com.sun.jdi.VirtualMachine
import com.sun.jdi.VirtualMachineManager
import com.sun.jdi.connect.Connector
import com.sun.jdi.connect.LaunchingConnector
import com.sun.jdi.connect.ListeningConnector
import com.sun.jdi.connect.Transport
import com.sun.jdi.connect.VMStartException
import java.io.File
import java.io.IOException
import java.lang.reflect.InvocationHandler
import java.lang.reflect.InvocationTargetException
import java.lang.reflect.Proxy
import java.util.concurrent.CompletableFuture
import java.util.concurrent.ExecutionException
import java.util.concurrent.TimeUnit

/**
 * The JDK's JDI with a launcher that honours a launch configuration's `cwd`
 * and `env`, like jdtls' `AdvancedVirtualMachineManager`.
 *
 * The JDK's own `SunCommandLineLauncher` has no working-directory or
 * environment argument, and java-debug picks it whenever it is listed, so
 * it is left out of [launchingConnectors].
 */
class KmpVirtualMachineManager(
    private val jdk: VirtualMachineManager = Bootstrap.virtualMachineManager(),
) : VirtualMachineManager by jdk {
    private val launcher = KmpLaunchingConnector(jdk)

    override fun defaultConnector(): LaunchingConnector = launcher

    override fun launchingConnectors(): List<LaunchingConnector> = listOf(launcher)

    override fun allConnectors(): List<Connector> = listOf<Connector>(launcher) + jdk.allConnectors()
}

/**
 * Starts the debuggee itself (in `cwd`, with `env`) and lets it connect back
 * to a JDWP socket this process listens on.
 */
class KmpLaunchingConnector(private val jdk: VirtualMachineManager) : LaunchingConnector {
    override fun name(): String = "io.github.hessesian.debugserver.KmpLaunchingConnector"

    override fun description(): String = "Launches a JVM in a working directory with an environment"

    override fun transport(): Transport = Transport { "dt_socket" }

    override fun defaultArguments(): MutableMap<String, Connector.Argument> = linkedMapOf(
        DebugUtility.HOME to StringArgument(DebugUtility.HOME, System.getProperty("java.home")),
        DebugUtility.OPTIONS to StringArgument(DebugUtility.OPTIONS, ""),
        DebugUtility.MAIN to StringArgument(DebugUtility.MAIN, "", mustSpecify = true),
        DebugUtility.SUSPEND to StringArgument(DebugUtility.SUSPEND, "true"),
        DebugUtility.QUOTE to StringArgument(DebugUtility.QUOTE, "\""),
        DebugUtility.EXEC to StringArgument(DebugUtility.EXEC, if (isWindows) "java.exe" else "java"),
        DebugUtility.CWD to StringArgument(DebugUtility.CWD, null),
        DebugUtility.ENV to StringArgument(DebugUtility.ENV, null),
    )

    override fun launch(arguments: Map<String, Connector.Argument>): VirtualMachine {
        val listener = socketListener()
        val listenArguments = listener.defaultArguments()
        listenArguments["localAddress"]?.setValue("127.0.0.1")
        listenArguments["port"]?.setValue("0")
        listenArguments["timeout"]?.setValue(ACCEPT_TIMEOUT_MILLIS.toString())
        val address = listener.startListening(listenArguments)

        val command = launchCommand(arguments, address)
        val workingDirectory = arguments[DebugUtility.CWD]?.value()
            ?.takeIf { it.isNotBlank() }
            ?.let(::File)
            ?.takeIf { it.isDirectory }
        val environment = runCatching { DebugUtility.decodeArrayArgument(arguments[DebugUtility.ENV]?.value()) }
            .getOrNull()
        val process = try {
            Runtime.getRuntime().exec(command.toTypedArray(), environment, workingDirectory)
        } catch (error: IOException) {
            runCatching { listener.stopListening(listenArguments) }
            throw error
        }

        val connected = CompletableFuture.supplyAsync {
            listener.accept(listenArguments)
        }
        // A debuggee that dies before connecting (bad classpath, missing main
        // class) must fail the launch, not wait out the accept timeout.
        process.onExit().thenRun {
            if (!connected.isDone) {
                runCatching { listener.stopListening(listenArguments) }
                connected.completeExceptionally(
                    VMStartException("the debuggee exited with status ${process.exitValue()}", process),
                )
            }
        }
        val virtualMachine = try {
            connected.get(ACCEPT_TIMEOUT_MILLIS + 1_000L, TimeUnit.MILLISECONDS)
        } catch (error: ExecutionException) {
            process.destroy()
            throw (error.cause as? VMStartException) ?: IOException(error.cause)
        } finally {
            runCatching { listener.stopListening(listenArguments) }
        }
        return withProcess(virtualMachine, process)
    }

    private fun socketListener(): ListeningConnector =
        jdk.listeningConnectors().first { it.transport().name() == "dt_socket" }

    private fun launchCommand(arguments: Map<String, Connector.Argument>, address: String): List<String> {
        val home = arguments[DebugUtility.HOME]?.value() ?: System.getProperty("java.home")
        val executable = arguments[DebugUtility.EXEC]?.value() ?: "java"
        val suspend = arguments[DebugUtility.SUSPEND]?.value()?.toBoolean() ?: true
        val options = arguments[DebugUtility.OPTIONS]?.value().orEmpty()
        val main = arguments[DebugUtility.MAIN]?.value().orEmpty()
        return buildList {
            add(File(File(home, "bin"), executable).path)
            add("-agentlib:jdwp=transport=dt_socket,address=$address,server=n,suspend=${if (suspend) "y" else "n"}")
            addAll(DebugUtility.parseArguments(options))
            addAll(DebugUtility.parseArguments(main))
        }
    }

    private class StringArgument(
        private val name: String,
        private var value: String?,
        private val mustSpecify: Boolean = false,
    ) : Connector.StringArgument {
        override fun name(): String = name
        override fun label(): String = name
        override fun description(): String = name
        override fun value(): String? = value
        override fun setValue(value: String?) {
            this.value = value
        }
        override fun isValid(value: String?): Boolean = true
        override fun mustSpecify(): Boolean = mustSpecify
    }

    private companion object {
        const val ACCEPT_TIMEOUT_MILLIS = 20_000
        val isWindows = System.getProperty("os.name").lowercase().startsWith("windows")
    }
}

/**
 * The JDK's `VirtualMachine` from a listening connector has no `process()`;
 * java-debug reads the debuggee's output and terminates it through it.
 */
internal fun withProcess(virtualMachine: VirtualMachine, process: Process): VirtualMachine {
    val handler = InvocationHandler { _, method, arguments ->
        if (method.name == "process" && method.parameterCount == 0) {
            return@InvocationHandler process
        }
        try {
            method.invoke(virtualMachine, *(arguments ?: emptyArray()))
        } catch (error: InvocationTargetException) {
            throw error.targetException
        }
    }
    return Proxy.newProxyInstance(
        VirtualMachine::class.java.classLoader,
        arrayOf(VirtualMachine::class.java),
        handler,
    ) as VirtualMachine
}
