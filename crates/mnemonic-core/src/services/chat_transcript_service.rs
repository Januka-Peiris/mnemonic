//! Chats saved as markdown files, so a linked workspace can open one or
//! hand it to its agent. Mnemonic itself only stores which chats exist; the
//! conversation lives in the running agent, so it is written out here each
//! time a turn ends.

use std::fs;
use std::path::{Path, PathBuf};

use crate::repositories::chat_session_repository;
use crate::services::workspace_service;
use crate::state::AppState;

/// A chat in a workspace linked to the current one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedChat {
    pub workspace_id: String,
    pub workspace_name: String,
    pub chat_id: String,
    pub title: String,
    /// None until the chat has been saved at least once.
    pub transcript: Option<PathBuf>,
}

/// Each chat gets its own folder so the file inside can carry the chat's
/// title, which is what people and agents see when it is attached.
fn chat_dir(state: &AppState, workspace_id: &str, chat_id: &str) -> PathBuf {
    state
        .data_dir
        .join("chat-transcripts")
        .join(workspace_id)
        .join(chat_id)
}

fn saved_transcript(dir: &Path) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|extension| extension == "md"))
}

/// Saves a chat's transcript, replacing the previous save, and returns
/// where it is.
pub fn save_transcript(
    state: &AppState,
    workspace_id: &str,
    chat_id: &str,
    title: &str,
    workspace_name: &str,
    markdown: &str,
) -> Result<PathBuf, String> {
    let dir = chat_dir(state, workspace_id, chat_id);
    fs::create_dir_all(&dir).map_err(|err| format!("Couldn't create {}: {err}", dir.display()))?;
    let slug = workspace_service::branch_slug(title);
    let file_name = if slug.is_empty() {
        "chat.md".to_string()
    } else {
        format!("{slug}.md")
    };
    let path = dir.join(file_name);
    let contents =
        format!("# {title}\n\nA chat from the workspace \"{workspace_name}\".\n\n{markdown}");
    // Written beside the target and renamed over it, so a reader never
    // sees half a file.
    let partial = dir.join(".partial");
    fs::write(&partial, contents)
        .and_then(|()| fs::rename(&partial, &path))
        .map_err(|err| format!("Couldn't save {}: {err}", path.display()))?;
    // A retitled chat is saved under a new name; the old file goes.
    for entry in fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
    {
        let other = entry.path();
        if other != path && other.extension().is_some_and(|extension| extension == "md") {
            if let Err(err) = fs::remove_file(&other) {
                log::warn!(target: "mnemonic_lib", "Couldn't remove {}: {err}", other.display());
            }
        }
    }
    Ok(path)
}

/// The chats of every workspace linked to this one.
pub fn linked_chats(state: &AppState, workspace_id: &str) -> Result<Vec<LinkedChat>, String> {
    let mut chats = Vec::new();
    for workspace in workspace_service::linked_workspaces(state, workspace_id)? {
        for record in chat_session_repository::list_for_workspace(&state.db, &workspace.id)? {
            chats.push(LinkedChat {
                transcript: saved_transcript(&chat_dir(state, &workspace.id, &record.id)),
                workspace_id: workspace.id.clone(),
                workspace_name: workspace.name.clone(),
                chat_id: record.id,
                title: record.title,
            });
        }
    }
    Ok(chats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_retitled_chat_keeps_one_transcript_named_after_its_title() -> Result<(), String> {
        let root = std::env::temp_dir().join(format!(
            "mnemonic-transcript-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        ));
        let state = AppState::initialize(
            root.join("data"),
            root.join("cache"),
            std::sync::Arc::new(crate::state::NoopEventEmitter),
        )?;

        save_transcript(
            &state,
            "ws-1",
            "chat-1",
            "Claude",
            "crisp-panda",
            "## User\n\nhi",
        )?;
        let saved = save_transcript(
            &state,
            "ws-1",
            "chat-1",
            "Fix the login page",
            "crisp-panda",
            "## User\n\nfix it",
        )?;

        let dir = chat_dir(&state, "ws-1", "chat-1");
        let files: Vec<PathBuf> = fs::read_dir(&dir)
            .map_err(|err| err.to_string())?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        let contents = fs::read_to_string(&saved).map_err(|err| err.to_string())?;
        fs::remove_dir_all(&root).map_err(|err| err.to_string())?;

        assert_eq!(files, vec![saved.clone()]);
        assert_eq!(
            saved.file_name().and_then(|name| name.to_str()),
            Some("fix-the-login-page.md")
        );
        assert!(
            contents
                .starts_with("# Fix the login page\n\nA chat from the workspace \"crisp-panda\".")
        );
        assert!(contents.ends_with("fix it"));
        Ok(())
    }
}
