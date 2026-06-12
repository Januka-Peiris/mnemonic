use serde::{Deserialize, Serialize};

pub const REPO_MAP_VERSION: u32 = 4;
pub const PARSER_VERSION: &str = "tree-sitter-v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoEntry {
    pub path: String,
    pub language: String,
    pub blob_oid: Option<String>,
    pub size_bytes: u64,
    pub loc: u32,
    pub summary: String,
    pub base_rank: f32,
    pub imports_internal: Vec<String>,
    pub neighbours: Vec<String>,
    pub top_symbols: Vec<RepoSymbol>,
    pub flags: RepoFlags,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoSymbol {
    pub name: String,
    pub kind: String,
    pub signature: Option<String>,
    pub line_start: u32,
    pub line_end: u32,
    pub symbol_rank: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RepoFlags {
    pub is_test: bool,
    pub is_config: bool,
    pub is_generated: bool,
    pub is_binary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoMapV2 {
    pub version: u32,
    pub entries: Vec<RepoEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoMapMetaV2 {
    pub version: u32,
    pub default_branch: String,
    pub base_commit: String,
    pub generated_at: String,
    pub generator: GeneratorInfo,
    pub exclusions: Vec<String>,
    pub stats: RepoMapStats,
    pub quality: RepoMapQuality,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratorInfo {
    pub engine: String,
    pub forge_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RepoMapStats {
    pub files_scanned: u32,
    pub files_indexed: u32,
    pub files_excluded: u32,
    pub symbol_count: u32,
    pub internal_edge_count: u32,
    pub languages: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RepoMapQuality {
    pub signal_score: f32,
    pub symbol_coverage: f32,
    pub noise_ratio: f32,
}

impl RepoMapQuality {
    pub fn compute(entries: &[RepoEntry]) -> Self {
        let total = entries.len() as f32;
        if total == 0.0 {
            return Self::default();
        }
        let source_files: Vec<&RepoEntry> = entries
            .iter()
            .filter(|e| !e.flags.is_config && !e.flags.is_generated && !e.flags.is_binary)
            .collect();
        let source_share = source_files.len() as f32 / total;
        let with_symbols = source_files
            .iter()
            .filter(|e| !e.top_symbols.is_empty())
            .count() as f32;
        let symbol_coverage = if source_files.is_empty() {
            0.0
        } else {
            with_symbols / source_files.len() as f32
        };
        let total_internal_edges: usize = entries.iter().map(|e| e.imports_internal.len()).sum();
        let ref_density = (total_internal_edges as f32 / total).min(1.0);
        let noise_files = entries
            .iter()
            .filter(|e| e.flags.is_test || e.flags.is_config || e.flags.is_generated)
            .count() as f32;
        let noise_ratio = noise_files / total;
        let signal_score = 0.35 * source_share
            + 0.25 * symbol_coverage
            + 0.20 * ref_density
            + 0.20 * (1.0 - noise_ratio);
        Self {
            signal_score,
            symbol_coverage,
            noise_ratio,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceOverlay {
    pub changed: Vec<OverlayFile>,
    pub new_files: Vec<OverlayFile>,
    pub deleted: Vec<String>,
    pub renamed: Vec<RenamePair>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayFile {
    pub path: String,
    pub diff: String,
    pub additions: u32,
    pub deletions: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenamePair {
    pub old: String,
    pub new: String,
}

#[derive(Debug, Clone)]
pub struct ContextCandidate {
    pub path: String,
    pub mandatory: bool,
    pub score: f32,
    pub render_mode: RenderMode,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RenderMode {
    Full,
    DiffHunks,
    SymbolCard,
    SummaryLine,
}

impl RenderMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            RenderMode::Full => "full",
            RenderMode::DiffHunks => "diff_hunks",
            RenderMode::SymbolCard => "symbol_card",
            RenderMode::SummaryLine => "summary_line",
        }
    }

    pub fn can_degrade(&self) -> bool {
        *self != RenderMode::SummaryLine
    }

    pub fn degraded(&self) -> Self {
        match self {
            RenderMode::Full => RenderMode::DiffHunks,
            RenderMode::DiffHunks => RenderMode::SymbolCard,
            RenderMode::SymbolCard => RenderMode::SummaryLine,
            RenderMode::SummaryLine => RenderMode::SummaryLine,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextPreview {
    pub included: Vec<ContextSegment>,
    pub excluded: Vec<String>,
    pub estimated_tokens_context: u32,
    pub estimated_tokens_total: u32,
    pub stale_map: bool,
    pub low_signal: bool,
    pub signal_score: f32,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSegment {
    pub path: String,
    pub tier: String,
    pub render_mode: String,
    pub estimated_tokens: u32,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct SelectConfig {
    pub soft_repo_context_tokens: u32,
    pub same_dir_limit_per_seed: usize,
    pub graph_neighbour_limit_per_seed: usize,
    pub fallback_top_ranked_limit: usize,
    pub top_symbols_per_file: usize,
    pub summary_max_chars: usize,
    pub signal_score_threshold: f32,
}

impl Default for SelectConfig {
    fn default() -> Self {
        Self {
            soft_repo_context_tokens: 4000,
            same_dir_limit_per_seed: 2,
            graph_neighbour_limit_per_seed: 6,
            fallback_top_ranked_limit: 12,
            top_symbols_per_file: 5,
            summary_max_chars: 140,
            signal_score_threshold: 0.55,
        }
    }
}

pub fn estimate_tokens(text: &str) -> u32 {
    (text.chars().count() as f32 / 4.0).ceil() as u32
}
