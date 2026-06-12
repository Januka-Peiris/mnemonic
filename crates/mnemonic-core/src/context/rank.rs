use crate::context::graph::FileGraph;
use crate::context::schema::{RepoEntry, WorkspaceOverlay};
use std::collections::HashSet;

pub fn score_candidate(
    path: &str,
    entry: Option<&RepoEntry>,
    seeds: &[String],
    overlay: &WorkspaceOverlay,
    prompt_keywords: &[String],
    graph: &FileGraph,
) -> f32 {
    let seed_set: HashSet<&str> = seeds.iter().map(|s| s.as_str()).collect();

    // Binary membership flags
    let m_selected = 0.0f32; // reserved for explicit user selection (not yet wired)
    let m_pinned = 0.0f32; // reserved for pinned files
    let m_new = if overlay.new_files.iter().any(|f| f.path == path) {
        1.0
    } else {
        0.0
    };

    // Changed file contribution (proportional to lines changed)
    let c_changed = overlay
        .changed
        .iter()
        .find(|f| f.path == path)
        .map(|f| ((f.additions + f.deletions) as f32 / 40.0).min(1.0))
        .unwrap_or(0.0);

    // Graph edge weights (one-hop in/out from seeds)
    let r_seed_to_f = {
        let incomers: Vec<&str> = graph
            .reverse_edges
            .get(path)
            .map(|v| v.iter().map(|s| s.as_str()).collect())
            .unwrap_or_default();
        let seed_count = incomers.iter().filter(|p| seed_set.contains(*p)).count();
        if seed_count > 0 {
            (seed_count as f32).min(3.0) / 3.0
        } else {
            0.0
        }
    };
    let r_f_to_seed = {
        let outgoers: Vec<&str> = graph
            .edges
            .get(path)
            .map(|v| v.iter().map(|s| s.as_str()).collect())
            .unwrap_or_default();
        let seed_count = outgoers.iter().filter(|p| seed_set.contains(*p)).count();
        if seed_count > 0 {
            (seed_count as f32).min(3.0) / 3.0
        } else {
            0.0
        }
    };

    // Same directory as any seed
    let path_dir = std::path::Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let i_same_dir = if seeds.iter().any(|seed| {
        std::path::Path::new(seed)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .as_deref()
            == Some(&path_dir)
    }) {
        1.0
    } else {
        0.0
    };

    // Jaccard: import overlap between this file and seeds
    let j_import_overlap = if let Some(entry) = entry {
        let file_imports: HashSet<&str> =
            entry.imports_internal.iter().map(|s| s.as_str()).collect();
        let seed_imports: HashSet<&str> = seeds
            .iter()
            .flat_map(|s| {
                graph
                    .edges
                    .get(s)
                    .map(|v| v.iter().map(|x| x.as_str()).collect::<Vec<_>>())
                    .unwrap_or_default()
            })
            .collect();
        let intersection = file_imports.intersection(&seed_imports).count();
        let union = file_imports.union(&seed_imports).count();
        if union > 0 {
            intersection as f32 / union as f32
        } else {
            0.0
        }
    } else {
        0.0
    };

    // Jaccard: prompt term overlap with path + symbol tokens
    let t_term_overlap = if !prompt_keywords.is_empty() {
        let path_tokens: HashSet<String> = tokenise_path(path);
        let symbol_tokens: HashSet<String> = entry
            .map(|e| {
                e.top_symbols
                    .iter()
                    .flat_map(|s| tokenise_identifier(&s.name))
                    .collect()
            })
            .unwrap_or_default();
        let file_tokens: HashSet<&str> = path_tokens
            .iter()
            .chain(symbol_tokens.iter())
            .map(|s| s.as_str())
            .collect();
        let kw_set: HashSet<&str> = prompt_keywords.iter().map(|s| s.as_str()).collect();
        let intersection = file_tokens.intersection(&kw_set).count();
        let union = file_tokens.union(&kw_set).count();
        if union > 0 {
            intersection as f32 / union as f32
        } else {
            0.0
        }
    } else {
        0.0
    };

    // Offline base rank (normalised PageRank)
    let b_base_rank = entry.map(|e| e.base_rank).unwrap_or(0.0);

    // Entrypoint heuristic
    let filename = std::path::Path::new(path)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("");
    let e_entrypoint = if matches!(
        filename,
        "main.rs"
            | "lib.rs"
            | "index.ts"
            | "index.tsx"
            | "index.js"
            | "main.py"
            | "app.py"
            | "main.go"
    ) {
        1.0
    } else {
        0.0
    };

    // Penalty flags
    let p_test = if entry.map(|e| e.flags.is_test).unwrap_or(false) {
        1.0
    } else {
        0.0
    };
    let p_config = if entry.map(|e| e.flags.is_config).unwrap_or(false) {
        1.0
    } else {
        0.0
    };
    let p_generated = if entry.map(|e| e.flags.is_generated).unwrap_or(false) {
        1.0
    } else {
        0.0
    };

    8.0 * m_selected
        + 6.0 * m_pinned
        + 5.0 * c_changed
        + 4.0 * m_new
        + 2.5 * r_seed_to_f
        + 2.0 * r_f_to_seed
        + 1.5 * i_same_dir
        + 1.5 * j_import_overlap
        + 1.5 * t_term_overlap
        + 1.0 * b_base_rank
        + 0.5 * e_entrypoint
        - 2.0 * p_test
        - 3.0 * p_config
        - 5.0 * p_generated
}

pub fn extract_keywords(prompt: &str) -> Vec<String> {
    const STOPWORDS: &[&str] = &[
        "the", "a", "an", "is", "in", "to", "of", "and", "or", "for", "with", "that", "this", "it",
        "be", "are", "was", "were", "have", "has", "had", "do", "does", "did", "will", "would",
        "could", "should", "may", "might", "can", "not", "but", "by", "on", "at", "from", "as",
        "if", "then", "so", "up", "out", "about", "into", "than", "also", "been", "its", "get",
        "set", "add", "new",
    ];
    prompt
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .map(|w| w.to_lowercase())
        .filter(|w| w.len() >= 3 && !STOPWORDS.contains(&w.as_str()))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect()
}

fn tokenise_path(path: &str) -> HashSet<String> {
    path.split(['/', '\\', '.', '-', '_'])
        .map(|s| s.to_lowercase())
        .filter(|s| s.len() >= 3)
        .collect()
}

fn tokenise_identifier(name: &str) -> Vec<String> {
    // Split camelCase and snake_case
    let mut parts = Vec::new();
    let mut current = String::new();
    for ch in name.chars() {
        if ch.is_uppercase() && !current.is_empty() {
            parts.push(current.to_lowercase());
            current = String::new();
        } else if ch == '_' || ch == '-' {
            if !current.is_empty() {
                parts.push(current.to_lowercase());
                current = String::new();
            }
            continue;
        }
        current.push(ch);
    }
    if !current.is_empty() {
        parts.push(current.to_lowercase());
    }
    parts.into_iter().filter(|s| s.len() >= 3).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_filters_stopwords() {
        let kw = extract_keywords("add a new feature to handle user authentication");
        assert!(kw.contains(&"feature".to_string()));
        assert!(kw.contains(&"handle".to_string()));
        assert!(kw.contains(&"user".to_string()));
        assert!(kw.contains(&"authentication".to_string()));
        assert!(!kw.contains(&"the".to_string()));
        assert!(!kw.contains(&"add".to_string()));
        assert!(!kw.contains(&"new".to_string()));
    }
}
