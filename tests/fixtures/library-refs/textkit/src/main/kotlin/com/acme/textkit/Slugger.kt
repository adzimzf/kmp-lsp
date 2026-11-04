package com.acme.textkit

class Slugger(private val separator: Char = '-') {
    fun slug(title: String): String = Normalizer.normalize(title).replace(' ', separator)
}
