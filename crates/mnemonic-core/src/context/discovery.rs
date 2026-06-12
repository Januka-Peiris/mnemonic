use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::context::cache;
use crate::context::extract;
use crate::context::graph::FileGraph;
use crate::context::ignore::IgnoreSet;
use crate::context::schema::{
    GeneratorInfo, RepoEntry, RepoMapMetaV2, RepoMapQuality, RepoMapStats, RepoMapV2,
    PARSER_VERSION, REPO_MAP_VERSION,
};
use crate::context::summary;
use crate::db::Database;

pub struct DefaultRef {
    pub branch: String,
    pub ref_name: String,
    pub commit_hash: String,
}

/// Check whether the cached map is up-to-date.
pub fn is_stale(root: &Path, _db: &Database) -> bool {
    let meta_path = root
        .join(".forge")
        .join("context")
        .join("repo_map.meta.json");
    let Ok(raw) = fs::read_to_string(&meta_path) else {
        return true;
    };
    let Ok(meta) = serde_json::from_str::<RepoMapMetaV2>(&raw) else {
        return true;
    };
    if meta.version != REPO_MAP_VERSION {
        return true;
    }
    // Check if default branch HEAD changed
    let Ok(current_ref) = resolve_default_ref(root) else {
        return true;
    };
    meta.base_commit != current_ref.commit_hash
}

/// Main entry point. If force=false and map is fresh, loads and returns existing map.
pub fn build_repo_map(
    root: &Path,
    force: bool,
    db: &Database,
) -> Result<(RepoMapV2, RepoMapMetaV2), String> {
    let context_dir = root.join(".forge").join("context");
    let map_path = context_dir.join("repo_map.json");
    let meta_path = context_dir.join("repo_map.meta.json");

    let default_ref = resolve_default_ref(root)?;

    if !force && map_path.exists() && meta_path.exists() {
        if let Ok(raw_meta) = fs::read_to_string(&meta_path) {
            if let Ok(meta) = serde_json::from_str::<RepoMapMetaV2>(&raw_meta) {
                if meta.version == REPO_MAP_VERSION && meta.base_commit == default_ref.commit_hash {
                    if let Ok(raw_map) = fs::read_to_string(&map_path) {
                        if let Ok(map) = serde_json::from_str::<RepoMapV2>(&raw_map) {
                            return Ok((map, meta));
                        }
                    }
                }
            }
        }
    }

    // Build fresh
    run_build_pipeline(root, &context_dir, &map_path, &meta_path, &default_ref, db)
}

fn run_build_pipeline(
    root: &Path,
    context_dir: &Path,
    map_path: &Path,
    meta_path: &Path,
    default_ref: &DefaultRef,
    db: &Database,
) -> Result<(RepoMapV2, RepoMapMetaV2), String> {
    let ignore = IgnoreSet::load(root);

    // git ls-tree -r --long <ref> — gives blob OID + size + path
    let ls_output = git(root, &["ls-tree", "-r", "--long", &default_ref.ref_name])?;

    // Collect all file paths for import resolution
    let all_paths: HashSet<String> = ls_output
        .lines()
        .filter_map(|line| parse_ls_tree_line(line).map(|(_, _, path)| path))
        .collect();

    let mut entries: Vec<RepoEntry> = Vec::new();
    let mut stats = RepoMapStats::default();
    let mut engines_used: HashSet<String> = HashSet::new();
    let mut languages_seen: HashSet<String> = HashSet::new();

    for line in ls_output.lines() {
        let Some((blob_oid, size_bytes, path)) = parse_ls_tree_line(line) else {
            continue;
        };
        stats.files_scanned += 1;

        if ignore.should_exclude(&path) {
            stats.files_excluded += 1;
            continue;
        }

        let language = IgnoreSet::detect_language(&path);
        let flags = ignore.detect_flags(&path);
        languages_seen.insert(language.clone());

        // Skip conditional excludes from base map (they can still be added via overlay)
        if ignore.is_conditional_exclude(&path) && !flags.is_test {
            // Include tests but mark them; exclude docs/migrations
            stats.files_excluded += 1;
            continue;
        }

        // Try cache first
        let (symbols, imports_internal, file_summary, engine) =
            if let Some(cached) = cache::get(db, &blob_oid, PARSER_VERSION) {
                (
                    cached.symbols,
                    cached.imports_internal,
                    cached.summary,
                    "cache".to_string(),
                )
            } else if should_extract(&language, size_bytes) {
                // Read file content via git show
                match git(
                    root,
                    &["show", &format!("{}:{}", default_ref.ref_name, path)],
                ) {
                    Ok(content) if content.len() < 200_000 => {
                        let result = extract::extract(&path, &content, &all_paths);
                        let file_summary =
                            summary::generate_summary(&path, &content, &result.symbols, 140);
                        let engine = result.engine.clone();
                        engines_used.insert(engine.clone());
                        let entry_cache = cache::CachedEntry {
                            symbols: result.symbols.clone(),
                            imports_internal: result.imports_internal.clone(),
                            summary: file_summary.clone(),
                        };
                        cache::put(db, &blob_oid, PARSER_VERSION, &entry_cache);
                        (
                            result.symbols,
                            result.imports_internal,
                            file_summary,
                            engine,
                        )
                    }
                    _ => (
                        vec![],
                        vec![],
                        summary::generate_summary(&path, "", &[], 140),
                        "skip".to_string(),
                    ),
                }
            } else {
                let file_summary = summary::generate_summary(&path, "", &[], 140);
                (vec![], vec![], file_summary, "skip".to_string())
            };

        if engine != "skip" {
            engines_used.insert(engine);
        }

        stats.symbol_count += symbols.len() as u32;
        stats.files_indexed += 1;

        entries.push(RepoEntry {
            path,
            language,
            blob_oid: Some(blob_oid),
            size_bytes,
            loc: 0, // Could count newlines in content but not worth the cost
            summary: file_summary,
            base_rank: 0.0, // set after PageRank
            imports_internal,
            neighbours: vec![],
            top_symbols: symbols.into_iter().take(5).collect(),
            flags,
        });
    }

    // Build import graph + PageRank
    let graph = FileGraph::build(&entries);
    stats.internal_edge_count = graph.edge_count() as u32;
    let ranks = graph.pagerank(0.85, 20);

    // Compute neighbours (top-3 in+out per file)
    for entry in &mut entries {
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

    // Quality
    let quality = RepoMapQuality::compute(&entries);
    stats.languages = languages_seen.into_iter().collect();
    stats.languages.sort();

    let engine_name = if engines_used.contains("tree-sitter") {
        "tree-sitter"
    } else {
        "regex-fallback"
    };

    let map = RepoMapV2 {
        version: REPO_MAP_VERSION,
        entries,
    };
    let meta = RepoMapMetaV2 {
        version: REPO_MAP_VERSION,
        default_branch: default_ref.branch.clone(),
        base_commit: default_ref.commit_hash.clone(),
        generated_at: unix_now(),
        generator: GeneratorInfo {
            engine: engine_name.to_string(),
            forge_version: "0.1.0".to_string(),
        },
        exclusions: vec![
            "node_modules/**".into(),
            "target/**".into(),
            "dist/**".into(),
            ".git/**".into(),
            ".venv/**".into(),
        ],
        stats,
        quality,
    };

    // Write to disk
    fs::create_dir_all(context_dir).map_err(|e| format!("Failed to create context dir: {e}"))?;
    write_json(map_path, &map)?;
    write_json(meta_path, &meta)?;

    Ok((map, meta))
}

fn should_extract(language: &str, size_bytes: u64) -> bool {
    matches!(
        language,
        "rust" | "typescript" | "javascript" | "python" | "go"
    ) && size_bytes < 200_000
}

/// Parse a line from `git ls-tree -r --long`:
/// format: `<mode> <type> <blob_oid> <size>\t<path>`
fn parse_ls_tree_line(line: &str) -> Option<(String, u64, String)> {
    let (meta, path) = line.split_once('\t')?;
    let parts: Vec<&str> = meta.split_whitespace().collect();
    if parts.len() < 4 {
        return None;
    }
    let blob_oid = parts[2].to_string();
    let size_bytes: u64 = parts[3].parse().unwrap_or(0);
    Some((blob_oid, size_bytes, path.trim().to_string()))
}

pub fn resolve_default_ref(root: &Path) -> Result<DefaultRef, String> {
    let candidates = [
        git(
            root,
            &[
                "symbolic-ref",
                "--quiet",
                "--short",
                "refs/remotes/origin/HEAD",
            ],
        )
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty()),
        Some("main".to_string()),
        Some("master".to_string()),
        git(root, &["branch", "--show-current"])
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty()),
    ];
    for candidate in candidates.into_iter().flatten() {
        if let Ok(hash) = git(root, &["rev-parse", "--verify", &candidate]) {
            let branch = candidate
                .strip_prefix("origin/")
                .unwrap_or(&candidate)
                .to_string();
            return Ok(DefaultRef {
                branch,
                ref_name: candidate,
                commit_hash: hash.trim().to_string(),
            });
        }
    }
    let hash = git(root, &["rev-parse", "HEAD"])?;
    Ok(DefaultRef {
        branch: "HEAD".to_string(),
        ref_name: "HEAD".to_string(),
        commit_hash: hash.trim().to_string(),
    })
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .map_err(|e| format!("git error: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let raw = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    fs::write(path, format!("{raw}\n"))
        .map_err(|e| format!("Failed to write {}: {e}", path.display()))
}

fn unix_now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}
