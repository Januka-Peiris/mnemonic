use crate::context::schema::RepoSymbol;

pub fn generate_summary(
    path: &str,
    content: &str,
    symbols: &[RepoSymbol],
    max_chars: usize,
) -> String {
    // Try docstring/header comment first
    if let Some(doc) = extract_docstring(path, content) {
        return truncate(&doc, max_chars);
    }
    // Infer from filename + directory + symbols
    let inferred = infer_from_path_and_symbols(path, symbols);
    truncate(&inferred, max_chars)
}

fn extract_docstring(path: &str, content: &str) -> Option<String> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext {
        "rs" => extract_rust_doc(content),
        "py" => extract_python_doc(content),
        "ts" | "tsx" | "js" | "jsx" => extract_js_doc(content),
        _ => None,
    }
}

fn extract_rust_doc(content: &str) -> Option<String> {
    let mut doc_lines = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("//!") {
            let text = rest.trim();
            if !text.is_empty() {
                doc_lines.push(text.to_string());
            }
        } else if let Some(rest) = trimmed.strip_prefix("///") {
            let text = rest.trim();
            if !text.is_empty() {
                doc_lines.push(text.to_string());
            }
        } else if !trimmed.is_empty() && !trimmed.starts_with("//") {
            break;
        }
    }
    if doc_lines.is_empty() {
        return None;
    }
    let joined = doc_lines.join(" ");
    Some(first_sentence(&joined))
}

fn extract_python_doc(content: &str) -> Option<String> {
    // Look for module-level docstring: first non-blank, non-comment line that is a triple-quote string
    let mut lines = content.lines().peekable();
    // Skip shebang/encoding lines
    while let Some(line) = lines.peek() {
        let t = line.trim();
        if t.starts_with("#") || t.is_empty() {
            lines.next();
        } else {
            break;
        }
    }
    if let Some(line) = lines.next() {
        let t = line.trim();
        let quote = if t.starts_with("\"\"\"") {
            "\"\"\""
        } else if t.starts_with("'''") {
            "'''"
        } else {
            return None;
        };
        let rest = t.strip_prefix(quote).unwrap_or("").trim();
        // Single line docstring
        if let Some(end) = rest.rfind(quote) {
            let doc = rest[..end].trim().to_string();
            if !doc.is_empty() {
                return Some(first_sentence(&doc));
            }
        }
        // Multi-line: collect until closing quote
        let mut parts = vec![rest.to_string()];
        for l in lines {
            let lt = l.trim();
            if lt.contains(quote) {
                if let Some(end) = lt.find(quote) {
                    parts.push(lt[..end].trim().to_string());
                }
                break;
            }
            parts.push(lt.to_string());
        }
        let joined = parts.join(" ").trim().to_string();
        if !joined.is_empty() {
            return Some(first_sentence(&joined));
        }
    }
    None
}

fn extract_js_doc(content: &str) -> Option<String> {
    // Look for /** ... */ at top of file
    let text = content.trim_start();
    if !text.starts_with("/**") {
        return None;
    }
    let end = text.find("*/")?;
    let block = &text[3..end];
    let cleaned: String = block
        .lines()
        .map(|l| l.trim().trim_start_matches('*').trim().to_string())
        .filter(|l| !l.starts_with('@') && !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if cleaned.is_empty() {
        None
    } else {
        Some(first_sentence(&cleaned))
    }
}

fn infer_from_path_and_symbols(path: &str, symbols: &[RepoSymbol]) -> String {
    let norm = path.replace('\\', "/");
    let parts: Vec<&str> = norm.split('/').collect();
    let filename = parts.last().copied().unwrap_or("");
    let stem = filename.split('.').next().unwrap_or(filename);
    let dir = parts.iter().rev().nth(1).copied().unwrap_or("");

    // Test file
    if stem.starts_with("test")
        || stem.ends_with("_test")
        || filename.contains(".test.")
        || filename.contains(".spec.")
    {
        return format!("Tests for {} module.", dir_to_phrase(dir));
    }

    // Common stem patterns
    let role = match stem {
        "mod" | "index" | "main" | "lib" => dir_to_phrase(dir),
        s if s.ends_with("_service") || s.ends_with("Service") => {
            format!("{} service orchestration.", stem_to_phrase(s))
        }
        s if s.ends_with("_repository") || s.ends_with("_repo") || s.ends_with("Repository") => {
            format!("{} data access and persistence.", stem_to_phrase(s))
        }
        s if s.ends_with("_handler") || s.ends_with("Handler") || s.ends_with("_controller") => {
            format!("{} request handling.", stem_to_phrase(s))
        }
        s if s.ends_with("_router") || s.ends_with("Router") || s.ends_with("router") => {
            format!("{} route definitions.", stem_to_phrase(s))
        }
        s if s.ends_with("_model") || s.ends_with("Model") => {
            format!("{} data model.", stem_to_phrase(s))
        }
        s if s.ends_with("_types") || s.ends_with("Types") || s.ends_with("_schema") => {
            format!("{} type definitions.", stem_to_phrase(s))
        }
        s if s.ends_with("_utils") || s.ends_with("Utils") || s.ends_with("_helpers") => {
            format!("{} utility functions.", stem_to_phrase(s))
        }
        s if s.ends_with("_migrations") || s.ends_with("migration") => {
            "Database schema migrations.".to_string()
        }
        s => stem_to_phrase(s),
    };

    // Append top symbol names if short
    if !symbols.is_empty() && role.len() < 60 {
        let names: Vec<&str> = symbols.iter().take(3).map(|s| s.name.as_str()).collect();
        format!("{} — {}", role, names.join(", "))
    } else {
        role
    }
}

fn dir_to_phrase(dir: &str) -> String {
    if dir.is_empty() {
        return "Root module.".to_string();
    }
    let words: Vec<String> = dir
        .split(['_', '-'])
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
            }
        })
        .collect();
    format!("{} module.", words.join(" "))
}

fn stem_to_phrase(stem: &str) -> String {
    let clean = stem.replace(['_', '-'], " ");
    // Title case
    clean
        .split_whitespace()
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn first_sentence(text: &str) -> String {
    // Split at first '.', '!', or '?' that is followed by whitespace or end of string
    let chars: Vec<char> = text.chars().collect();
    let mut end = chars.len();
    for i in 0..chars.len() {
        if matches!(chars[i], '.' | '!' | '?') {
            let next_is_end_or_space = i + 1 >= chars.len() || chars[i + 1].is_whitespace();
            if next_is_end_or_space {
                end = i + 1;
                break;
            }
        }
    }
    chars[..end].iter().collect::<String>().trim().to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", truncated.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rust_doc() {
        let content = "//! This module handles authentication.\n//! It also manages sessions.\npub fn login() {}";
        let result = extract_rust_doc(content);
        assert!(result.is_some());
        assert!(result.unwrap().contains("authentication"));
    }

    #[test]
    fn infers_service_role() {
        let result = infer_from_path_and_symbols("src/pricing/pricing_service.rs", &[]);
        assert!(
            result.contains("service") || result.contains("Service") || result.contains("Pricing")
        );
    }

    #[test]
    fn truncates_long_text() {
        let long = "a".repeat(200);
        let result = truncate(&long, 140);
        assert!(result.chars().count() <= 141); // 140 + ellipsis
    }
}
