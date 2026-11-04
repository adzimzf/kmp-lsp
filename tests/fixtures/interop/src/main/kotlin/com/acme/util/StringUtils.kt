package com.acme.util

fun slugify(input: String): String = input.trim().lowercase().replace(' ', '-')

fun String.shout(): String = uppercase() + "!"
