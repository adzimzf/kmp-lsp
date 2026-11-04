package com.acme.app

import com.acme.greeter.Greeter

fun welcome(name: String): String {
    val greeter = Greeter("Hello")
    return greeter.greet(name)
}
