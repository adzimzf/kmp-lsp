package com.acme.textkit

/** Same member name as [Normalizer.normalize] — never one of its references. */
object Whitespace {
    fun normalize(text: String): String = text.trim().replace(Regex("\\s+"), " ")
}
