package dev.mnemonic.phone.chat

import dev.mnemonic.phone.api.TeamAgentState

enum class ChatStatus { Working, NeedsYou, Error, Idle }

fun chatStatus(generating: Boolean, hasPendingPermission: Boolean, agentState: TeamAgentState?): ChatStatus =
    when {
        hasPendingPermission -> ChatStatus.NeedsYou
        generating -> ChatStatus.Working
        agentState == TeamAgentState.Error -> ChatStatus.Error
        else -> ChatStatus.Idle
    }
