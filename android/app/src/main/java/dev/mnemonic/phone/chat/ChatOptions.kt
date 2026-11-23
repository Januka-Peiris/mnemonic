package dev.mnemonic.phone.chat

import androidx.compose.runtime.Composable
import dev.mnemonic.phone.api.PhoneChatOption
import dev.mnemonic.phone.api.PhoneOptionChoice
import dev.mnemonic.phone.ui.DropdownChip

/** Model first: the efforts an agent offers can depend on the model. */
fun orderedChoices(options: List<PhoneChatOption>, chosen: Map<String, String>): List<PhoneOptionChoice> =
    options.sortedBy { if (it.kind == "model") 0 else 1 }
        .mapNotNull { option -> chosen[option.id]?.let { PhoneOptionChoice(option.id, it) } }

fun optionLabel(option: PhoneChatOption, chosen: String?): String {
    val id = chosen ?: option.current
    return option.values.firstOrNull { it.id == id }?.name ?: "${option.name}: default"
}

@Composable
fun OptionChips(
    options: List<PhoneChatOption>,
    chosen: Map<String, String>,
    enabled: Boolean = true,
    onSelect: (PhoneChatOption, String) -> Unit,
) {
    for (option in options) {
        DropdownChip(
            label = optionLabel(option, chosen[option.id]),
            options = option.values.map { it.id to it.name },
            selectedId = chosen[option.id] ?: option.current,
            enabled = enabled,
        ) { onSelect(option, it) }
    }
}
