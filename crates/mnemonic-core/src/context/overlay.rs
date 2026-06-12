use crate::context::schema::{OverlayFile, RenamePair, WorkspaceOverlay};
use crate::services::git_review_service;
use crate::state::AppState;

pub fn build_workspace_overlay(state: &AppState, workspace_id: &str) -> WorkspaceOverlay {
    let files =
        git_review_service::get_workspace_changed_files(state, workspace_id).unwrap_or_default();

    let mut changed = Vec::new();
    let mut new_files = Vec::new();
    let mut deleted = Vec::new();
    let mut renamed = Vec::new();

    for file in &files {
        match file.status.as_str() {
            "deleted" | "D" => {
                deleted.push(file.path.clone());
            }
            status if status.starts_with('R') || status == "renamed" => {
                if let Some(old_path) = &file.old_path {
                    renamed.push(RenamePair {
                        old: old_path.clone(),
                        new: file.path.clone(),
                    });
                }
                // Also add new path as a changed file
                let diff =
                    git_review_service::get_workspace_file_diff(state, workspace_id, &file.path)
                        .map(|d| d.diff)
                        .unwrap_or_default();
                changed.push(OverlayFile {
                    path: file.path.clone(),
                    diff,
                    additions: file.additions.unwrap_or(0),
                    deletions: file.deletions.unwrap_or(0),
                });
            }
            "added" | "A" | "untracked" => {
                let diff =
                    git_review_service::get_workspace_file_diff(state, workspace_id, &file.path)
                        .map(|d| d.diff)
                        .unwrap_or_default();
                new_files.push(OverlayFile {
                    path: file.path.clone(),
                    diff,
                    additions: file.additions.unwrap_or(0),
                    deletions: file.deletions.unwrap_or(0),
                });
            }
            _ => {
                // modified or anything else
                let diff =
                    git_review_service::get_workspace_file_diff(state, workspace_id, &file.path)
                        .map(|d| d.diff)
                        .unwrap_or_default();
                changed.push(OverlayFile {
                    path: file.path.clone(),
                    diff,
                    additions: file.additions.unwrap_or(0),
                    deletions: file.deletions.unwrap_or(0),
                });
            }
        }
    }

    WorkspaceOverlay {
        changed,
        new_files,
        deleted,
        renamed,
    }
}
