use std::path::Path;
use std::process::Command;

use super::{enriched_path, TerminalCommandSpec};

/// Dedicated tmux server socket so agent sessions are isolated from the
/// user's own tmux: their config (status bar, prefix key, mouse mode) does
/// not leak into embedded terminals and `tmux kill-server` on their personal
/// server cannot kill persistent agent sessions.
const SOCKET_NAME: &str = "mnemonic";

fn tmux_command() -> Command {
    let mut command = Command::new("tmux");
    // -u forces UTF-8: the GUI app has no LANG/LC_* in its environment, and
    // without it tmux assumes the client cannot display UTF-8 and replaces
    // every non-ASCII glyph with '_'. LANG covers the server side so programs
    // inside panes also see a UTF-8 locale.
    command.args(["-u", "-L", SOCKET_NAME]);
    command.env("PATH", enriched_path());
    command.env("LANG", "en_US.UTF-8");
    command
}

pub(super) fn available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .env("PATH", enriched_path())
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

pub(super) fn safe_session_name(workspace_id: &str, session_id: &str) -> String {
    let raw = format!("mnemonic-{workspace_id}-{session_id}");
    let mut name = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    name.truncate(180);
    name
}

pub(super) fn session_exists(name: &str) -> bool {
    tmux_command()
        .args(["has-session", "-t", name])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

pub(super) fn start_detached(
    name: &str,
    cwd: &Path,
    command_spec: &TerminalCommandSpec,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    // Embed PATH in the command itself: if the private server already exists
    // it spawns sessions with its own (possibly stale) environment, and an
    // npm-installed claude needs `node` from its own bin directory.
    let shell_command = format!(
        "env PATH={} {}",
        shell_quote(&super::path_for_command(&command_spec.command)),
        shell_command(command_spec)
    );
    // -f /dev/null skips the user's tmux.conf if this command ends up
    // starting the private server.
    let mut command = tmux_command();
    command
        .args(["-f", "/dev/null", "new-session", "-d", "-s", name])
        .args(["-x", &cols.to_string(), "-y", &rows.to_string()])
        .arg("-c")
        .arg(cwd)
        .arg(shell_command)
        .env("TERM", "xterm-256color");
    let output = command
        .output()
        .map_err(|err| format!("Failed to start tmux: {err}"))?;
    if output.status.success() {
        configure_session(name);
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if stderr.is_empty() {
            format!("tmux failed to start session {name}")
        } else {
            format!("tmux failed to start session {name}: {stderr}")
        })
    }
}

/// Best-effort session options: no status bar inside the embedded xterm pane
/// and no escape-key delay (TUIs read lone ESC presses).
fn configure_session(name: &str) {
    let _ = tmux_command()
        .args(["set-option", "-t", name, "status", "off"])
        .output();
    let _ = tmux_command()
        .args(["set-option", "-g", "escape-time", "0"])
        .output();
    let _ = tmux_command()
        .args(["set-option", "-g", "history-limit", "10000"])
        .output();
}

/// Returns the currently rendered pane contents as plain text lines. Far more
/// reliable than parsing the raw escape-code stream when inspecting what a
/// TUI is showing right now.
pub(super) fn capture_pane(name: &str) -> Result<String, String> {
    let output = tmux_command()
        .args(["capture-pane", "-p", "-t", name])
        .output()
        .map_err(|err| format!("Failed to capture tmux pane: {err}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if stderr.is_empty() {
            format!("tmux failed to capture pane for session {name}")
        } else {
            format!("tmux failed to capture pane for session {name}: {stderr}")
        })
    }
}

pub(super) fn kill_session(name: &str) -> Result<(), String> {
    let output = tmux_command()
        .args(["kill-session", "-t", name])
        .output()
        .map_err(|err| format!("Failed to stop tmux session: {err}"))?;
    if output.status.success() || !session_exists(name) {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if stderr.is_empty() {
            format!("tmux failed to stop session {name}")
        } else {
            format!("tmux failed to stop session {name}: {stderr}")
        })
    }
}

pub(super) fn attach_command(name: &str) -> (String, Vec<String>) {
    (
        "tmux".to_string(),
        vec![
            // -u forces UTF-8 output; see tmux_command().
            "-u".to_string(),
            "-L".to_string(),
            SOCKET_NAME.to_string(),
            "attach-session".to_string(),
            "-t".to_string(),
            name.to_string(),
        ],
    )
}

fn shell_command(command_spec: &TerminalCommandSpec) -> String {
    std::iter::once(command_spec.command.as_str())
        .chain(command_spec.args.iter().map(String::as_str))
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', r#"'\''"#))
}
