@file:JvmName("TextHelper")

package com.acme.util

fun wordCount(text: String): Int = text.split(' ').count { it.isNotBlank() }
