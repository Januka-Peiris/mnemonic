package dev.mnemonic.phone.chat

import dev.mnemonic.phone.api.PermissionOption
import dev.mnemonic.phone.api.PlanEntry
import dev.mnemonic.phone.api.TeamChatUpdate
import dev.mnemonic.phone.api.TeamEntry
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.contentOrNull

sealed interface ChatItem {
    data class UserMessage(val text: String) : ChatItem
    data class AgentMessage(val text: String) : ChatItem
    data class Thought(val text: String) : ChatItem
    data class Tool(
        val id: String,
        val title: String,
        val status: String,
        val waiting: Boolean,
        val kind: String = "other",
    ) : ChatItem
    data class FoldedTools(val tools: List<Tool>, val label: String) : ChatItem
    data class Plan(val entries: List<PlanEntry>, val done: Boolean) : ChatItem
    data class Compaction(val summary: String) : ChatItem
}

class ChatState {
    private var entries: MutableList<TeamEntry?> = mutableListOf()

    var cursor: String? = null
        private set
    var generating: Boolean = false
        private set
    var permissions: Map<String, List<PermissionOption>> = emptyMap()
        private set
    var plan: List<PlanEntry> = emptyList()
        private set
    var items: List<ChatItem> = emptyList()
        private set

    fun apply(update: TeamChatUpdate) {
        if (update.reset) entries = mutableListOf()
        for (change in update.changes) {
            while (entries.size <= change.index) entries.add(null)
            entries[change.index] = change.entry
        }
        while (entries.size < update.len) entries.add(null)
        while (entries.size > update.len) entries.removeAt(entries.size - 1)

        permissions = update.permissions.associate { it.toolCallId to it.options }
        cursor = update.cursor
        generating = update.generating
        plan = update.plan
        items = buildItems()
    }

    private fun buildItems(): List<ChatItem> {
        val built = mutableListOf<ChatItem>()
        var run = mutableListOf<ChatItem.Tool>()
        fun flush() {
            if (run.isEmpty()) return
            built.add(ChatItem.FoldedTools(run.toList(), toolRunSummary(run)))
            run = mutableListOf()
        }
        for (entry in entries) {
            val entryItems = itemsFor(entry)
            val tool = entryItems.singleOrNull() as? ChatItem.Tool
            if (tool != null && !tool.waiting && tool.status != "in_progress") {
                run.add(tool)
                continue
            }
            // An entry that shows nothing doesn't break a run.
            if (entryItems.isEmpty()) continue
            flush()
            built.addAll(entryItems)
        }
        flush()
        if (plan.isNotEmpty()) built.add(ChatItem.Plan(plan, done = false))
        return built
    }

    private fun itemsFor(entry: TeamEntry?): List<ChatItem> =
        when (entry) {
            is TeamEntry.CompletedPlan -> listOf(ChatItem.Plan(entry.entries, done = true))
            is TeamEntry.Compaction -> listOf(ChatItem.Compaction(entry.summary.orEmpty()))
            is TeamEntry.Updates -> rebuildEntry(entry.updates)
            is TeamEntry.Unknown, null -> emptyList()
        }

    private class Part(val thought: Boolean, var text: String)

    private class BuiltTool(val id: String, var title: String, var kind: String, var status: String)

    // One entry is one thread entry: a user message, an agent message (with
    // any thoughts), or a tool call followed by its updates.
    private fun rebuildEntry(updates: List<JsonObject>): List<ChatItem> {
        var user = ""
        val parts = mutableListOf<Part>()
        var tool: BuiltTool? = null
        for (update in updates) {
            when (update.string("sessionUpdate")) {
                "user_message_chunk" -> user += contentText(update["content"])
                "agent_message_chunk", "agent_thought_chunk" -> {
                    val thought = update.string("sessionUpdate") == "agent_thought_chunk"
                    val last = parts.lastOrNull()
                    if (last != null && last.thought == thought) {
                        last.text += contentText(update["content"])
                    } else {
                        parts.add(Part(thought, contentText(update["content"])))
                    }
                }
                "tool_call" -> tool = BuiltTool(
                    id = update.string("toolCallId").orEmpty(),
                    title = update.string("title").orEmpty(),
                    kind = update.string("kind").orEmpty().ifEmpty { "other" },
                    status = update.string("status").orEmpty().ifEmpty { "pending" },
                )
                "tool_call_update" -> tool?.let { current ->
                    if (current.id == update.string("toolCallId")) {
                        nonEmpty(update.string("title"))?.let { current.title = it }
                        nonEmpty(update.string("kind"))?.let { current.kind = it }
                        nonEmpty(update.string("status"))?.let { current.status = it }
                    }
                }
            }
        }
        tool?.let {
            return listOf(ChatItem.Tool(it.id, it.title, it.status, permissions.containsKey(it.id), it.kind))
        }
        if (user.isNotEmpty()) return listOf(ChatItem.UserMessage(user))
        return parts.map { if (it.thought) ChatItem.Thought(it.text) else ChatItem.AgentMessage(it.text) }
    }

    private fun JsonObject.string(key: String): String? = (this[key] as? JsonPrimitive)?.contentOrNull

    private fun contentText(content: JsonElement?): String {
        val block = content as? JsonObject ?: return ""
        return when (block.string("type")) {
            "text" -> block.string("text").orEmpty()
            "resource_link" -> "[" + (nonEmpty(block.string("title")) ?: nonEmpty(block.string("name"))
                ?: nonEmpty(block.string("uri")) ?: "link") + "]"
            "resource" -> "[" + (nonEmpty((block["resource"] as? JsonObject)?.string("uri")) ?: "resource") + "]"
            "image" -> "[image]"
            "audio" -> "[audio]"
            else -> ""
        }
    }

    private fun nonEmpty(text: String?): String? = text?.takeIf { it.isNotEmpty() }
}
