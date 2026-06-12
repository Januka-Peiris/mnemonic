use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::models::{
    AgentDecisionEvent, AgentDecisionOption, AgentDecisionResolvedEvent, AgentModeChangedEvent,
    TerminalSession,
};
use crate::repositories::terminal_repository;
use crate::state::{ActiveTerminal, AppState};

use super::runtime::active_for_session;
use super::tmux;

/// How long PTY output must stay quiet before the screen is inspected for a
/// new decision dialog (mirrors READY_IDLE_MILLIS in queue.rs).
const INSPECT_IDLE_MILLIS: u64 = 700;
/// Watcher tick interval.
const POLL_INTERVAL_MILLIS: u64 = 400;
/// Cap on how many recent output chunks the non-tmux fallback inspects.
const FALLBACK_CHUNK_WINDOW: u64 = 80;
/// Visible lines the non-tmux fallback keeps from the tail of the stream.
const FALLBACK_LINE_WINDOW: usize = 50;

/// Watches an agent TUI for numbered decision dialogs (plan approval,
/// permission prompts, AskUserQuestion) and mirrors them to the chat UI via
/// `mn://agent-decision-required` / `mn://agent-decision-resolved` events.
/// Also tracks the permission mode shown in the TUI footer and mirrors it via
/// `mn://agent-mode-changed`, so the composer's mode button stays truthful.
pub(super) fn spawn_decision_watcher(
    state: AppState,
    session: TerminalSession,
    active: Arc<ActiveTerminal>,
) {
    thread::spawn(move || watch_session(state, session, active));
}

fn watch_session(state: AppState, session: TerminalSession, active: Arc<ActiveTerminal>) {
    let mut last_mode_seq = u64::MAX;
    let mut last_dialog_seq = u64::MAX;
    let mut last_mode: Option<&'static str> = None;
    let mut pending_fingerprint: Option<u64> = None;
    loop {
        thread::sleep(Duration::from_millis(POLL_INTERVAL_MILLIS));

        // Exit when this attach was torn down or replaced by a reattach (the
        // new attach spawns its own watcher).
        let still_current = active_for_session(&state, &session.id)
            .ok()
            .flatten()
            .map(|current| Arc::ptr_eq(&current, &active))
            .unwrap_or(false);
        if !still_current {
            if pending_fingerprint.is_some() {
                emit_resolved(&state, &session);
            }
            return;
        }

        let last_output = active.last_output_at_millis.load(Ordering::Relaxed);
        if last_output == 0 {
            continue;
        }
        let seq = active.seq_counter.load(Ordering::Relaxed);
        // A fresh dialog is only looked for once output has settled; a dialog
        // already surfaced in chat is re-checked on any new output so the card
        // clears promptly after it is answered (in chat or in the terminal).
        // Mode changes redraw the footer immediately, so they skip the idle gate.
        let idle = now_millis().saturating_sub(last_output) >= INSPECT_IDLE_MILLIS;
        let mode_due = seq != last_mode_seq;
        let dialog_due = seq != last_dialog_seq && (pending_fingerprint.is_some() || idle);
        if !mode_due && !dialog_due {
            continue;
        }

        let Some(screen) = capture_screen(&state, &session) else {
            continue;
        };

        if mode_due {
            last_mode_seq = seq;
            if let Some(mode) = parse_agent_mode(&screen) {
                if last_mode != Some(mode) {
                    last_mode = Some(mode);
                    state.event_emitter.emit(
                        crate::events::AGENT_MODE_CHANGED,
                        &serde_json::json!(AgentModeChangedEvent {
                            workspace_id: session.workspace_id.clone(),
                            session_id: session.id.clone(),
                            mode: mode.to_string(),
                        }),
                    );
                }
            }
        }

        if !dialog_due {
            continue;
        }
        last_dialog_seq = seq;

        match parse_decision_dialog(&screen) {
            Some(dialog) => {
                let fingerprint = dialog.fingerprint();
                if pending_fingerprint != Some(fingerprint) {
                    pending_fingerprint = Some(fingerprint);
                    state.event_emitter.emit(
                        crate::events::AGENT_DECISION_REQUIRED,
                        &serde_json::json!(AgentDecisionEvent {
                            workspace_id: session.workspace_id.clone(),
                            session_id: session.id.clone(),
                            question: dialog.question,
                            options: dialog.options,
                        }),
                    );
                }
            }
            None => {
                if pending_fingerprint.take().is_some() {
                    emit_resolved(&state, &session);
                }
            }
        }
    }
}

/// Lines from the bottom of the screen inspected for the mode footer.
const MODE_FOOTER_LINE_WINDOW: usize = 6;

/// Permission mode shown in the agent TUI footer. None when the footer is not
/// visible (mid-redraw, alt screens), so callers keep the last known mode.
fn parse_agent_mode(screen: &str) -> Option<&'static str> {
    let lines: Vec<String> = screen
        .lines()
        .map(clean_line)
        .filter(|line| !line.is_empty())
        .collect();
    let start = lines.len().saturating_sub(MODE_FOOTER_LINE_WINDOW);
    let mut composer_visible = false;
    for line in &lines[start..] {
        let lowered = line.to_lowercase();
        if lowered.contains("plan mode on") {
            return Some("plan");
        }
        if lowered.contains("accept edits on") {
            return Some("acceptEdits");
        }
        if lowered.contains("bypass permissions on") {
            return Some("bypassPermissions");
        }
        if line.starts_with('>') || lowered.contains("? for shortcuts") {
            composer_visible = true;
        }
    }
    // No mode marker but the composer is on screen: the TUI is in default mode.
    composer_visible.then_some("default")
}

fn emit_resolved(state: &AppState, session: &TerminalSession) {
    state.event_emitter.emit(
        crate::events::AGENT_DECISION_RESOLVED,
        &serde_json::json!(AgentDecisionResolvedEvent {
            workspace_id: session.workspace_id.clone(),
            session_id: session.id.clone(),
        }),
    );
}

fn capture_screen(state: &AppState, session: &TerminalSession) -> Option<String> {
    if session.backend == "tmux" {
        let name = session.tmux_session_name.as_deref()?;
        return tmux::capture_pane(name).ok();
    }
    recent_screen_from_chunks(state, &session.id)
}

/// Best-effort screen reconstruction for non-tmux sessions: ANSI-stripped
/// (line structure preserved) tail of the recent output stream. TUI redraws
/// make this less reliable than capture-pane, so it errs toward not matching.
fn recent_screen_from_chunks(state: &AppState, session_id: &str) -> Option<String> {
    let next_seq = terminal_repository::next_seq(&state.db, session_id).unwrap_or(0);
    let since = next_seq.saturating_sub(FALLBACK_CHUNK_WINDOW);
    let chunks = terminal_repository::list_output_chunks(&state.db, session_id, since).ok()?;
    let raw: String = chunks.iter().map(|chunk| chunk.data.as_str()).collect();
    let stripped = strip_ansi_keep_lines(&raw);
    let lines: Vec<&str> = stripped.lines().collect();
    let start = lines.len().saturating_sub(FALLBACK_LINE_WINDOW);
    Some(lines[start..].join("\n"))
}

/// Strips ANSI escape sequences (CSI, OSC, lone ESC) while preserving line
/// breaks, unlike queue.rs's normalize_terminal_text which collapses all
/// whitespace for substring matching.
fn strip_ansi_keep_lines(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\u{1b}' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    let mut prev = ' ';
                    for next in chars.by_ref() {
                        if next == '\u{7}' || (prev == '\u{1b}' && next == '\\') {
                            break;
                        }
                        prev = next;
                    }
                }
                _ => {
                    chars.next();
                }
            },
            '\r' => {}
            _ => out.push(ch),
        }
    }
    out
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, PartialEq, Eq)]
struct DecisionDialog {
    question: String,
    options: Vec<AgentDecisionOption>,
}

impl DecisionDialog {
    fn fingerprint(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.question.hash(&mut hasher);
        for option in &self.options {
            option.key.hash(&mut hasher);
            option.label.hash(&mut hasher);
        }
        hasher.finish()
    }
}

/// Maximum non-empty lines tolerated below the option block (dialog footers
/// like "ctrl+g to edit in VS Code" or "esc to cancel").
const MAX_TRAILER_LINES: usize = 4;
/// Maximum lines of wrapped option text between two numbered options.
const MAX_OPTION_GAP_LINES: usize = 2;
/// Cap on the extracted question text.
const MAX_QUESTION_CHARS: usize = 300;

/// Parses the rendered screen for a trailing numbered decision dialog.
///
/// Shape: a contiguous block of options numbered 1..N (allowing short wrapped
/// continuation lines), preceded by question text, with nothing but footer
/// hints below it. An idle TUI showing its composer input box ("> ") below a
/// numbered list is rejected, so ordinary numbered lists in agent responses
/// do not produce false positives.
fn parse_decision_dialog(screen: &str) -> Option<DecisionDialog> {
    let lines: Vec<String> = screen.lines().map(clean_line).collect();
    let mut end = lines.len();
    while end > 0 && lines[end - 1].is_empty() {
        end -= 1;
    }
    let lines = &lines[..end];
    if lines.is_empty() {
        return None;
    }

    // All numbered option candidates, in screen order.
    let candidates: Vec<(usize, u32, String)> = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            parse_option_line(line).map(|(number, label)| (index, number, label))
        })
        .collect();

    // The dialog's block starts at the last "1." on screen.
    let start = candidates.iter().rposition(|(_, number, _)| *number == 1)?;
    let mut options: Vec<AgentDecisionOption> = Vec::new();
    let mut last_line_index = 0;
    for (expected, (line_index, number, label)) in (1_u32..).zip(candidates[start..].iter()) {
        if *number != expected {
            break;
        }
        if expected > 1 && line_index - last_line_index > MAX_OPTION_GAP_LINES + 1 {
            break;
        }
        options.push(AgentDecisionOption {
            key: number.to_string(),
            label: label.clone(),
        });
        last_line_index = *line_index;
    }
    if options.len() < 2 {
        return None;
    }

    // Reject when the bottom of the screen is a composer rather than a dialog.
    let mut trailers = 0;
    for line in &lines[last_line_index + 1..] {
        if line.is_empty() {
            continue;
        }
        if line.starts_with('>') || line.contains("? for shortcuts") {
            return None;
        }
        trailers += 1;
        if trailers > MAX_TRAILER_LINES {
            return None;
        }
    }

    // Question: the contiguous non-empty block immediately above the options.
    let first_option_line = candidates[start].0;
    let mut question_lines: Vec<&str> = Vec::new();
    let mut index = first_option_line;
    while index > 0 && lines[index - 1].is_empty() {
        index -= 1;
    }
    while index > 0 && !lines[index - 1].is_empty() {
        question_lines.push(lines[index - 1].as_str());
        index -= 1;
    }
    question_lines.reverse();
    let mut question = question_lines.join(" ").trim().to_string();
    if question.is_empty() {
        return None;
    }
    if question.len() > MAX_QUESTION_CHARS {
        let mut cut = MAX_QUESTION_CHARS;
        while cut > 0 && !question.is_char_boundary(cut) {
            cut -= 1;
        }
        question.truncate(cut);
        question.push('…');
    }

    // Dialogs the app already answers on its own stay out of chat.
    let lowered = question.to_lowercase();
    if lowered.contains("trust this folder") {
        return None;
    }

    Some(DecisionDialog { question, options })
}

/// Strips box-drawing borders and the selection caret from a rendered line.
/// Underscores are kept (labels may contain them); a leading caret rendered as
/// '_' by a non-UTF-8 tmux server is handled by parse_option_line instead.
fn clean_line(line: &str) -> String {
    line.chars()
        .map(|ch| match ch {
            '\u{2500}'..='\u{257F}' | '\u{2580}'..='\u{259F}' | '❯' => ' ',
            _ => ch,
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Matches "1. Label", optionally preceded by a selection caret (`❯`, already
/// stripped) or its '_' stand-in on screens rendered before the UTF-8 fix.
fn parse_option_line(line: &str) -> Option<(u32, String)> {
    let rest = line.trim_start_matches(|c: char| c == '_' || c.is_whitespace());
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || digits.len() > 2 {
        return None;
    }
    let after = rest[digits.len()..].strip_prefix('.')?;
    let label = after.trim();
    if label.is_empty() {
        return None;
    }
    let number: u32 = digits.parse().ok()?;
    if number == 0 {
        return None;
    }
    Some((number, label.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(dialog: &DecisionDialog) -> Vec<&str> {
        dialog
            .options
            .iter()
            .map(|option| option.label.as_str())
            .collect()
    }

    #[test]
    fn parses_plan_approval_dialog() {
        let screen = "\
⏺ I dug into the latest changes and wrote up a plan.

 Claude has written up a plan and is ready to execute. Would you like to proceed?

 ❯ 1. Yes, and use auto mode
   2. Yes, manually approve edits
   3. No, refine with Ultraplan on Claude Code on the web
   4. Tell Claude what to change
      shift+tab to approve with this feedback

 ctrl+g to edit in  VS Code  · ~/.claude/plans/can-you-make-the-graceful-marshmallow.md
";
        let dialog = parse_decision_dialog(screen).expect("plan dialog should parse");
        assert!(dialog.question.ends_with("Would you like to proceed?"));
        assert_eq!(
            labels(&dialog),
            vec![
                "Yes, and use auto mode",
                "Yes, manually approve edits",
                "No, refine with Ultraplan on Claude Code on the web",
                "Tell Claude what to change",
            ]
        );
    }

    #[test]
    fn parses_permission_prompt_inside_box() {
        let screen = "\
╭──────────────────────────────────────────────╮
│ Bash command                                 │
│                                              │
│   rm -rf node_modules                        │
│   Remove node modules directory              │
│                                              │
│ Do you want to proceed?                      │
│ ❯ 1. Yes                                     │
│   2. Yes, and don't ask again this session   │
│   3. No, and tell Claude what to do          │
╰──────────────────────────────────────────────╯
";
        let dialog = parse_decision_dialog(screen).expect("permission dialog should parse");
        assert_eq!(dialog.question, "Do you want to proceed?");
        assert_eq!(dialog.options.len(), 3);
        assert_eq!(dialog.options[0].key, "1");
    }

    #[test]
    fn parses_dialog_rendered_without_utf8_glyphs() {
        // Pre-fix tmux servers replace ❯ and box borders with '_'.
        let screen = "\
 Claude has written up a plan and is ready to execute. Would you like to proceed?

 _ 1. Yes, and use auto mode
   2. Yes, manually approve edits
   3. Tell Claude what to change
";
        let dialog = parse_decision_dialog(screen).expect("underscore dialog should parse");
        assert_eq!(dialog.options.len(), 3);
        assert_eq!(dialog.options[0].label, "Yes, and use auto mode");
    }

    #[test]
    fn rejects_numbered_list_above_idle_composer() {
        let screen = "\
⏺ Here are the steps:
  1. Install the dependencies
  2. Run the build

╭──────────────────────────────────────────────╮
│ >                                            │
╰──────────────────────────────────────────────╯
  ? for shortcuts
";
        assert!(parse_decision_dialog(screen).is_none());
    }

    #[test]
    fn rejects_numbered_list_followed_by_more_output() {
        let screen = "\
⏺ Two options:
  1. Use the cache
  2. Recompute each time

  I recommend the cache because it is faster.
  It also reduces network load.
  Let me know which you prefer before I continue.
  Some more explanation here.
  And a final line of detail.
";
        assert!(parse_decision_dialog(screen).is_none());
    }

    #[test]
    fn skips_trust_dialog() {
        let screen = "\
 Quick safety check: do you trust this folder?

 ❯ 1. Yes, I trust this folder
   2. No, exit
";
        assert!(parse_decision_dialog(screen).is_none());
    }

    #[test]
    fn requires_question_text() {
        let screen = "\n 1. Yes\n 2. No\n";
        assert!(parse_decision_dialog(screen).is_none());
    }

    #[test]
    fn parses_plan_mode_footer() {
        let screen = "\
⏺ Done.

╭──────────────────────────────────────────────╮
│ >                                            │
╰──────────────────────────────────────────────╯
  ⏸ plan mode on (shift+tab to cycle) · PR #6 · ← for agents
";
        assert_eq!(parse_agent_mode(screen), Some("plan"));
    }

    #[test]
    fn parses_accept_edits_footer() {
        let screen = "\
╭──────────────────────────────────────────────╮
│ >                                            │
╰──────────────────────────────────────────────╯
  ⏵⏵ accept edits on (shift+tab to cycle)
";
        assert_eq!(parse_agent_mode(screen), Some("acceptEdits"));
    }

    #[test]
    fn parses_bypass_permissions_footer() {
        let screen = "\
╭──────────────────────────────────────────────╮
│ >                                            │
╰──────────────────────────────────────────────╯
  bypass permissions on (shift+tab to cycle)
";
        assert_eq!(parse_agent_mode(screen), Some("bypassPermissions"));
    }

    #[test]
    fn default_mode_when_composer_visible_without_marker() {
        let screen = "\
⏺ All tests pass.

╭──────────────────────────────────────────────╮
│ >                                            │
╰──────────────────────────────────────────────╯
  ? for shortcuts
";
        assert_eq!(parse_agent_mode(screen), Some("default"));
    }

    #[test]
    fn no_mode_when_footer_not_visible() {
        let screen = "\
Compiling crate foo v0.1.0
Compiling crate bar v0.2.0
Building [=========>          ] 42/100
";
        assert_eq!(parse_agent_mode(screen), None);
    }

    #[test]
    fn strip_ansi_keeps_line_structure() {
        let raw = "\u{1b}[2K\u{1b}[1mDo you want to proceed?\u{1b}[0m\r\n\u{1b}[36m❯ 1. Yes\u{1b}[39m\r\n  2. No\r\n";
        let stripped = strip_ansi_keep_lines(raw);
        let lines: Vec<&str> = stripped.lines().collect();
        assert_eq!(
            lines,
            vec!["Do you want to proceed?", "❯ 1. Yes", "  2. No"]
        );
    }
}
