package io.github.hessesian.debugserver

import com.microsoft.java.debug.core.IEvaluatableBreakpoint
import com.microsoft.java.debug.core.adapter.IEvaluationProvider
import com.sun.jdi.ArrayReference
import com.sun.jdi.ClassType
import com.sun.jdi.Method
import com.sun.jdi.ObjectReference
import com.sun.jdi.ReferenceType
import com.sun.jdi.StackFrame
import com.sun.jdi.ThreadReference
import com.sun.jdi.Value
import java.util.concurrent.CompletableFuture
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Executors

/**
 * Expression evaluation without a compiler.
 *
 * jdtls compiles watch expressions, conditions and logpoints with the JDT
 * compiler against the project model. kmp-lsp's debug server evaluates what
 * needs no compiler — variable and field paths such as `user`,
 * `this.state.items.length`, `request.headers` — and invokes methods the
 * debugger itself asks for (`toString()` in the Variables view, lazy
 * `getter`s), which JDI does directly.
 */
class KmpEvaluationProvider : IEvaluationProvider {
    private val evaluating: MutableSet<ThreadReference> = ConcurrentHashMap.newKeySet()
    private val executor = Executors.newCachedThreadPool { runnable ->
        Thread(runnable, "kmp-debug-evaluation").apply { isDaemon = true }
    }

    override fun isInEvaluation(thread: ThreadReference): Boolean = thread in evaluating

    override fun evaluate(expression: String, thread: ThreadReference, depth: Int): CompletableFuture<Value?> =
        supply { PathEvaluator.evaluate(expression, thread.frame(depth)) }

    override fun evaluate(
        expression: String,
        thisContext: ObjectReference,
        thread: ThreadReference,
    ): CompletableFuture<Value?> = supply { PathEvaluator.evaluateOn(thisContext, expression) }

    override fun evaluateForBreakpoint(
        breakpoint: IEvaluatableBreakpoint,
        thread: ThreadReference,
    ): CompletableFuture<Value?> = CompletableFuture.failedFuture(
        UnsupportedOperationException(
            "conditional breakpoints and logpoints need a Java expression compiler, " +
                "which kmp-lsp's debug server doesn't have",
        ),
    )

    override fun invokeMethod(
        thisContext: ObjectReference,
        methodName: String,
        methodSignature: String?,
        args: Array<Value>?,
        thread: ThreadReference,
        invokeSuper: Boolean,
    ): CompletableFuture<Value?> = supply {
        val arguments = args?.toList() ?: emptyList()
        val owner: ReferenceType = if (invokeSuper) {
            (thisContext.referenceType() as? ClassType)?.superclass() ?: thisContext.referenceType()
        } else {
            thisContext.referenceType()
        }
        val method = findMethod(owner, methodName, methodSignature, arguments.size)
            ?: throw NoSuchMethodException("$methodName${methodSignature ?: ""} in ${owner.name()}")
        var options = ObjectReference.INVOKE_SINGLE_THREADED
        if (invokeSuper) options = options or ObjectReference.INVOKE_NONVIRTUAL
        evaluating.add(thread)
        try {
            thisContext.invokeMethod(thread, method, arguments, options)
        } finally {
            evaluating.remove(thread)
        }
    }

    override fun clearState(thread: ThreadReference) {
        evaluating.remove(thread)
    }

    private fun findMethod(owner: ReferenceType, name: String, signature: String?, arity: Int): Method? {
        val candidates = if (signature != null) {
            owner.methodsByName(name, signature)
        } else {
            owner.methodsByName(name).filter { it.argumentTypeNames().size == arity }
        }
        return candidates.firstOrNull { !it.isAbstract }
    }

    private fun supply(work: () -> Value?): CompletableFuture<Value?> =
        CompletableFuture.supplyAsync(work, executor)
}

/** `a.b.c` against a stack frame or an object. */
internal object PathEvaluator {
    private val identifier = Regex("[\\p{L}_$][\\p{L}\\p{N}_$]*")

    fun evaluate(expression: String, frame: StackFrame): Value? {
        val path = parse(expression)
        val root = resolveRoot(path.first(), frame)
        return walk(root, path.drop(1), expression)
    }

    fun evaluateOn(target: ObjectReference, expression: String): Value? {
        val path = parse(expression)
        val start = if (path.first() == "this") path.drop(1) else path
        return walk(target, start, expression)
    }

    /** The identifiers of a dotted path; anything else needs a compiler. */
    fun parse(expression: String): List<String> {
        val segments = expression.trim().split('.').map { it.trim() }
        val isPath = segments.isNotEmpty() && segments.all { identifier.matches(it) }
        if (!isPath) {
            throw UnsupportedOperationException(
                "'$expression': only variable and field paths (a.b.c) can be evaluated",
            )
        }
        return segments
    }

    private fun resolveRoot(name: String, frame: StackFrame): Value? {
        if (name == "this") {
            return frame.thisObject() ?: throw IllegalStateException("there is no 'this' in a static method")
        }
        val variable = runCatching { frame.visibleVariableByName(name) }.getOrNull()
        if (variable != null) return frame.getValue(variable)
        frame.thisObject()?.let { self ->
            self.referenceType().fieldByName(name)?.let { return self.getValue(it) }
        }
        val declaringType = frame.location().declaringType()
        declaringType.fieldByName(name)?.takeIf { it.isStatic }?.let { return declaringType.getValue(it) }
        throw NoSuchFieldException("'$name' is not a variable or field here")
    }

    private fun walk(start: Value?, path: List<String>, expression: String): Value? {
        var current = start
        for (segment in path) {
            val target = current as? ObjectReference
                ?: throw NullPointerException("'$expression': '$segment' read from null or a primitive")
            current = if (target is ArrayReference && segment == "length") {
                target.virtualMachine().mirrorOf(target.length())
            } else {
                val field = target.referenceType().fieldByName(segment)
                    ?: throw NoSuchFieldException("'$segment' is not a field of ${target.referenceType().name()}")
                target.getValue(field)
            }
        }
        return current
    }
}
