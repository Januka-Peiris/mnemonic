use std::collections::HashSet;
use std::path::Path;

use crate::context::graph::FileGraph;
use crate::context::rank;
use crate::context::schema::{
    ContextCandidate, RenderMode, RepoMapV2, SelectConfig, WorkspaceOverlay,
};

pub fn build_candidate_pool(
    prompt: &str,
    overlay: &WorkspaceOverlay,
    map: &RepoMapV2,
    graph: &FileGraph,
    cfg: &SelectConfig,
) -> Vec<ContextCandidate> {
    let keywords = rank::extract_keywords(prompt);

    // Seed files: changed + new
    let seeds: Vec<String> = overlay
        .changed
        .iter()
        .map(|f| f.path.clone())
        .chain(overlay.new_files.iter().map(|f| f.path.clone()))
        .collect();

    // Build entry index for fast lookup
    let entry_map: std::collections::HashMap<&str, &crate::context::schema::RepoEntry> =
        map.entries.iter().map(|e| (e.path.as_str(), e)).collect();

    let mut candidates: HashSet<String> = seeds.iter().cloned().collect();
    let seed_set: HashSet<String> = seeds.iter().cloned().collect();

    // Same-directory neighbours: bounded per seed directory by PageRank
    let seed_dirs: HashSet<String> = seeds
        .iter()
        .filter_map(|s| {
            Path::new(s)
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
        })
        .collect();

    let dir_limit = cfg.same_dir_limit_per_seed.max(1);
    for dir in &seed_dirs {
        let mut in_dir: Vec<&crate::context::schema::RepoEntry> = map
            .entries
            .iter()
            .filter(|e| {
                if seed_set.contains(&e.path) {
                    return false;
                }
                let pdir = Path::new(&e.path)
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                pdir == *dir
            })
            .collect();
        in_dir.sort_by(|a, b| {
            b.base_rank
                .partial_cmp(&a.base_rank)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for e in in_dir.into_iter().take(dir_limit) {
            candidates.insert(e.path.clone());
        }
    }

    // Graph neighbours of seeds
    let graph_neighbours = graph.one_hop_neighbours(&seeds, cfg.graph_neighbour_limit_per_seed);
    for n in graph_neighbours {
        candidates.insert(n);
    }

    // Prompt-term fallback: top-ranked files matching keywords
    if !keywords.is_empty() {
        let mut term_scores: Vec<(f32, &str)> = map
            .entries
            .iter()
            .filter(|e| !candidates.contains(&e.path))
            .map(|e| {
                let path_tokens: HashSet<String> = e
                    .path
                    .split(['/', '.', '_', '-'])
                    .map(|s| s.to_lowercase())
                    .filter(|s| s.len() >= 3)
                    .collect();
                let kw_set: HashSet<&str> = keywords.iter().map(|s| s.as_str()).collect();
                let overlap = path_tokens
                    .iter()
                    .filter(|t| kw_set.contains(t.as_str()))
                    .count();
                (overlap as f32 + e.base_rank, e.path.as_str())
            })
            .filter(|(score, _)| *score > 0.0)
            .collect();
        term_scores.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        for (_, path) in term_scores.into_iter().take(cfg.fallback_top_ranked_limit) {
            candidates.insert(path.to_string());
        }
    }

    // Score all candidates
    let mut result: Vec<ContextCandidate> = candidates
        .into_iter()
        .map(|path| {
            let is_changed = overlay.changed.iter().any(|f| f.path == path);
            let is_new = overlay.new_files.iter().any(|f| f.path == path);
            let mandatory = is_changed || is_new;
            let entry = entry_map.get(path.as_str()).copied();
            let score = rank::score_candidate(&path, entry, &seeds, overlay, &keywords, graph);
            let render_mode = if is_new {
                RenderMode::Full
            } else if is_changed {
                RenderMode::DiffHunks
            } else {
                RenderMode::SymbolCard
            };
            ContextCandidate {
                path,
                mandatory,
                score,
                render_mode,
            }
        })
        .collect();

    result.sort_by(|a, b| {
        b.mandatory
            .cmp(&a.mandatory)
            .then(
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(a.path.cmp(&b.path))
    });

    result
}
