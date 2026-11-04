package com.acme.model

class User(val name: String, var email: String) {
    val isActive: Boolean = true

    var nickname: String? = null

    fun displayName(): String = nickname ?: name

    companion object {
        const val MAX_NAME_LENGTH = 64

        fun anonymous(): User = User("anonymous", "")
    }
}
