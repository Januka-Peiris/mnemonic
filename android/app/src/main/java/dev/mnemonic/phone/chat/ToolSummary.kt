package dev.mnemonic.phone.chat

private fun plural(count: Int, singular: String, pluralForm: String) = if (count == 1) singular else pluralForm

fun toolRunSummary(tools: List<ChatItem.Tool>): String {
    tools.singleOrNull()?.title?.takeIf { it.isNotEmpty() }?.let { return it }
    val counts = tools.groupingBy { it.kind }.eachCount()
    val known = setOf("read", "edit", "execute", "search", "fetch")
    val parts = mutableListOf<String>()
    counts["read"]?.let { parts.add("read $it ${plural(it, "file", "files")}") }
    counts["edit"]?.let { parts.add("edited $it ${plural(it, "file", "files")}") }
    counts["execute"]?.let { parts.add("ran $it ${plural(it, "command", "commands")}") }
    counts["search"]?.let { parts.add("searched $it ${plural(it, "time", "times")}") }
    counts["fetch"]?.let { parts.add("fetched $it ${plural(it, "page", "pages")}") }
    val other = counts.filterKeys { it !in known }.values.sum()
    if (other > 0) parts.add("$other other ${plural(other, "step", "steps")}")
    return parts.joinToString(" · ")
}

fun toolRunFailed(tools: List<ChatItem.Tool>): Int = tools.count { it.status == "failed" }
