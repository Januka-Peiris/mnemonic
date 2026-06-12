use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::models::{PromptTemplate, WorkspacePromptTemplates};
use crate::repositories::workspace_repository;
use crate::state::AppState;

const PROMPTS_JSON: &str = ".forge/prompts.json";
const PROMPTS_DIR: &str = ".forge/prompts";

pub fn list_workspace_prompt_templates(
    state: &AppState,
    workspace_id: &str,
) -> Result<WorkspacePromptTemplates, String> {
    let root = workspace_root_path(state, workspace_id)?;
    Ok(load_templates_from_root(&root))
}

pub fn load_templates_from_root(root: &Path) -> WorkspacePromptTemplates {
    let mut templates = Vec::new();
    let mut warning = None;

    let json_path = root.join(PROMPTS_JSON);
    if json_path.exists() {
        match load_json_templates(&json_path) {
            Ok(mut json_templates) => templates.append(&mut json_templates),
            Err(err) => warning = Some(err),
        }
    }

    let prompts_dir = root.join(PROMPTS_DIR);
    if prompts_dir.exists() && prompts_dir.is_dir() {
        match load_markdown_templates(&prompts_dir) {
            Ok(mut markdown_templates) => templates.append(&mut markdown_templates),
            Err(err) => {
                warning = Some(match warning {
                    Some(existing) => format!("{existing}; {err}"),
                    None => err,
                })
            }
        }
    }

    templates.sort_by_key(|t| t.title.to_lowercase());
    WorkspacePromptTemplates { templates, warning }
}

fn load_json_templates(path: &Path) -> Result<Vec<PromptTemplate>, String> {
    let text = fs::read_to_string(path)
        .map_err(|err| format!("Could not read .forge/prompts.json: {err}"))?;
    let value = serde_json::from_str::<Value>(&text)
        .map_err(|err| format!("Invalid .forge/prompts.json: {err}"))?;
    let source = path.display().to_string();

    match value {
        Value::Array(items) => Ok(items
            .into_iter()
            .enumerate()
            .filter_map(|(index, item)| match item {
                Value::String(body) => Some(template(
                    format!("json-{index}"),
                    format!("Prompt {}", index + 1),
                    body,
                    source.clone(),
                )),
                Value::Object(mut object) => {
                    let body = object.remove("body").or_else(|| object.remove("prompt"))?;
                    let body = body.as_str()?.to_string();
                    let title = object
                        .remove("title")
                        .and_then(|value| value.as_str().map(ToString::to_string))
                        .unwrap_or_else(|| format!("Prompt {}", index + 1));
                    Some(template(
                        format!("json-{index}"),
                        title,
                        body,
                        source.clone(),
                    ))
                }
                _ => None,
            })
            .collect()),
        Value::Object(object) => Ok(object
            .into_iter()
            .filter_map(|(key, value)| {
                value.as_str().map(|body| {
                    template(
                        format!("json-{}", slug(&key)),
                        title_from_slug(&key),
                        body.to_string(),
                        source.clone(),
                    )
                })
            })
            .collect()),
        _ => Err("Invalid .forge/prompts.json: expected an array or object".to_string()),
    }
}

fn load_markdown_templates(dir: &Path) -> Result<Vec<PromptTemplate>, String> {
    let mut templates = Vec::new();
    let entries =
        fs::read_dir(dir).map_err(|err| format!("Could not read .forge/prompts: {err}"))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("Could not read prompt template entry: {err}"))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let body = fs::read_to_string(&path)
            .map_err(|err| format!("Could not read prompt template {}: {err}", path.display()))?;
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("prompt");
        templates.push(template(
            format!("md-{}", slug(stem)),
            title_from_slug(stem),
            body,
            path.display().to_string(),
        ));
    }
    Ok(templates)
}

fn template(id: String, title: String, body: String, source: String) -> PromptTemplate {
    PromptTemplate {
        id,
        title,
        body,
        source,
    }
}

fn workspace_root_path(state: &AppState, workspace_id: &str) -> Result<PathBuf, String> {
    let workspace = workspace_repository::get_detail(&state.db, workspace_id)?
        .ok_or_else(|| format!("Workspace {workspace_id} was not found"))?;
    let path = workspace
        .summary
        .workspace_root_path
        .clone()
        .unwrap_or_else(|| workspace.worktree_path.clone());
    let path = PathBuf::from(path);
    if !path.exists() || !path.is_dir() {
        return Err(format!(
            "Workspace root path is unavailable: {}",
            path.display()
        ));
    }
    Ok(path)
}

fn title_from_slug(value: &str) -> String {
    let title = value
        .replace(['_', '-'], " ")
        .split_whitespace()
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if title.is_empty() {
        "Prompt".to_string()
    } else {
        title
    }
}

fn slug(value: &str) -> String {
    let slug = value
        .to_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "prompt".to_string()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::terminal_service;

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "forge-prompt-template-test-{name}-{}",
            terminal_service::timestamp()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("temp root");
        root
    }

    #[test]
    fn missing_templates_returns_empty() {
        let dir = temp_root("missing");
        let result = load_templates_from_root(&dir);
        assert!(result.templates.is_empty());
        assert!(result.warning.is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn parses_json_object_templates() {
        let dir = temp_root("json-object");
        fs::create_dir_all(dir.join(".forge")).expect("forge dir");
        fs::write(
            dir.join(PROMPTS_JSON),
            r#"{"fix_tests":"Fix the failing tests.","review_notes":"Summarize the changes."}"#,
        )
        .expect("write prompts");
        let result = load_templates_from_root(&dir);
        assert_eq!(result.templates.len(), 2);
        assert!(result
            .templates
            .iter()
            .any(|item| item.title == "Fix Tests"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn invalid_json_is_warning() {
        let dir = temp_root("invalid-json");
        fs::create_dir_all(dir.join(".forge")).expect("forge dir");
        fs::write(dir.join(PROMPTS_JSON), "{").expect("write prompts");
        let result = load_templates_from_root(&dir);
        assert!(result.templates.is_empty());
        assert!(result
            .warning
            .unwrap()
            .contains("Invalid .forge/prompts.json"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn parses_markdown_templates_with_titles() {
        let dir = temp_root("markdown");
        fs::create_dir_all(dir.join(PROMPTS_DIR)).expect("prompts dir");
        fs::write(
            dir.join(PROMPTS_DIR).join("continue_work.md"),
            "Continue from here.",
        )
        .expect("write md");
        let result = load_templates_from_root(&dir);
        assert_eq!(result.templates.len(), 1);
        assert_eq!(result.templates[0].title, "Continue Work");
        assert_eq!(result.templates[0].body, "Continue from here.");
        let _ = fs::remove_dir_all(dir);
    }
}
