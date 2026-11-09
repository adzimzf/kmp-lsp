package io.github.hessesian.debugserver

import com.microsoft.java.debug.core.JavaBreakpointLocation
import com.microsoft.java.debug.core.adapter.HotCodeReplaceEvent
import com.microsoft.java.debug.core.adapter.ICompletionsProvider
import com.microsoft.java.debug.core.adapter.IEvaluationProvider
import com.microsoft.java.debug.core.adapter.IHotCodeReplaceProvider
import com.microsoft.java.debug.core.adapter.IProviderContext
import com.microsoft.java.debug.core.adapter.ISourceLookUpProvider
import com.microsoft.java.debug.core.adapter.IVirtualMachineManagerProvider
import com.microsoft.java.debug.core.adapter.ProviderContext
import com.microsoft.java.debug.core.adapter.Source
import com.microsoft.java.debug.core.adapter.SourceType
import com.microsoft.java.debug.core.protocol.Types
import com.sun.jdi.StackFrame
import com.sun.jdi.VirtualMachineManager
import io.reactivex.Observable
import java.io.File
import java.net.URI
import java.nio.charset.StandardCharsets
import java.util.concurrent.CompletableFuture
import java.util.function.Consumer

/**
 * The five providers java-debug core requires, as jdtls'
 * `JdtProviderContextFactory` registers them — here backed by kmp-lsp and
 * the JDK's JDI instead of the JDT Java model and Eclipse JDI.
 */
fun createProviderContext(bridge: LspBridge): IProviderContext {
    val context = ProviderContext()
    context.registerProvider(ISourceLookUpProvider::class.java, KmpSourceLookUpProvider(bridge))
    context.registerProvider(IVirtualMachineManagerProvider::class.java, KmpVirtualMachineManagerProvider)
    context.registerProvider(IHotCodeReplaceProvider::class.java, NoHotCodeReplaceProvider())
    context.registerProvider(IEvaluationProvider::class.java, KmpEvaluationProvider())
    context.registerProvider(ICompletionsProvider::class.java, NoCompletionsProvider)
    return context
}

/**
 * Source ⇄ class mapping. jdtls parses the compilation unit with JDT; kmp-lsp
 * answers from its tree-sitter index, which also covers Kotlin.
 */
class KmpSourceLookUpProvider(private val bridge: LspBridge) : ISourceLookUpProvider {
    override fun supportsRealtimeBreakpointVerification(): Boolean = false

    @Deprecated("java-debug uses getBreakpointLocations")
    override fun getFullyQualifiedName(uri: String, lines: IntArray, columns: IntArray): Array<String> =
        breakpointClasses(uri, lines.toList()).map { it ?: "" }.toTypedArray()

    override fun getBreakpointLocations(
        sourceUri: String,
        sourceBreakpoints: Array<Types.SourceBreakpoint>,
    ): Array<JavaBreakpointLocation> {
        if (sourceBreakpoints.isEmpty()) return emptyArray()
        val classNames = breakpointClasses(sourceUri, sourceBreakpoints.map { it.line })
        return sourceBreakpoints
            .mapIndexed { index, breakpoint ->
                // A location without a class name is left unverified (not installed).
                JavaBreakpointLocation(breakpoint.line, breakpoint.column).apply {
                    classNames.getOrNull(index)?.let { setClassName(it) }
                }
            }
            .toTypedArray()
    }

    private fun breakpointClasses(uri: String, lines: List<Int>): List<String?> {
        val answer = bridge.request("breakpointClasses", mapOf("uri" to uri, "lines" to lines))
        val classNames = answer?.asJsonObject?.getAsJsonArray("classNames") ?: return lines.map { null }
        return classNames.map { element -> element.takeUnless { it.isJsonNull }?.asString }
    }

    @Deprecated("java-debug uses getSource")
    override fun getSourceFileURI(fullyQualifiedName: String, sourcePath: String?): String? {
        val answer = bridge.request(
            "sourceUri",
            mapOf("className" to fullyQualifiedName, "sourcePath" to sourcePath),
        )
        return answer?.asJsonObject?.get("uri")?.takeUnless { it.isJsonNull }?.asString
    }

    override fun getSource(fullyQualifiedName: String, sourcePath: String?): Source? {
        @Suppress("DEPRECATION")
        val uri = getSourceFileURI(fullyQualifiedName, sourcePath) ?: return null
        return Source(uri, SourceType.LOCAL)
    }

    override fun getSourceContents(uri: String): String {
        val file = runCatching { File(URI(uri)) }.getOrNull() ?: return ""
        return runCatching { file.readText(StandardCharsets.UTF_8) }.getOrDefault("")
    }

    /** Step-into targets need resolved call sites, which kmp-lsp doesn't offer here. */
    override fun findMethodInvocations(uri: String, line: Int): List<ISourceLookUpProvider.MethodInvocation> =
        emptyList()
}

object KmpVirtualMachineManagerProvider : IVirtualMachineManagerProvider {
    private val manager: VirtualMachineManager by lazy { KmpVirtualMachineManager() }

    override fun getVirtualMachineManager(): VirtualMachineManager = manager
}

/**
 * Hot code replace needs a build that recompiles changed classes, which
 * jdtls has (the Eclipse incremental builder) and kmp-lsp doesn't.
 */
class NoHotCodeReplaceProvider : IHotCodeReplaceProvider {
    override fun onClassRedefined(consumer: Consumer<List<String>>) {}

    override fun redefineClasses(): CompletableFuture<List<String>> = CompletableFuture.completedFuture(emptyList())

    override fun getEventHub(): Observable<HotCodeReplaceEvent> = Observable.empty()
}

/** Debug-console completion needs a compiler model of the frame; none here. */
object NoCompletionsProvider : ICompletionsProvider {
    override fun codeComplete(
        frame: StackFrame,
        snippet: String,
        line: Int,
        column: Int,
    ): List<Types.CompletionItem> = emptyList()
}
