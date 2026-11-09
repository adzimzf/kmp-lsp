package io.github.hessesian.jarindexer

import io.github.hessesian.jarindexer.model.SymbolEntry
import java.io.ByteArrayInputStream
import java.util.zip.ZipInputStream

/**
 * Index all public symbols from a JAR or AAR file.
 *
 * - JAR: iterate `.class` entries directly
 * - AAR: find `classes.jar` entry inside the outer ZIP, then treat as JAR
 *
 * If a `-sources.jar` sibling is found in the Gradle cache, KDoc comments
 * are extracted and attached to matching symbols.
 */
fun indexJarFile(path: String): List<SymbolEntry> {
    return try {
        val kdocMap = SourcesKdocReader.readKdocMap(path)
        val bytes = java.io.File(path).readBytes()
        val symbols = when {
            path.endsWith(".aar", ignoreCase = true) -> indexAar(bytes)
            path.endsWith(".jmod", ignoreCase = true) -> indexJmod(bytes)
            else -> indexJarBytes(bytes)
        }
        if (kdocMap.isEmpty()) symbols
        else symbols.map { sym ->
            val doc = kdocMap[sym.name] ?: ""
            if (doc.isEmpty()) sym else sym.copy(doc = doc)
        }
    } catch (_: Exception) {
        emptyList()
    }
}

private fun indexAar(aarBytes: ByteArray): List<SymbolEntry> {
    ZipInputStream(ByteArrayInputStream(aarBytes)).use { zip ->
        var entry = zip.nextEntry
        while (entry != null) {
            if (entry.name == "classes.jar") {
                val classesJar = zip.readBytes()
                return indexJarBytes(classesJar)
            }
            entry = zip.nextEntry
        }
    }
    return emptyList()
}

/** A JMOD starts with the 4-byte magic `JM 0x01 0x00`, then a plain ZIP. */
private const val JMOD_HEADER_SIZE = 4

/**
 * The public API of a JDK module (`jmods/java.base.jmod`): what a JDK without
 * its `src.zip` still ships. Its classes live under `classes/`; only the
 * `java.*` / `javax.*` packages are indexed, never `sun.*` / `jdk.internal.*`.
 */
private fun indexJmod(jmodBytes: ByteArray): List<SymbolEntry> {
    if (jmodBytes.size <= JMOD_HEADER_SIZE || jmodBytes[0] != 'J'.code.toByte() || jmodBytes[1] != 'M'.code.toByte()) {
        return emptyList()
    }
    val zipBytes = jmodBytes.copyOfRange(JMOD_HEADER_SIZE, jmodBytes.size)
    return indexJarBytes(zipBytes) { name ->
        name.startsWith("classes/java/") || name.startsWith("classes/javax/")
    }
}

private fun indexJarBytes(
    jarBytes: ByteArray,
    includeEntry: (String) -> Boolean = { true },
): List<SymbolEntry> {
    val results = mutableListOf<SymbolEntry>()
    ZipInputStream(ByteArrayInputStream(jarBytes)).use { zip ->
        var entry = zip.nextEntry
        while (entry != null) {
            if (!entry.isDirectory && entry.name.endsWith(".class") && includeEntry(entry.name)) {
                val classBytes = zip.readBytes()
                results += indexClassBytes(classBytes)
            }
            entry = zip.nextEntry
        }
    }
    return results
}
