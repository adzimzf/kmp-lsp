package com.acme.textkit

object Normalizer {
    fun normalize(text: String): String = Whitespace.normalize(text).lowercase()
}
