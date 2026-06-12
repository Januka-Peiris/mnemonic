use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::models::{DiscoveredRepository, ScanRepositoriesResult};
use crate::repositories::{repository_repository, settings_repository};
use crate::services::{repo_intelligence_service, worktree_discovery_service};
use crate::state::AppState;

const MAX_SCAN_DEPTH: usize = 5;

/// Resolves a directory (or subfolder of a checkout) to the Git worktree toplevel path.
/// Used when adding a single repository root from a folder picker.
pub fn resolve_git_repository_path(path: &str) -> Result<String, String> {
    let path = path.trim();
    if path.is_empty() {
        return Err("Path is empty".to_string());
    }
    let p = Path::new(path);
    if !p.exists() {
        return Err(format!("Path does not exist: {path}"));
    }
    if !p.is_dir() {
        return Err("Path must be a directory".to_string());
    }

    let top_level = git(p, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .map_err(|err| format!("Not a Git repository (or git failed): {err}"))?;
    let canonical = canonicalize(&top_level);
    canonical
        .to_str()
        .map(str::to_string)
        .ok_or_else(|| "Path is not valid UTF-8".to_string())
}

pub fn scan_repositories(state: &AppState) -> Result<ScanRepositoriesResult, String> {
    let repo_roots = settings_repository::get_repo_roots(&state.db)?;
    let scanned_at = unix_timestamp_string();
    let managed_workspaces_root = settings_repository::get_managed_workspaces_root(&state.db)
        .ok()
        .map(PathBuf::from);
    let mut warnings = Vec::new();
    let mut repo_paths = BTreeSet::new();

    for root in &repo_roots {
        let root_path = PathBuf::from(root);
        if !root_path.exists() {
            warnings.push(format!("Repo root does not exist: {root}"));
            continue;
        }

        collect_git_repositories(&root_path, 0, &mut repo_paths, &mut warnings);
    }

    let mut repositories = Vec::new();
    for path in repo_paths {
        match build_repository(&path, &scanned_at) {
            Ok(mut repo) => {
                let (worktrees, mut worktree_warnings) =
                    worktree_discovery_service::discover_worktrees(
                        &repo.id,
                        &path,
                        managed_workspaces_root.as_deref(),
                    );
                repo.worktrees = worktrees;
                warnings.append(&mut worktree_warnings);
                repositories.push(repo);
            }
            Err(err) => warnings.push(err),
        }
    }

    repository_repository::replace_all(&state.db, &repositories)?;

    if repo_intelligence_service::repo_intelligence_enabled(state) {
        for repository in &repositories {
            let _ = repo_intelligence_service::refresh_repository_intelligence(
                state,
                &repository.id,
                &repository.path,
                false,
            );
        }
    }

    Ok(ScanRepositoriesResult {
        repo_roots,
        repositories,
        scanned_at,
        warnings,
    })
}

pub fn remove_repository(state: &AppState, repository_id: &str) -> Result<(), String> {
    repository_repository::remove(&state.db, repository_id)
}

pub fn refresh_repository_by_id(state: &AppState, repository_id: &str) -> Result<(), String> {
    let existing = repository_repository::get(&state.db, repository_id)?
        .ok_or_else(|| format!("Repository {repository_id} was not found"))?;
    let scanned_at = unix_timestamp_string();
    let managed_workspaces_root = settings_repository::get_managed_workspaces_root(&state.db)
        .ok()
        .map(PathBuf::from);
    let mut repository = build_repository(Path::new(&existing.path), &scanned_at)?;
    // Preserve the stable id from the persisted discovered repository.
    repository.id = existing.id;
    let (worktrees, _warnings) = worktree_discovery_service::discover_worktrees(
        &repository.id,
        Path::new(&repository.path),
        managed_workspaces_root.as_deref(),
    );
    repository.worktrees = worktrees;
    repository_repository::upsert(&state.db, &repository)?;
    if repo_intelligence_service::repo_intelligence_enabled(state) {
        let _ = repo_intelligence_service::refresh_repository_intelligence(
            state,
            &repository.id,
            &repository.path,
            false,
        );
    }
    Ok(())
}

fn collect_git_repositories(
    path: &Path,
    depth: usize,
    repo_paths: &mut BTreeSet<PathBuf>,
    warnings: &mut Vec<String>,
) {
    if depth > MAX_SCAN_DEPTH || should_skip(path) {
        return;
    }

    if is_git_repository(path) {
        repo_paths.insert(canonicalize(path));
        return;
    }

    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(err) => {
            warnings.push(format!("Could not read {}: {err}", path.display()));
            return;
        }
    };

    for entry in entries.flatten() {
        let child = entry.path();
        if child.is_dir() {
            collect_git_repositories(&child, depth + 1, repo_paths, warnings);
        }
    }
}

pub fn build_repository(path: &Path, scanned_at: &str) -> Result<DiscoveredRepository, String> {
    let top_level = git(path, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .unwrap_or_else(|_| path.to_path_buf());
    let canonical = canonicalize(&top_level);
    let path_string = canonical.to_string_lossy().to_string();
    let name = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repository")
        .to_string();

    Ok(DiscoveredRepository {
        id: stable_id(&path_string),
        name,
        path: path_string,
        current_branch: worktree_discovery_service::current_branch(&canonical),
        head: worktree_discovery_service::head(&canonical),
        is_dirty: worktree_discovery_service::is_dirty(&canonical),
        worktrees: Vec::new(),
        last_scanned_at: scanned_at.to_string(),
    })
}

fn is_git_repository(path: &Path) -> bool {
    path.join(".git").exists()
        || git(path, &["rev-parse", "--is-inside-work-tree"])
            .map(|value| value == "true")
            .unwrap_or(false)
}

fn should_skip(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };

    matches!(
        name,
        ".git"
            | ".forge"
            | ".forge-worktrees"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | ".next"
            | ".turbo"
            | ".venv"
            | "venv"
            | "Library"
            | "Applications"
    )
}

fn git(path: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .map_err(|err| format!("failed to run git in {}: {err}", path.display()))?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn canonicalize(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn stable_id(input: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in input.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("repo-{hash:016x}")
}

fn unix_timestamp_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}
