package dev.mnemonic.phone.home

import dev.mnemonic.phone.api.TeamAgentState
import dev.mnemonic.phone.api.TeamWorkspace
import org.junit.Assert.assertEquals
import org.junit.Test

class HomeOrderTest {
    private fun workspace(id: String, project: String, state: TeamAgentState, idle: Long?) =
        TeamWorkspace(id = id, project = project, name = id, branch = "b", agentState = state, idleSeconds = idle)

    @Test
    fun groupsAreAlphabeticalAndRowsGoByAttentionThenRecentActivity() {
        val groups = groupWorkspaces(
            listOf(
                workspace("idle-old", "zeta", TeamAgentState.Idle, 900),
                workspace("idle-null", "alpha", TeamAgentState.Idle, null),
                workspace("idle-new", "alpha", TeamAgentState.Idle, 10),
                workspace("working", "alpha", TeamAgentState.Working, 5),
                workspace("error", "alpha", TeamAgentState.Error, 50),
                workspace("waiting-late", "alpha", TeamAgentState.WaitingForApproval, 300),
                workspace("waiting-early", "alpha", TeamAgentState.WaitingForApproval, 20),
            ),
        )

        assertEquals(listOf("alpha", "zeta"), groups.map { it.first })
        assertEquals(
            listOf("waiting-early", "waiting-late", "error", "working", "idle-new", "idle-null"),
            groups[0].second.map { it.id },
        )
    }
}
