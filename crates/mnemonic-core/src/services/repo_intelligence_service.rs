use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::context::cache;
use crate::context::extract;
use crate::context::graph::FileGraph;
use crate::context::ignore::IgnoreSet;
use crate::context::overlay;
use crate::context::schema::{
    ContextPreview, ContextSegment, RepoEntry, RepoMapQuality, RepoMapV2, SelectConfig,
    PARSER_VERSION, REPO_MAP_VERSION,
};
use crate::context::{select, summary, token_fit};
use crate::db::Database;
use crate::repositories::{
    repo_intelligence_repository, repository_repository, settings_repository, workspace_repository,
};
use crate::state::AppState;

const REPO_INTELLIGENCE_KEY: &str = "repo_intelligence_enabled";
const REPO_INTELLIGENCE_VERSION: u32 = 1;
const BACKGROUND_REFRESH_SECONDS: u64 = 300;

#[derive(Debug, Clone)]
pub struct RepoIntelligenceSnapshot {
    pub map: RepoMapV2,
    pub default_branch: String,
    pub commit_hash: String,
    pub stale: bool,
    pub files_indexed: u32,
    pub symbol_count: u32,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
struct WorkspaceRepoIdentity {
    repo_id: String,
    repo_root: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RepoIntelligenceArtifact {
    version: u32,
    generated_at: String,
    entries: Vec<RepoEntry>,
}

#[derive(Debug, Clone)]
struct TreeFile {
    blob_oid: String,
    size_bytes: u64,
}

pub fn repo_intelligence_enabled(state: &AppState) -> bool {
    settings_repository::get_value(&state.db, REPO_INTELLIGENCE_KEY)
        .ok()
        .flatten()
        .map(|value| value == "true")
        .unwrap_or(false)
}

pub fn start_repo_intelligence_loop(state: AppState) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(BACKGROUND_REFRESH_SECONDS));
        if !repo_intelligence_enabled(&state) {
            continue;
        }
        let repos = repository_repository::list(&state.db).unwrap_or_default();
        for repo in repos {
            let _ = refresh_repository_intelligence(&state, &repo.id, &repo.path, false);
        }
    });
}

pub fn refresh_repository_intelligence(
    state: &AppState,
    repo_id: &str,
    repo_path: &str,
    force: bool,
) -> Result<RepoIntelligenceSnapshot, String> {
    let repo_root = canonical_repo_root(Path::new(repo_path))?;
    refresh_repo_index(state, repo_id, &repo_root, force)
}

pub fn ensure_workspace_repo_intelligence(
    state: &AppState,
    workspace_id: &str,
    force: bool,
) -> Result<RepoIntelligenceSnapshot, String> {
    let identity = workspace_repo_identity(state, workspace_id)?;
    refresh_repo_index(state, &identity.repo_id, &identity.repo_root, force)
}

pub fn build_workspace_context_preview(
    state: &AppState,
    workspace_id: &str,
    prompt_hint: Option<&str>,
    cfg: &SelectConfig,
) -> Result<ContextPreview, String> {
    let snapshot = ensure_workspace_repo_intelligence(state, workspace_id, false)?;
    let signal_score = RepoMapQuality::compute(&snapshot.map.entries).signal_score;

    if signal_score < cfg.signal_score_threshold {
        let overlay = overlay::build_workspace_overlay(state, workspace_id);
        let segments = overlay_only_segments(&overlay);
        let total_tokens: u32 = segments.iter().map(|s| s.estimated_tokens).sum();
        return Ok(ContextPreview {
            included: segments,
            excluded: vec![],
            estimated_tokens_context: total_tokens,
            estimated_tokens_total: total_tokens,
            stale_map: snapshot.stale,
            low_signal: true,
            signal_score,
            warning: Some(format!(
                "Repo intelligence signal score {signal_score:.2} is below threshold {:.2}. Using changed-file diffs only.",
                cfg.signal_score_threshold
            )),
        });
    }

    let overlay_data = overlay::build_workspace_overlay(state, workspace_id);
    let graph = FileGraph::build(&snapshot.map.entries);
    let prompt = prompt_hint.unwrap_or("");
    let candidates =
        select::build_candidate_pool(prompt, &overlay_data, &snapshot.map, &graph, cfg);
    let (included, excluded) =
        token_fit::fit_to_budget(candidates, &snapshot.map, &overlay_data, cfg);
    let estimated_tokens_context: u32 = included.iter().map(|s| s.estimated_tokens).sum();

    let warning = if snapshot.stale {
        Some("Repo intelligence is stale (default branch has new commits). Refreshing in background.".to_string())
    } else {
        snapshot.last_error.clone()
    };

    Ok(ContextPreview {
        included,
        excluded,
        estimated_tokens_context,
        estimated_tokens_total: estimated_tokens_context,
        stale_map: snapshot.stale,
        low_signal: false,
        signal_score,
        warning,
    })
}

pub fn workspace_repo_intelligence_status(
    state: &AppState,
    workspace_id: &str,
) -> Result<Option<serde_json::Value>, String> {
    let identity = match workspace_repo_identity(state, workspace_id) {
        Ok(identity) => identity,
        Err(_) => return Ok(None),
    };

    let Some(saved) = repo_intelligence_repository::get(&state.db, &identity.repo_id)? else {
        return Ok(Some(serde_json::json!({
            "enabled": true,
            "repoId": identity.repo_id,
            "indexed": false,
            "stale": true,
        })));
    };

    let default_ref =
        crate::context::discovery::resolve_default_ref(Path::new(&identity.repo_root)).ok();
    let stale = default_ref
        .as_ref()
        .map(|r| r.commit_hash != saved.indexed_commit)
        .unwrap_or(saved.stale);

    Ok(Some(serde_json::json!({
        "enabled": true,
        "repoId": saved.repo_id,
        "repoRoot": identity.repo_root,
        "defaultBranch": saved.default_branch,
        "refName": saved.ref_name,
        "indexedCommit": saved.indexed_commit,
        "filesIndexed": saved.files_indexed,
        "symbolCount": saved.symbol_count,
        "edgeCount": saved.edge_count,
        "generatedAt": saved.generated_at,
        "refreshedAt": saved.refreshed_at,
        "stale": stale,
        "lastError": saved.last_error,
    })))
}

fn refresh_repo_index(
    state: &AppState,
    repo_id: &str,
    repo_root: &str,
    force: bool,
) -> Result<RepoIntelligenceSnapshot, String> {
    let repo_root_path = Path::new(repo_root);
    let default_ref = crate::context::discovery::resolve_default_ref(repo_root_path)?;
    let saved = repo_intelligence_repository::get(&state.db, repo_id)?;

    if !force {
        if let Some(existing) = saved.as_ref() {
            if existing.indexed_commit == default_ref.commit_hash {
                if let Ok(artifact) = load_artifact(Path::new(&existing.artifact_path)) {
                    return Ok(RepoIntelligenceSnapshot {
                        map: RepoMapV2 {
                            version: REPO_MAP_VERSION,
                            entries: artifact.entries,
                        },
                        default_branch: default_ref.branch,
                        commit_hash: default_ref.commit_hash,
                        stale: false,
                        files_indexed: existing.files_indexed,
                        symbol_count: existing.symbol_count,
                        last_error: existing.last_error.clone(),
                    });
                }
            }
        }
    }

    let acquired = {
        let mut guard = state
            .repo_intelligence_inflight
            .lock()
            .map_err(|_| "Repo intelligence lock poisoned".to_string())?;
        if guard.contains(repo_id) {
            false
        } else {
            guard.insert(repo_id.to_string());
            true
        }
    };

    if !acquired {
        if let Some(existing) = saved {
            if let Ok(artifact) = load_artifact(Path::new(&existing.artifact_path)) {
                return Ok(RepoIntelligenceSnapshot {
                    map: RepoMapV2 {
                        version: REPO_MAP_VERSION,
                        entries: artifact.entries,
                    },
                    default_branch: default_ref.branch,
                    commit_hash: default_ref.commit_hash,
                    stale: true,
                    files_indexed: existing.files_indexed,
                    symbol_count: existing.symbol_count,
                    last_error: existing.last_error,
                });
            }
        }
        return Err("Repo intelligence refresh already in progress".to_string());
    }

    let result = refresh_repo_index_inner(state, repo_id, repo_root, force, &default_ref, saved);

    if let Ok(mut guard) = state.repo_intelligence_inflight.lock() {
        guard.remove(repo_id);
    }

    result
}

fn refresh_repo_index_inner(
    state: &AppState,
    repo_id: &str,
    repo_root: &str,
    force: bool,
    default_ref: &crate::context::discovery::DefaultRef,
    saved: Option<repo_intelligence_repository::RepoIntelligenceState>,
) -> Result<RepoIntelligenceSnapshot, String> {
    let repo_root_path = Path::new(repo_root);
    let tree_files = list_tree_files(repo_root_path, &default_ref.ref_name)?;
    let all_paths: HashSet<String> = tree_files.keys().cloned().collect();
    let ignore = IgnoreSet::load(repo_root_path);

    let mut entries = if !force {
        if let Some(existing) = saved.as_ref() {
            if !existing.artifact_path.trim().is_empty() {
                load_artifact(Path::new(&existing.artifact_path))
                    .map(|artifact| artifact.entries)
                    .unwrap_or_default()
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    if entries.is_empty() {
        entries = build_full_entries(
            &state.db,
            repo_root_path,
            &default_ref.ref_name,
            &ignore,
            &all_paths,
            &tree_files,
        );
    } else if let Some(existing) = saved.as_ref() {
        if existing.indexed_commit != default_ref.commit_hash {
            if !existing.indexed_commit.trim().is_empty() {
                apply_incremental_updates(
                    &state.db,
                    repo_root_path,
                    &existing.indexed_commit,
                    &default_ref.commit_hash,
                    &default_ref.ref_name,
                    &ignore,
                    &all_paths,
                    &tree_files,
                    &mut entries,
                )?;
            } else {
                entries = build_full_entries(
                    &state.db,
                    repo_root_path,
                    &default_ref.ref_name,
                    &ignore,
                    &all_paths,
                    &tree_files,
                );
            }
        }
    }

    recompute_graph_fields(&mut entries);

    let artifact = RepoIntelligenceArtifact {
        version: REPO_INTELLIGENCE_VERSION,
        generated_at: timestamp(),
        entries: entries.clone(),
    };

    let artifact_path = artifact_path(state, repo_id)?;
    if let Some(parent) = artifact_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("Failed to create repo intelligence cache dir: {err}"))?;
    }
    save_artifact(&artifact_path, &artifact)?;

    let edge_count = entries
        .iter()
        .map(|entry| entry.imports_internal.len() as u32)
        .sum();
    let symbol_count = entries
        .iter()
        .map(|entry| entry.top_symbols.len() as u32)
        .sum();

    let state_row = repo_intelligence_repository::RepoIntelligenceState {
        repo_id: repo_id.to_string(),
        repository_path: repo_root.to_string(),
        default_branch: default_ref.branch.clone(),
        ref_name: default_ref.ref_name.clone(),
        indexed_commit: default_ref.commit_hash.clone(),
        artifact_path: artifact_path.display().to_string(),
        files_indexed: entries.len() as u32,
        symbol_count,
        edge_count,
        generated_at: artifact.generated_at.clone(),
        refreshed_at: timestamp(),
        stale: false,
        last_error: None,
    };

    if let Err(err) = repo_intelligence_repository::upsert_success(&state.db, &state_row) {
        let _ = repo_intelligence_repository::mark_error(
            &state.db,
            repo_id,
            repo_root,
            &default_ref.branch,
            &default_ref.ref_name,
            &default_ref.commit_hash,
            &err,
        );
        return Err(err);
    }

    Ok(RepoIntelligenceSnapshot {
        map: RepoMapV2 {
            version: REPO_MAP_VERSION,
            entries,
        },
        default_branch: default_ref.branch.clone(),
        commit_hash: default_ref.commit_hash.clone(),
        stale: false,
        files_indexed: state_row.files_indexed,
        symbol_count: state_row.symbol_count,
        last_error: None,
    })
}

#[allow(clippy::too_many_arguments)]
fn apply_incremental_updates(
    db: &Database,
    repo_root: &Path,
    old_commit: &str,
    new_commit: &str,
    ref_name: &str,
    ignore: &IgnoreSet,
    all_paths: &HashSet<String>,
    tree_files: &HashMap<String, TreeFile>,
    entries: &mut Vec<RepoEntry>,
) -> Result<(), String> {
    let diff_output = git(
        repo_root,
        &[
            "diff",
            "--name-status",
            &format!("{old_commit}..{new_commit}"),
        ],
    )?;

    let mut entry_map: HashMap<String, RepoEntry> = entries
        .drain(..)
        .map(|entry| (entry.path.clone(), entry))
        .collect();

    for line in diff_output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.split('\t');
        let status = parts.next().unwrap_or("");
        if status.starts_with('R') {
            let old_path = parts.next().unwrap_or("").trim();
            let new_path = parts.next().unwrap_or("").trim();
            if !old_path.is_empty() {
                entry_map.remove(old_path);
            }
            if !new_path.is_empty() {
                if let Some(entry) = build_entry_for_path(
                    db, repo_root, ref_name, new_path, ignore, all_paths, tree_files,
                ) {
                    entry_map.insert(new_path.to_string(), entry);
                } else {
                    entry_map.remove(new_path);
                }
            }
            continue;
        }

        let path = parts.next().unwrap_or("").trim();
        if path.is_empty() {
            continue;
        }

        if status.starts_with('D') {
            entry_map.remove(path);
            continue;
        }

        if let Some(entry) =
            build_entry_for_path(db, repo_root, ref_name, path, ignore, all_paths, tree_files)
        {
            entry_map.insert(path.to_string(), entry);
        } else {
            entry_map.remove(path);
        }
    }

    *entries = entry_map.into_values().collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(())
}

fn build_full_entries(
    db: &Database,
    repo_root: &Path,
    ref_name: &str,
    ignore: &IgnoreSet,
    all_paths: &HashSet<String>,
    tree_files: &HashMap<String, TreeFile>,
) -> Vec<RepoEntry> {
    let mut entries = Vec::new();
    for path in tree_files.keys() {
        if let Some(entry) =
            build_entry_for_path(db, repo_root, ref_name, path, ignore, all_paths, tree_files)
        {
            entries.push(entry);
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries
}

fn build_entry_for_path(
    db: &Database,
    repo_root: &Path,
    ref_name: &str,
    path: &str,
    ignore: &IgnoreSet,
    all_paths: &HashSet<String>,
    tree_files: &HashMap<String, TreeFile>,
) -> Option<RepoEntry> {
    if ignore.should_exclude(path) {
        return None;
    }

    let file = tree_files.get(path)?;
    let language = IgnoreSet::detect_language(path);
    let flags = ignore.detect_flags(path);

    if ignore.is_conditional_exclude(path) && !flags.is_test {
        return None;
    }

    let (symbols, imports_internal, file_summary) =
        extract_symbols_and_summary(db, repo_root, ref_name, path, file, &language, all_paths);

    Some(RepoEntry {
        path: path.to_string(),
        language,
        blob_oid: Some(file.blob_oid.clone()),
        size_bytes: file.size_bytes,
        loc: 0,
        summary: file_summary,
        base_rank: 0.0,
        imports_internal,
        neighbours: vec![],
        top_symbols: symbols.into_iter().take(5).collect(),
        flags,
    })
}

fn extract_symbols_and_summary(
    db: &Database,
    repo_root: &Path,
    ref_name: &str,
    path: &str,
    file: &TreeFile,
    language: &str,
    all_paths: &HashSet<String>,
) -> (Vec<crate::context::schema::RepoSymbol>, Vec<String>, String) {
    if let Some(cached) = cache::get(db, &file.blob_oid, PARSER_VERSION) {
        return (cached.symbols, cached.imports_internal, cached.summary);
    }

    if !should_extract(language, file.size_bytes) {
        let summary_line = summary::generate_summary(path, "", &[], 140);
        return (vec![], vec![], summary_line);
    }

    let Ok(content) = git(repo_root, &["show", &format!("{}:{}", ref_name, path)]) else {
        let summary_line = summary::generate_summary(path, "", &[], 140);
        return (vec![], vec![], summary_line);
    };

    if content.len() >= 200_000 {
        let summary_line = summary::generate_summary(path, "", &[], 140);
        return (vec![], vec![], summary_line);
    }

    let result = extract::extract(path, &content, all_paths);
    let file_summary = summary::generate_summary(path, &content, &result.symbols, 140);
    let cache_entry = cache::CachedEntry {
        symbols: result.symbols.clone(),
        imports_internal: result.imports_internal.clone(),
        summary: file_summary.clone(),
    };
    cache::put(db, &file.blob_oid, PARSER_VERSION, &cache_entry);
    (result.symbols, result.imports_internal, file_summary)
}

fn should_extract(language: &str, size_bytes: u64) -> bool {
    matches!(
        language,
        "rust" | "typescript" | "javascript" | "python" | "go"
    ) && size_bytes < 200_000
}

fn recompute_graph_fields(entries: &mut [RepoEntry]) {
    let graph = FileGraph::build(entries);
    let ranks = graph.pagerank(0.85, 20);
    for entry in entries.iter_mut() {
        let out = graph.edges.get(&entry.path).cloned().unwrap_or_default();
        let inc = graph
            .reverse_edges
            .get(&entry.path)
            .cloned()
            .unwrap_or_default();
        let mut neighbours: Vec<String> = out.into_iter().chain(inc).collect();
        neighbours.sort();
        neighbours.dedup();
        neighbours.truncate(6);
        entry.neighbours = neighbours;
        entry.base_rank = *ranks.get(&entry.path).unwrap_or(&0.0);
    }
}

fn list_tree_files(root: &Path, ref_name: &str) -> Result<HashMap<String, TreeFile>, String> {
    let output = git(root, &["ls-tree", "-r", "--long", ref_name])?;
    let mut files = HashMap::new();
    for line in output.lines() {
        if let Some((blob_oid, size_bytes, path)) = parse_ls_tree_line(line) {
            files.insert(
                path,
                TreeFile {
                    blob_oid,
                    size_bytes,
                },
            );
        }
    }
    Ok(files)
}

fn parse_ls_tree_line(line: &str) -> Option<(String, u64, String)> {
    let (meta, path) = line.split_once('\t')?;
    let parts: Vec<&str> = meta.split_whitespace().collect();
    if parts.len() < 4 {
        return None;
    }
    let blob_oid = parts[2].to_string();
    let size_bytes: u64 = parts[3].parse().ok()?;
    Some((blob_oid, size_bytes, path.trim().to_string()))
}

fn workspace_repo_identity(
    state: &AppState,
    workspace_id: &str,
) -> Result<WorkspaceRepoIdentity, String> {
    let workspace = workspace_repository::get_detail(&state.db, workspace_id)?
        .ok_or_else(|| format!("Workspace {workspace_id} not found"))?;
    let workspace_root = workspace
        .summary
        .workspace_root_path
        .clone()
        .unwrap_or_else(|| workspace.worktree_path.clone());
    let repo_root = canonical_repo_root(Path::new(&workspace_root))?;

    let repo_id = workspace
        .summary
        .repository_id
        .clone()
        .unwrap_or_else(|| stable_id(&repo_root));

    Ok(WorkspaceRepoIdentity { repo_id, repo_root })
}

fn canonical_repo_root(path: &Path) -> Result<String, String> {
    let raw = git(path, &["rev-parse", "--show-toplevel"])?;
    let top = PathBuf::from(raw.trim());
    let canonical = fs::canonicalize(&top).unwrap_or(top);
    Ok(canonical.to_string_lossy().to_string())
}

fn artifact_path(state: &AppState, repo_id: &str) -> Result<PathBuf, String> {
    Ok(state
        .cache_dir
        .join("repo-intelligence")
        .join(repo_id)
        .join("repo-map.json"))
}

fn save_artifact(path: &Path, artifact: &RepoIntelligenceArtifact) -> Result<(), String> {
    let raw = serde_json::to_string_pretty(artifact).map_err(|err| err.to_string())?;
    fs::write(path, format!("{raw}\n"))
        .map_err(|err| format!("Failed to write repo intelligence artifact: {err}"))
}

fn load_artifact(path: &Path) -> Result<RepoIntelligenceArtifact, String> {
    let raw = fs::read_to_string(path).map_err(|err| {
        format!(
            "Failed to read repo intelligence artifact {}: {err}",
            path.display()
        )
    })?;
    serde_json::from_str(&raw).map_err(|err| {
        format!(
            "Failed to parse repo intelligence artifact {}: {err}",
            path.display()
        )
    })
}

fn git(path: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .map_err(|err| format!("failed to run git in {}: {err}", path.display()))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if stderr.is_empty() {
            format!("git {} failed", args.join(" "))
        } else {
            stderr
        })
    }
}

fn stable_id(input: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in input.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("repo-{hash:016x}")
}

fn timestamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

fn overlay_only_segments(
    overlay_data: &crate::context::schema::WorkspaceOverlay,
) -> Vec<ContextSegment> {
    use crate::context::schema::estimate_tokens;
    let mut segments = Vec::new();
    for file in &overlay_data.changed {
        let content = format!(
            "### {} (changed: +{} -{} lines)\n```diff\n{}\n```",
            file.path, file.additions, file.deletions, file.diff
        );
        segments.push(ContextSegment {
            path: file.path.clone(),
            tier: "mandatory".to_string(),
            render_mode: "diff_hunks".to_string(),
            estimated_tokens: estimate_tokens(&content),
            content,
        });
    }
    for file in &overlay_data.new_files {
        let content = format!("### {} (new file)\n{}", file.path, file.diff);
        segments.push(ContextSegment {
            path: file.path.clone(),
            tier: "mandatory".to_string(),
            render_mode: "full".to_string(),
            estimated_tokens: estimate_tokens(&content),
            content,
        });
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::schema::{RepoFlags, RepoSymbol};
    use crate::db::Database;

    #[test]
    fn parse_ls_tree_line_reads_oid_size_and_path() {
        let line = "100644 blob e69de29bb2d1d6434b8b29ae775ad8c2e48c5391 0\tsrc/lib.rs";
        let parsed = parse_ls_tree_line(line).expect("expected parse");
        assert_eq!(parsed.0, "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
        assert_eq!(parsed.1, 0);
        assert_eq!(parsed.2, "src/lib.rs");
    }

    #[test]
    fn stable_id_is_deterministic() {
        let one = stable_id("/tmp/repo");
        let two = stable_id("/tmp/repo");
        assert_eq!(one, two);
    }

    #[test]
    fn no_op_incremental_update_leaves_entries_unchanged() {
        let root = test_repo_dir("no-op");
        init_git_repo(&root);
        fs::write(root.join("src.rs"), "pub fn a() {}\n").expect("write");
        git_run(&root, &["add", "."]);
        git_run(&root, &["commit", "-m", "base"]);
        let commit = git_out(&root, &["rev-parse", "HEAD"]);

        let db = Database::in_memory().expect("db");
        let ignore = IgnoreSet::load(&root);
        let tree = list_tree_files(&root, &commit).expect("tree");
        let paths: HashSet<String> = tree.keys().cloned().collect();
        let mut entries = build_full_entries(&db, &root, &commit, &ignore, &paths, &tree);
        let before = projection(&entries);

        apply_incremental_updates(
            &db,
            &root,
            &commit,
            &commit,
            &commit,
            &ignore,
            &paths,
            &tree,
            &mut entries,
        )
        .expect("incremental");

        let after = projection(&entries);
        assert_eq!(before, after);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn incremental_update_matches_full_rebuild_for_rename_add_delete_modify() {
        let root = test_repo_dir("incremental");
        init_git_repo(&root);
        fs::create_dir_all(root.join("src")).expect("mkdir");
        fs::write(root.join("src/a.rs"), "pub fn a() -> i32 { 1 }\n").expect("write");
        fs::write(root.join("src/b.rs"), "pub fn b() -> i32 { 2 }\n").expect("write");
        fs::write(root.join("src/remove.rs"), "pub fn remove_me() {}\n").expect("write");
        git_run(&root, &["add", "."]);
        git_run(&root, &["commit", "-m", "base"]);
        let old_commit = git_out(&root, &["rev-parse", "HEAD"]);

        fs::write(
            root.join("src/a.rs"),
            "use crate::src::d::b;\npub fn a() -> i32 { b() + 1 }\n",
        )
        .expect("write");
        fs::write(root.join("src/new.rs"), "pub fn created() -> i32 { 10 }\n").expect("write");
        git_run(&root, &["mv", "src/b.rs", "src/d.rs"]);
        fs::remove_file(root.join("src/remove.rs")).expect("remove");
        git_run(&root, &["add", "-A"]);
        git_run(&root, &["commit", "-m", "change"]);
        let new_commit = git_out(&root, &["rev-parse", "HEAD"]);

        let db = Database::in_memory().expect("db");
        let ignore = IgnoreSet::load(&root);

        let old_tree = list_tree_files(&root, &old_commit).expect("old tree");
        let old_paths: HashSet<String> = old_tree.keys().cloned().collect();
        let mut incremental =
            build_full_entries(&db, &root, &old_commit, &ignore, &old_paths, &old_tree);

        let new_tree = list_tree_files(&root, &new_commit).expect("new tree");
        let new_paths: HashSet<String> = new_tree.keys().cloned().collect();
        apply_incremental_updates(
            &db,
            &root,
            &old_commit,
            &new_commit,
            &new_commit,
            &ignore,
            &new_paths,
            &new_tree,
            &mut incremental,
        )
        .expect("incremental");
        recompute_graph_fields(&mut incremental);

        let mut rebuilt =
            build_full_entries(&db, &root, &new_commit, &ignore, &new_paths, &new_tree);
        recompute_graph_fields(&mut rebuilt);

        assert_same_entries(&incremental, &rebuilt);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cache_hit_returns_without_reading_repo_file() {
        let db = Database::in_memory().expect("db");
        let cached = cache::CachedEntry {
            symbols: vec![RepoSymbol {
                name: "cached_symbol".to_string(),
                kind: "function".to_string(),
                signature: Some("fn cached_symbol()".to_string()),
                line_start: 1,
                line_end: 1,
                symbol_rank: 1.0,
            }],
            imports_internal: vec!["src/lib.rs".to_string()],
            summary: "cached summary".to_string(),
        };
        cache::put(&db, "blob-1", PARSER_VERSION, &cached);

        let (symbols, imports, summary) = extract_symbols_and_summary(
            &db,
            Path::new("/path/that/does/not/exist"),
            "HEAD",
            "src/file.rs",
            &TreeFile {
                blob_oid: "blob-1".to_string(),
                size_bytes: 123,
            },
            "rust",
            &HashSet::new(),
        );

        assert_eq!(summary, "cached summary");
        assert_eq!(imports, vec!["src/lib.rs".to_string()]);
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "cached_symbol");
    }

    #[test]
    fn graph_recompute_is_deterministic() {
        let mut entries = vec![
            make_entry(
                "src/a.rs",
                vec!["src/b.rs".to_string(), "src/c.rs".to_string()],
            ),
            make_entry("src/b.rs", vec!["src/c.rs".to_string()]),
            make_entry("src/c.rs", vec![]),
        ];
        recompute_graph_fields(&mut entries);
        let once = projection(&entries);
        recompute_graph_fields(&mut entries);
        let twice = projection(&entries);
        assert_eq!(once, twice);
    }

    fn make_entry(path: &str, imports: Vec<String>) -> RepoEntry {
        RepoEntry {
            path: path.to_string(),
            language: "rust".to_string(),
            blob_oid: None,
            size_bytes: 0,
            loc: 0,
            summary: path.to_string(),
            base_rank: 0.0,
            imports_internal: imports,
            neighbours: vec![],
            top_symbols: vec![],
            flags: RepoFlags::default(),
        }
    }

    type RepoEntryProjection = (String, String, Vec<String>, Vec<String>, i64);

    fn projection(entries: &[RepoEntry]) -> Vec<RepoEntryProjection> {
        let mut rows = entries
            .iter()
            .map(|entry| {
                (
                    entry.path.clone(),
                    entry.summary.clone(),
                    entry.imports_internal.clone(),
                    entry.neighbours.clone(),
                    (entry.base_rank * 1_000_000.0).round() as i64,
                )
            })
            .collect::<Vec<_>>();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows
    }

    fn assert_same_entries(left: &[RepoEntry], right: &[RepoEntry]) {
        let left_rows = projection(left);
        let right_rows = projection(right);
        assert_eq!(left_rows, right_rows);
    }

    fn init_git_repo(root: &Path) {
        git_run(root, &["init"]);
        git_run(
            root,
            &["config", "user.email", "mnemonic-tests@example.com"],
        );
        git_run(root, &["config", "user.name", "Mnemonic Tests"]);
    }

    fn git_run(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_out(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn test_repo_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "forge-ri-test-{}-{}-{}",
            tag,
            std::process::id(),
            nanos
        ));
        fs::create_dir_all(&dir).expect("create temp repo dir");
        dir
    }
}
