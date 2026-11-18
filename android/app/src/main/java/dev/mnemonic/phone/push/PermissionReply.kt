package dev.mnemonic.phone.push

import dev.mnemonic.phone.api.MacResult

fun replyResultText(result: MacResult<*>, allowed: Boolean): String =
    when (result) {
        is MacResult.Ok -> if (allowed) "Allowed" else "Denied"
        is MacResult.Refused -> if (result.status == 409) "Already answered" else "Couldn't reach the Mac"
        is MacResult.Unreachable -> "Couldn't reach the Mac"
    }
