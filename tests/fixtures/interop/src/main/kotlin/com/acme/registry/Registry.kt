package com.acme.registry

import com.acme.model.User

object Registry {
    private val users = mutableListOf<User>()

    fun register(user: User) {
        users += user
    }

    @JvmName("removeAll")
    fun clear() {
        users.clear()
    }
}
