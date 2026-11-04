package com.acme.app

import com.acme.legacy.Account
import com.acme.legacy.Point

class KotlinClient {
    fun settle(account: Account): Long {
        account.balance = 100L
        account.owner = "acme"
        val locked = account.isLocked
        return if (locked) 0L else account.balance
    }

    fun move(point: Point): Int {
        val moved = point.translate(1, 1)
        return moved.x + point.y
    }
}
