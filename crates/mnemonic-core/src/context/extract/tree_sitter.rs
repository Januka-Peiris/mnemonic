use std::collections::HashSet;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Node, Parser, Query, QueryCursor};

use crate::context::schema::RepoSymbol;

pub fn extract(
    path: &str,
    content: &str,
    repo_files: &HashSet<String>,
) -> Option<(Vec<RepoSymbol>, Vec<String>)> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let (language, queries) = language_and_queries(ext)?;

    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(content.as_bytes(), None)?;

    let root = tree.root_node();
    if root.kind() == "ERROR" {
        return None;
    }

    let symbols = extract_symbols_from_tree(root, content, &queries.symbol_query);
    let imports = extract_imports_from_tree(root, content, path, &queries.import_query, repo_files);

    Some((symbols, imports))
}

struct LangQueries {
    symbol_query: Query,
    import_query: Option<Query>,
}

fn language_and_queries(ext: &str) -> Option<(Language, LangQueries)> {
    match ext {
        "rs" => {
            let lang = Language::new(tree_sitter_rust::LANGUAGE);
            let sq = Query::new(
                &lang,
                r#"
                (function_item name: (identifier) @fn.name)
                (struct_item name: (type_identifier) @struct.name)
                (enum_item name: (type_identifier) @enum.name)
                (trait_item name: (type_identifier) @trait.name)
                (type_item name: (type_identifier) @type.name)
                "#,
            )
            .ok()?;
            Some((
                lang,
                LangQueries {
                    symbol_query: sq,
                    import_query: None,
                },
            ))
        }
        "ts" => {
            let lang = Language::new(tree_sitter_typescript::LANGUAGE_TYPESCRIPT);
            let sq = Query::new(
                &lang,
                r#"
                (function_declaration name: (identifier) @fn.name)
                (class_declaration name: (type_identifier) @class.name)
                (interface_declaration name: (type_identifier) @interface.name)
                (type_alias_declaration name: (type_identifier) @type.name)
                (enum_declaration name: (identifier) @enum.name)
                "#,
            )
            .ok()?;
            let iq = Query::new(&lang, r#"(import_statement source: (string) @import_path)"#).ok();
            Some((
                lang,
                LangQueries {
                    symbol_query: sq,
                    import_query: iq,
                },
            ))
        }
        "tsx" => {
            let lang = Language::new(tree_sitter_typescript::LANGUAGE_TSX);
            let sq = Query::new(
                &lang,
                r#"
                (function_declaration name: (identifier) @fn.name)
                (class_declaration name: (type_identifier) @class.name)
                (interface_declaration name: (type_identifier) @interface.name)
                (type_alias_declaration name: (type_identifier) @type.name)
                (enum_declaration name: (identifier) @enum.name)
                "#,
            )
            .ok()?;
            let iq = Query::new(&lang, r#"(import_statement source: (string) @import_path)"#).ok();
            Some((
                lang,
                LangQueries {
                    symbol_query: sq,
                    import_query: iq,
                },
            ))
        }
        "js" | "jsx" | "mjs" => {
            let lang = Language::new(tree_sitter_javascript::LANGUAGE);
            let sq = Query::new(
                &lang,
                r#"
                (function_declaration name: (identifier) @fn.name)
                (class_declaration name: (identifier) @class.name)
                "#,
            )
            .ok()?;
            let iq = Query::new(&lang, r#"(import_statement source: (string) @import_path)"#).ok();
            Some((
                lang,
                LangQueries {
                    symbol_query: sq,
                    import_query: iq,
                },
            ))
        }
        "py" => {
            let lang = Language::new(tree_sitter_python::LANGUAGE);
            let sq = Query::new(
                &lang,
                r#"
                (function_definition name: (identifier) @fn.name)
                (class_definition name: (identifier) @class.name)
                "#,
            )
            .ok()?;
            Some((
                lang,
                LangQueries {
                    symbol_query: sq,
                    import_query: None,
                },
            ))
        }
        "go" => {
            let lang = Language::new(tree_sitter_go::LANGUAGE);
            let sq = Query::new(
                &lang,
                r#"
                (function_declaration name: (identifier) @fn.name)
                (method_declaration name: (field_identifier) @method.name)
                "#,
            )
            .ok()?;
            Some((
                lang,
                LangQueries {
                    symbol_query: sq,
                    import_query: None,
                },
            ))
        }
        _ => None,
    }
}

fn extract_symbols_from_tree(root: Node, content: &str, query: &Query) -> Vec<RepoSymbol> {
    let mut cursor = QueryCursor::new();
    let mut symbols: Vec<RepoSymbol> = Vec::new();
    let bytes = content.as_bytes();

    let mut matches = cursor.matches(query, root, bytes);
    while let Some(m) = matches.next() {
        for capture in m.captures {
            let node = capture.node;
            let capture_name = &query.capture_names()[capture.index as usize];
            let name = node.utf8_text(bytes).unwrap_or("").to_string();
            if name.is_empty() || symbols.iter().any(|s: &RepoSymbol| s.name == name) {
                continue;
            }
            let kind = capture_name_to_kind(capture_name);
            let line_start = node.start_position().row as u32 + 1;
            let line_end = node.end_position().row as u32 + 1;
            // Signature: first line of the parent node, capped at 120 chars
            let parent = node.parent();
            let signature = parent.and_then(|p| {
                let start_byte = p.start_byte();
                let end_byte = (start_byte + 120).min(bytes.len());
                std::str::from_utf8(&bytes[start_byte..end_byte])
                    .ok()
                    .map(|s| s.lines().next().unwrap_or("").trim().to_string())
            });
            symbols.push(RepoSymbol {
                name,
                kind,
                signature,
                line_start,
                line_end,
                symbol_rank: 0.5,
            });
            if symbols.len() >= 30 {
                return symbols;
            }
        }
    }
    symbols
}

fn extract_imports_from_tree(
    root: Node,
    content: &str,
    current_path: &str,
    import_query: &Option<Query>,
    repo_files: &HashSet<String>,
) -> Vec<String> {
    let Some(query) = import_query else {
        return vec![];
    };
    let bytes = content.as_bytes();
    let mut cursor = QueryCursor::new();
    let current_dir = std::path::Path::new(current_path)
        .parent()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    let mut imports = Vec::new();

    let mut matches = cursor.matches(query, root, bytes);
    while let Some(m) = matches.next() {
        for capture in m.captures {
            let raw = capture.node.utf8_text(bytes).unwrap_or("");
            let raw = raw.trim_matches(|c| c == '\'' || c == '"');
            if !raw.starts_with('.') {
                continue; // skip external
            }
            if let Some(resolved) =
                crate::context::extract::regex::resolve_import_path(raw, &current_dir, repo_files)
            {
                if !imports.contains(&resolved) {
                    imports.push(resolved);
                }
            }
        }
    }
    imports
}

fn capture_name_to_kind(name: &str) -> String {
    match name {
        "fn.name" | "fn" => "function",
        "struct.name" | "struct" => "struct",
        "enum.name" | "enum" => "enum",
        "trait.name" | "trait" => "trait",
        "type.name" | "type" => "type",
        "class.name" | "class" => "class",
        "interface.name" | "interface" => "interface",
        "method.name" | "method" => "method",
        _ => "symbol",
    }
    .to_string()
}
