package dev.mnemonic.phone.api

import kotlinx.serialization.KSerializer
import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.descriptors.PrimitiveKind
import kotlinx.serialization.descriptors.PrimitiveSerialDescriptor
import kotlinx.serialization.descriptors.SerialDescriptor
import kotlinx.serialization.encoding.Decoder
import kotlinx.serialization.encoding.Encoder
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonContentPolymorphicSerializer
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive

val MacJson = Json { ignoreUnknownKeys = true }

object TeamAgentStateSerializer : KSerializer<TeamAgentState> {
    override val descriptor: SerialDescriptor =
        PrimitiveSerialDescriptor("TeamAgentState", PrimitiveKind.STRING)

    override fun deserialize(decoder: Decoder): TeamAgentState =
        when (decoder.decodeString()) {
            "working" -> TeamAgentState.Working
            "waiting_for_approval" -> TeamAgentState.WaitingForApproval
            "error" -> TeamAgentState.Error
            else -> TeamAgentState.Idle
        }

    override fun serialize(encoder: Encoder, value: TeamAgentState) {
        encoder.encodeString(
            when (value) {
                TeamAgentState.Idle -> "idle"
                TeamAgentState.Working -> "working"
                TeamAgentState.WaitingForApproval -> "waiting_for_approval"
                TeamAgentState.Error -> "error"
            },
        )
    }
}

@Serializable(with = TeamAgentStateSerializer::class)
enum class TeamAgentState { Idle, Working, WaitingForApproval, Error }

@Serializable
data class TeamChat(
    val id: String,
    val title: String,
    val agent: String,
    val open: Boolean = false,
    val live: Boolean = false,
    @SerialName("last_turn_ended_at") val lastTurnEndedAt: Long? = null,
    @SerialName("last_turn_errored") val lastTurnErrored: Boolean = false,
    val waiting: Boolean = false,
)

@Serializable
data class TeamWorkspace(
    val id: String,
    val project: String,
    val name: String,
    val branch: String,
    @SerialName("pr_number") val prNumber: Long? = null,
    @SerialName("pr_state") val prState: String? = null,
    @SerialName("changed_files") val changedFiles: Int = 0,
    val additions: Int = 0,
    val deletions: Int = 0,
    @SerialName("agent_state") val agentState: TeamAgentState = TeamAgentState.Idle,
    @SerialName("idle_seconds") val idleSeconds: Long? = null,
    val chats: List<TeamChat> = emptyList(),
    @SerialName("step_in") val stepIn: Boolean = false,
    @SerialName("base_branch") val baseBranch: String? = null,
)

@Serializable
data class TeamWorkspaceList(val workspaces: List<TeamWorkspace> = emptyList())

@Serializable
data class PermissionOption(val optionId: String, val name: String, val kind: String = "")

@Serializable
data class PlanEntry(val content: String = "", val priority: String = "", val status: String = "")

object TeamEntrySerializer : JsonContentPolymorphicSerializer<TeamEntry>(TeamEntry::class) {
    override fun selectDeserializer(element: JsonElement) =
        when (element.jsonObject["kind"]?.jsonPrimitive?.content) {
            "updates" -> TeamEntry.Updates.serializer()
            "completed_plan" -> TeamEntry.CompletedPlan.serializer()
            "compaction" -> TeamEntry.Compaction.serializer()
            else -> TeamEntry.Unknown.serializer()
        }
}

@Serializable(with = TeamEntrySerializer::class)
sealed interface TeamEntry {
    @Serializable
    data class Updates(val updates: List<JsonObject> = emptyList()) : TeamEntry

    @Serializable
    data class CompletedPlan(val entries: List<PlanEntry> = emptyList()) : TeamEntry

    @Serializable
    data class Compaction(val summary: String? = null) : TeamEntry

    @Serializable
    data class Unknown(val kind: String = "") : TeamEntry
}

@Serializable
data class TeamEntryChange(val index: Int, val entry: TeamEntry)

@Serializable
data class TeamPermissionRequest(
    @SerialName("tool_call_id") val toolCallId: String,
    val options: List<PermissionOption> = emptyList(),
)

@Serializable
data class TeamChatUpdate(
    val cursor: String,
    val reset: Boolean,
    val len: Int,
    val changes: List<TeamEntryChange> = emptyList(),
    val plan: List<PlanEntry> = emptyList(),
    val generating: Boolean = false,
    val permissions: List<TeamPermissionRequest> = emptyList(),
)

@Serializable
data class PhonePrStatus(
    val found: Boolean = false,
    val number: Long? = null,
    val title: String? = null,
    val url: String? = null,
    val state: String? = null,
    @SerialName("is_draft") val isDraft: Boolean = false,
    @SerialName("merge_state_status") val mergeStateStatus: String? = null,
    @SerialName("head_ref_oid") val headRefOid: String? = null,
    @SerialName("checks_summary") val checksSummary: String = "",
    @SerialName("can_merge") val canMerge: Boolean = false,
    @SerialName("merge_method") val mergeMethod: String = "",
)

@Serializable
data class PhoneChatOptionValue(val id: String, val name: String)

/** A chat's model or effort. [current] is null in an agent's list, which no chat is using. */
@Serializable
data class PhoneChatOption(
    val id: String,
    val name: String,
    val kind: String = "",
    val current: String? = null,
    val values: List<PhoneChatOptionValue> = emptyList(),
)

@Serializable
data class PhoneChatOptionList(val options: List<PhoneChatOption> = emptyList())

@Serializable
data class PhoneOptionChoice(@SerialName("config_id") val configId: String, val value: String)

@Serializable
data class PhoneAgent(val id: String, val label: String, val options: List<PhoneChatOption> = emptyList())

@Serializable
data class PhoneAgentList(val agents: List<PhoneAgent> = emptyList())

@Serializable
data class PhoneRepository(val id: String, val name: String)

@Serializable
data class PhoneRepositoryList(val repositories: List<PhoneRepository> = emptyList())

@Serializable
data class PhoneTranscript(val text: String)

@Serializable
data class PhoneActionOutcome(
    @SerialName("chat_id") val chatId: String? = null,
    @SerialName("workspace_id") val workspaceId: String? = null,
    val message: String? = null,
    val options: List<PhoneChatOption>? = null,
)
