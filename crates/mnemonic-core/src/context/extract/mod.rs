pub mod regex;
pub mod tree_sitter;

use crate::context::schema::RepoSymbol;
use std::collections::HashSet;

pub struct ExtractResult {
    pub symbols: Vec<RepoSymbol>,
    pub imports_internal: Vec<String>,
    pub engine: String, // "tree-sitter" or "regex"
}

/// Try tree-sitter first; fall back to regex on failure.
pub fn extract(path: &str, content: &str, repo_files: &HashSet<String>) -> ExtractResult {
    if let Some((symbols, imports_internal)) = tree_sitter::extract(path, content, repo_files) {
        return ExtractResult {
            symbols,
            imports_internal,
            engine: "tree-sitter".to_string(),
        };
    }

    let result = regex::extract(path, content, repo_files);
    ExtractResult {
        symbols: result.symbols,
        imports_internal: result.imports_internal,
        engine: "regex".to_string(),
    }
}
