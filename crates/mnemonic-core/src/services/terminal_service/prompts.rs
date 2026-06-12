use crate::models::TerminalSession;

/// How a prompt should be delivered to the PTY.
pub(super) enum PromptPayload {
    /// Write the whole payload in one chunk (line-based REPLs like ollama).
    Single(String),
    /// Write the text first (wrapped in bracketed-paste markers), then send
    /// Enter as a separate write after a short delay. TUI inputs (Claude Code,
    /// Codex) treat a CR arriving in the same chunk as pasted text rather than
    /// a submit, and raw newlines mid-chunk can submit partial prompts.
    PasteThenEnter(String),
}

pub(super) fn terminal_prompt_payload_for_session(
    session: &TerminalSession,
    prompt: &str,
) -> PromptPayload {
    if is_ollama_terminal_session(session) {
        if prompt.contains('\n') {
            return PromptPayload::Single(format!(
                "\"\"\"\n{}\n\"\"\"\r\n",
                escape_ollama_multiline_prompt(prompt)
            ));
        }
        return PromptPayload::Single(format!("{prompt}\r\n"));
    }
    PromptPayload::PasteThenEnter(format!("\x1b[200~{prompt}\x1b[201~"))
}

fn is_ollama_terminal_session(session: &TerminalSession) -> bool {
    let command = session.command.to_ascii_lowercase();
    command.ends_with("/ollama") || command == "ollama" || command.contains("ollama")
}

fn escape_ollama_multiline_prompt(prompt: &str) -> String {
    prompt.replace("\"\"\"", "\\\"\\\"\\\"")
}
