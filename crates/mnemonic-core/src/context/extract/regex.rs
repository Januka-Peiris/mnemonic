use super::ExtractResult;
use crate::context::schema::RepoSymbol;
use std::collections::HashSet;

pub fn extract(path: &str, content: &str, repo_files: &HashSet<String>) -> ExtractResult {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let symbols = extract_symbols(content, ext);
    let imports_internal = extract_imports(content, ext, path, repo_files);
    ExtractResult {
        symbols,
        imports_internal,
        engine: "regex".to_string(),
    }
}

fn extract_symbols(content: &str, ext: &str) -> Vec<RepoSymbol> {
    let mut symbols = Vec::new();
    for (line_idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        let line_num = line_idx as u32 + 1;
        let sym = match ext {
            "rs" => extract_rust_symbol(trimmed, line_num),
            "ts" | "tsx" | "js" | "jsx" => extract_ts_symbol(trimmed, line_num),
            "py" => extract_py_symbol(trimmed, line_num),
            "go" => extract_go_symbol(trimmed, line_num),
            _ => None,
        };
        if let Some(s) = sym {
            if !symbols
                .iter()
                .any(|existing: &RepoSymbol| existing.name == s.name)
            {
                symbols.push(s);
            }
        }
        if symbols.len() >= 30 {
            break;
        }
    }
    symbols
}

fn extract_rust_symbol(line: &str, ln: u32) -> Option<RepoSymbol> {
    let prefixes = [
        ("pub fn ", "function"),
        ("pub async fn ", "function"),
        ("pub struct ", "struct"),
        ("pub enum ", "enum"),
        ("pub trait ", "trait"),
        ("pub type ", "type"),
    ];
    for (prefix, kind) in &prefixes {
        if let Some(rest) = line.strip_prefix(prefix) {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                return Some(RepoSymbol {
                    name,
                    kind: kind.to_string(),
                    signature: Some(line.to_string()),
                    line_start: ln,
                    line_end: ln,
                    symbol_rank: 0.5,
                });
            }
        }
    }
    None
}

fn extract_ts_symbol(line: &str, ln: u32) -> Option<RepoSymbol> {
    let prefixes = [
        ("export function ", "function"),
        ("export async function ", "function"),
        ("export default function ", "function"),
        ("export class ", "class"),
        ("export interface ", "interface"),
        ("export type ", "type"),
        ("export enum ", "enum"),
        ("export const ", "const"),
    ];
    for (prefix, kind) in &prefixes {
        if let Some(rest) = line.strip_prefix(prefix) {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() && name != "=" {
                return Some(RepoSymbol {
                    name,
                    kind: kind.to_string(),
                    signature: Some(line.to_string()),
                    line_start: ln,
                    line_end: ln,
                    symbol_rank: 0.5,
                });
            }
        }
    }
    None
}

fn extract_py_symbol(line: &str, ln: u32) -> Option<RepoSymbol> {
    for (prefix, kind) in &[
        ("def ", "function"),
        ("async def ", "function"),
        ("class ", "class"),
    ] {
        if let Some(rest) = line.strip_prefix(prefix) {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                return Some(RepoSymbol {
                    name,
                    kind: kind.to_string(),
                    signature: Some(line.to_string()),
                    line_start: ln,
                    line_end: ln,
                    symbol_rank: 0.5,
                });
            }
        }
    }
    None
}

fn extract_go_symbol(line: &str, ln: u32) -> Option<RepoSymbol> {
    if let Some(rest) = line.strip_prefix("func ") {
        let name: String = rest
            .chars()
            .skip_while(|c| *c == '(')
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() {
            return Some(RepoSymbol {
                name,
                kind: "function".to_string(),
                signature: Some(line.to_string()),
                line_start: ln,
                line_end: ln,
                symbol_rank: 0.5,
            });
        }
    }
    None
}

fn extract_imports(
    content: &str,
    ext: &str,
    current_path: &str,
    repo_files: &HashSet<String>,
) -> Vec<String> {
    let mut imports = Vec::new();
    let current_dir = std::path::Path::new(current_path)
        .parent()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();

    for line in content.lines() {
        let trimmed = line.trim();
        let raw_import = match ext {
            "ts" | "tsx" | "js" | "jsx" => extract_ts_import(trimmed),
            "py" => extract_py_import(trimmed),
            "rs" => extract_rs_import(trimmed),
            _ => None,
        };
        if let Some(raw) = raw_import {
            // Resolve relative imports to repo-relative paths
            if let Some(resolved) = resolve_import_path(&raw, &current_dir, repo_files) {
                if !imports.contains(&resolved) {
                    imports.push(resolved);
                }
            }
        }
    }
    imports
}

fn extract_ts_import(line: &str) -> Option<String> {
    // import ... from 'path' or import 'path'
    if line.starts_with("import ") {
        if let Some(from_pos) = line.rfind(" from ") {
            let after = line[from_pos + 6..]
                .trim()
                .trim_matches(|c| c == '\'' || c == '"' || c == ';');
            return Some(after.to_string());
        }
    }
    // require('path')
    if line.contains("require(") {
        if let Some(start) = line.find("require('").or_else(|| line.find("require(\"")) {
            let after = &line[start + 9..];
            let end = after.find(['\'', '"']).unwrap_or(0);
            let path = after[..end].trim();
            if !path.is_empty() {
                return Some(path.to_string());
            }
        }
    }
    None
}

fn extract_py_import(line: &str) -> Option<String> {
    if let Some(rest) = line.strip_prefix("from ") {
        let module = rest.split_whitespace().next()?;
        // Only relative imports (start with .)
        if module.starts_with('.') {
            return Some(module.to_string());
        }
    }
    None
}

fn extract_rs_import(line: &str) -> Option<String> {
    // `use crate::...` paths — convert to file paths
    if let Some(rest) = line.strip_prefix("use crate::") {
        let path: String = rest
            .trim_end_matches(';')
            .split("::")
            .collect::<Vec<_>>()
            .first()
            .unwrap_or(&"")
            .to_string();
        if !path.is_empty() && !path.starts_with('{') {
            return Some(format!("src/{}.rs", path));
        }
    }
    None
}

pub fn resolve_import_path(
    raw: &str,
    current_dir: &str,
    repo_files: &HashSet<String>,
) -> Option<String> {
    if !raw.starts_with('.') {
        return None; // External package, not internal
    }
    // Build candidate paths
    let base = if current_dir.is_empty() {
        raw.to_string()
    } else {
        format!("{}/{}", current_dir, raw)
    };
    let candidates = vec![
        format!("{}.ts", base),
        format!("{}.tsx", base),
        format!("{}.js", base),
        format!("{}.jsx", base),
        format!("{}/index.ts", base),
        format!("{}/index.tsx", base),
        format!("{}/index.js", base),
        base.clone(),
    ];
    for candidate in candidates {
        // Normalise: resolve ../ segments
        let normalised = normalise_path(&candidate);
        if repo_files.contains(&normalised) {
            return Some(normalised);
        }
    }
    None
}

fn normalise_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            p => parts.push(p),
        }
    }
    parts.join("/")
}
