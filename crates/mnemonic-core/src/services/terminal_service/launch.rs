use std::path::{Path, PathBuf};

use crate::models::AgentProfile;
use crate::repositories::workspace_repository;
use crate::services::environment_service;
use crate::state::AppState;

#[derive(Clone)]
pub(super) struct TerminalProfile {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
}

#[derive(Clone)]
pub(super) struct TerminalCommandSpec {
    pub command: String,
    pub args: Vec<String>,
}

impl TerminalCommandSpec {
    pub fn from_input(
        profile: &TerminalProfile,
        command: Option<&str>,
        args: Option<Vec<String>>,
    ) -> Result<Self, String> {
        if let Some(command) = command.map(str::trim).filter(|command| !command.is_empty()) {
            return Ok(Self {
                command: "/bin/zsh".to_string(),
                args: vec!["-lc".to_string(), command.to_string()],
            });
        }
        Ok(Self {
            command: resolve_terminal_command(&profile.command),
            args: args.unwrap_or_else(|| profile.args.clone()),
        })
    }
}

pub(super) enum SessionResume {
    New(String),
    Resume(String),
}

impl TerminalProfile {
    pub fn from_agent_profile(
        profile: &AgentProfile,
        effective_model: Option<&str>,
        session_resume: Option<&SessionResume>,
        extra_args: &[String],
    ) -> Self {
        // Extra args are appended after profile args by the caller, so
        // consider both when deciding whether to add a default flag.
        fn flag_present(args: &[String], extra_args: &[String], flag: &str) -> bool {
            args.iter().any(|arg| arg == flag) || extra_args.iter().any(|arg| arg == flag)
        }

        let args = if profile.command.contains("claude") {
            let mut args = profile.args.clone();
            let has_flag = |args: &[String], flag: &str| flag_present(args, extra_args, flag);

            if !has_flag(&args, "--permission-mode")
                && !has_flag(&args, "--dangerously-skip-permissions")
            {
                args.push("--permission-mode".to_string());
                args.push("bypassPermissions".to_string());
            }

            match session_resume {
                Some(SessionResume::Resume(id)) => {
                    args.push("--resume".to_string());
                    args.push(id.clone());
                }
                Some(SessionResume::New(id)) => {
                    args.push("--session-id".to_string());
                    args.push(id.clone());
                }
                None => {
                    args.push("--continue".to_string());
                }
            }

            if let Some(model) = effective_model.filter(|model| !model.is_empty()) {
                if !has_flag(&args, "--model") && !has_flag(&args, "-m") {
                    args.push("--model".to_string());
                    args.push(model.to_string());
                }
            }
            args
        } else {
            profile.args.clone()
        };
        Self {
            name: profile.agent.clone(),
            command: profile.command.clone(),
            args,
        }
    }
}

pub(super) fn workspace_root_path(state: &AppState, workspace_id: &str) -> Result<PathBuf, String> {
    let workspace = workspace_repository::get_detail(&state.db, workspace_id)?
        .ok_or_else(|| format!("Workspace {workspace_id} was not found"))?;
    let cwd = workspace
        .summary
        .workspace_root_path
        .clone()
        .unwrap_or_else(|| workspace.worktree_path.clone());
    let path = PathBuf::from(cwd);
    if !path.exists() {
        return Err(format!(
            "Workspace root path does not exist: {}",
            path.display()
        ));
    }
    if !path.is_dir() {
        return Err(format!(
            "Workspace root path is not a directory: {}",
            path.display()
        ));
    }
    if !is_git_worktree(&path) {
        return Err(format!(
            "Workspace root path is not a Git worktree: {}",
            path.display()
        ));
    }
    Ok(path)
}

pub(super) fn resolve_session_role(explicit: Option<&str>, profile: &str) -> String {
    match explicit.unwrap_or("").trim() {
        "agent" => "agent".to_string(),
        "utility" => "utility".to_string(),
        _ => {
            if profile == "shell" {
                "utility".to_string()
            } else {
                "agent".to_string()
            }
        }
    }
}

pub(super) fn normalize_terminal_kind(kind: &str, profile: &str) -> String {
    match kind {
        "agent" | "shell" | "run" | "utility" => kind.to_string(),
        _ if profile == "shell" => "shell".to_string(),
        _ => "agent".to_string(),
    }
}

pub(super) fn default_terminal_title(kind: &str, profile: &str) -> String {
    match (kind, profile) {
        ("shell", _) | ("utility", _) => "Shell".to_string(),
        (_, "claude_code") => "Claude".to_string(),
        (_, "codex") => "Codex".to_string(),
        ("run", _) => "Run".to_string(),
        _ => profile.to_string(),
    }
}

fn is_git_worktree(path: &Path) -> bool {
    std::process::Command::new("git")
        .arg("rev-parse")
        .arg("--is-inside-work-tree")
        .current_dir(path)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn resolve_terminal_command(command: &str) -> String {
    let trimmed = command.trim();
    if trimmed.is_empty() || trimmed.contains('/') {
        return command.to_string();
    }
    environment_service::find_binary(trimmed)
        .ok()
        .flatten()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| command.to_string())
}
