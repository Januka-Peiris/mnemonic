use std::collections::HashMap;

use crate::context::schema::RepoEntry;

pub struct FileGraph {
    pub nodes: Vec<String>,
    /// path → list of paths it imports (outgoing edges)
    pub edges: HashMap<String, Vec<String>>,
    /// path → list of paths that import it (incoming edges)
    pub reverse_edges: HashMap<String, Vec<String>>,
}

impl FileGraph {
    pub fn build(entries: &[RepoEntry]) -> Self {
        let nodes: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
        let node_set: std::collections::HashSet<&str> = nodes.iter().map(|s| s.as_str()).collect();

        let mut edges: HashMap<String, Vec<String>> = HashMap::new();
        let mut reverse_edges: HashMap<String, Vec<String>> = HashMap::new();

        for entry in entries {
            let out: Vec<String> = entry
                .imports_internal
                .iter()
                .filter(|imp| node_set.contains(imp.as_str()))
                .cloned()
                .collect();
            for target in &out {
                reverse_edges
                    .entry(target.clone())
                    .or_default()
                    .push(entry.path.clone());
            }
            edges.insert(entry.path.clone(), out);
        }

        Self {
            nodes,
            edges,
            reverse_edges,
        }
    }

    /// Weighted PageRank with standard power iteration.
    /// d = 0.85, convergence threshold 1e-6, max 30 iterations.
    pub fn pagerank(&self, damping: f32, iterations: u32) -> HashMap<String, f32> {
        let n = self.nodes.len();
        if n == 0 {
            return HashMap::new();
        }
        let base = (1.0 - damping) / n as f32;
        let mut rank: HashMap<String, f32> = self
            .nodes
            .iter()
            .map(|p| (p.clone(), 1.0 / n as f32))
            .collect();

        for _ in 0..iterations {
            let mut new_rank: HashMap<String, f32> =
                self.nodes.iter().map(|p| (p.clone(), base)).collect();

            for path in &self.nodes {
                let out = self.edges.get(path).map(|v| v.as_slice()).unwrap_or(&[]);
                let out_degree = out.len();
                if out_degree == 0 {
                    // Dangling node: distribute rank equally to all
                    let contrib = damping * rank[path] / n as f32;
                    for target in &self.nodes {
                        *new_rank.get_mut(target).unwrap() += contrib;
                    }
                } else {
                    let contrib = damping * rank[path] / out_degree as f32;
                    for target in out {
                        if let Some(r) = new_rank.get_mut(target) {
                            *r += contrib;
                        }
                    }
                }
            }

            // Check convergence
            let delta: f32 = self
                .nodes
                .iter()
                .map(|p| (new_rank[p] - rank[p]).abs())
                .sum();
            rank = new_rank;
            if delta < 1e-6 {
                break;
            }
        }

        // Normalise to [0, 1]
        let max = rank.values().cloned().fold(0.0_f32, f32::max);
        if max > 0.0 {
            for v in rank.values_mut() {
                *v /= max;
            }
        }
        rank
    }

    /// Returns up to `limit_per_seed` one-hop neighbours for each seed path.
    /// Includes both files that the seed imports AND files that import the seed.
    pub fn one_hop_neighbours(&self, seeds: &[String], limit_per_seed: usize) -> Vec<String> {
        let seed_set: std::collections::HashSet<&str> = seeds.iter().map(|s| s.as_str()).collect();
        let mut neighbours: std::collections::HashSet<String> = std::collections::HashSet::new();

        for seed in seeds {
            let mut seed_neighbours: Vec<String> = Vec::new();
            // Outgoing (seed imports these)
            if let Some(out) = self.edges.get(seed) {
                seed_neighbours.extend(
                    out.iter()
                        .filter(|p| !seed_set.contains(p.as_str()))
                        .cloned(),
                );
            }
            // Incoming (these import seed)
            if let Some(inc) = self.reverse_edges.get(seed) {
                seed_neighbours.extend(
                    inc.iter()
                        .filter(|p| !seed_set.contains(p.as_str()))
                        .cloned(),
                );
            }
            seed_neighbours.dedup();
            for n in seed_neighbours.into_iter().take(limit_per_seed) {
                neighbours.insert(n);
            }
        }

        neighbours
            .into_iter()
            .filter(|p| !seed_set.contains(p.as_str()))
            .collect()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.values().map(|v| v.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::schema::{RepoEntry, RepoFlags};

    fn make_entry(path: &str, imports: &[&str]) -> RepoEntry {
        RepoEntry {
            path: path.to_string(),
            language: "rust".to_string(),
            blob_oid: None,
            size_bytes: 0,
            loc: 0,
            summary: String::new(),
            base_rank: 0.0,
            imports_internal: imports.iter().map(|s| s.to_string()).collect(),
            neighbours: Vec::new(),
            top_symbols: Vec::new(),
            flags: RepoFlags::default(),
        }
    }

    #[test]
    fn pagerank_central_node_scores_higher() {
        // a imports b and c; b imports d; c imports d → d is most central (imported by 2)
        let entries = vec![
            make_entry("a.rs", &["b.rs", "c.rs"]),
            make_entry("b.rs", &["d.rs"]),
            make_entry("c.rs", &["d.rs"]),
            make_entry("d.rs", &[]),
        ];
        let graph = FileGraph::build(&entries);
        let ranks = graph.pagerank(0.85, 30);
        assert!(ranks["d.rs"] > ranks["a.rs"], "d should rank higher than a");
    }

    #[test]
    fn one_hop_finds_neighbours() {
        let entries = vec![
            make_entry("a.rs", &["b.rs"]),
            make_entry("b.rs", &["c.rs"]),
            make_entry("c.rs", &[]),
        ];
        let graph = FileGraph::build(&entries);
        let neighbours = graph.one_hop_neighbours(&["a.rs".to_string()], 10);
        assert!(neighbours.contains(&"b.rs".to_string()));
    }
}
