package com.acme.app

import com.acme.textkit.Normalizer
import com.acme.textkit.Slugger

fun headline(title: String): String {
    val clean = Normalizer.normalize(title)
    return Slugger().slug(clean)
}
