use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::models::{FileReviewInsight, WorkspaceChangedFile, WorkspaceReviewSummary};
use crate::repositories::review_summary_repository;
use crate::services::git_review_service;
use crate::state::AppState;

pub fn get_workspace_review_summary(
    state: &AppState,
    workspace_id: &str,
) -> Result<WorkspaceReviewSummary, String> {
    if let Some(summary) = review_summary_repository::get(&state.db, workspace_id)? {
        return Ok(summary);
    }
    refresh_workspace_review_summary(state, workspace_id)
}

pub fn refresh_workspace_review_summary(
    state: &AppState,
    workspace_id: &str,
) -> Result<WorkspaceReviewSummary, String> {
    let files = git_review_service::get_workspace_changed_files(state, workspace_id)?;
    let summary = generate_summary(state, workspace_id, &files)?;
    review_summary_repository::upsert(&state.db, &summary)?;
    Ok(summary)
}

fn generate_summary(
    _state: &AppState,
    workspace_id: &str,
    files: &[WorkspaceChangedFile],
) -> Result<WorkspaceReviewSummary, String> {
    let files_changed = files.len() as u32;
    let additions = files
        .iter()
        .map(|file| file.additions.unwrap_or(0))
        .sum::<u32>();
    let deletions = files
        .iter()
        .map(|file| file.deletions.unwrap_or(0))
        .sum::<u32>();
    let mut score = 0u32;
    let mut risk_reasons = Vec::new();
    let mut file_insights = Vec::new();
    let mut categories = BTreeSet::new();

    if files_changed == 0 {
        let summary = WorkspaceReviewSummary {
            workspace_id: workspace_id.to_string(),
            summary: "No local git changes were found for this workspace.".to_string(),
            risk_level: "low".to_string(),
            risk_reasons: vec!["No changed files detected.".to_string()],
            files_changed: 0,
            files_flagged: 0,
            additions: 0,
            deletions: 0,
            generated_at: timestamp(),
            file_insights: Vec::new(),
        };
        return Ok(summary);
    }

    if files_changed >= 20 {
        score += 3;
        risk_reasons.push(format!("Large change set: {files_changed} files changed."));
    } else if files_changed >= 8 {
        score += 2;
        risk_reasons.push(format!(
            "Moderate change set: {files_changed} files changed."
        ));
    }

    let churn = additions + deletions;
    if churn >= 1200 {
        score += 3;
        risk_reasons.push(format!("Very large diff: +{additions} -{deletions}."));
    } else if churn >= 400 {
        score += 2;
        risk_reasons.push(format!("Large diff: +{additions} -{deletions}."));
    } else if churn >= 120 {
        score += 1;
        risk_reasons.push(format!("Moderate diff size: +{additions} -{deletions}."));
    }

    for file in files {
        let mut file_score = 0u32;
        let mut reasons = Vec::new();
        let path = file.path.to_lowercase();
        let file_additions = file.additions.unwrap_or(0);
        let file_deletions = file.deletions.unwrap_or(0);
        categories.insert(category_for_path(&path).to_string());

        if matches!(file.status.as_str(), "deleted" | "renamed") {
            file_score += 2;
            reasons.push(format!("File was {}.", file.status));
        }
        if is_config_or_build_file(&path) {
            file_score += 2;
            reasons.push("Touches config/build/package metadata.".to_string());
        }
        if is_backend_runtime_file(&path) {
            file_score += 2;
            reasons.push("Touches backend/runtime/process/git/database code.".to_string());
        }
        if has_security_keyword(&path) {
            file_score += 3;
            reasons.push("Touches auth/security/infrastructure-sensitive path.".to_string());
        }
        if file_additions + file_deletions >= 300 {
            file_score += 2;
            reasons.push(format!(
                "Large file diff: +{file_additions} -{file_deletions}."
            ));
        }

        if file_score > 0 {
            score += file_score;
            file_insights.push(FileReviewInsight {
                path: file.path.clone(),
                status: file.status.clone(),
                risk_level: level_for_score(file_score).to_string(),
                reasons,
                additions: file_additions,
                deletions: file_deletions,
            });
        }
    }

    if risk_reasons.is_empty() {
        risk_reasons.push(
            "Small, localized change set with no high-sensitivity paths detected.".to_string(),
        );
    }

    for insight in file_insights.iter().take(5) {
        if let Some(reason) = insight.reasons.first() {
            risk_reasons.push(format!("{}: {reason}", insight.path));
        }
    }
    risk_reasons.sort();
    risk_reasons.dedup();

    let risk_level = level_for_score(score).to_string();
    let summary_text = build_summary_text(
        &risk_level,
        files_changed,
        additions,
        deletions,
        &categories,
        &file_insights,
    );

    Ok(WorkspaceReviewSummary {
        workspace_id: workspace_id.to_string(),
        summary: summary_text,
        risk_level,
        risk_reasons,
        files_changed,
        files_flagged: file_insights.len() as u32,
        additions,
        deletions,
        generated_at: timestamp(),
        file_insights,
    })
}

fn build_summary_text(
    risk_level: &str,
    files_changed: u32,
    additions: u32,
    deletions: u32,
    categories: &BTreeSet<String>,
    insights: &[FileReviewInsight],
) -> String {
    let categories = if categories.is_empty() {
        "general project files".to_string()
    } else {
        categories.iter().cloned().collect::<Vec<_>>().join(", ")
    };
    let focus = if insights.is_empty() {
        "The main review focus should be verifying the intended behavior and checking for missing tests.".to_string()
    } else {
        let files = insights
            .iter()
            .take(3)
            .map(|insight| insight.path.clone())
            .collect::<Vec<_>>()
            .join(", ");
        format!("The most important files to inspect are: {files}.")
    };

    format!(
        "This workspace changes {files_changed} file(s) across {categories}, with +{additions} additions and -{deletions} deletions. The deterministic review score is {risk_level}. {focus}"
    )
}

fn category_for_path(path: &str) -> &'static str {
    if path.ends_with(".rs") || path.contains("src-tauri") {
        "Rust/backend"
    } else if path.ends_with(".tsx")
        || path.ends_with(".ts")
        || path.ends_with(".jsx")
        || path.ends_with(".js")
    {
        "TypeScript/frontend"
    } else if is_config_or_build_file(path) {
        "configuration/build"
    } else if path.contains("test") || path.contains("spec") {
        "tests"
    } else {
        "project files"
    }
}

fn is_config_or_build_file(path: &str) -> bool {
    path.ends_with("package.json")
        || path.ends_with("package-lock.json")
        || path.ends_with("pnpm-lock.yaml")
        || path.ends_with("cargo.toml")
        || path.ends_with("cargo.lock")
        || path.ends_with("tauri.conf.json")
        || path.contains("vite.config")
        || path.contains("tsconfig")
        || path.contains("docker")
        || path.ends_with(".env")
        || path.ends_with(".yml")
        || path.ends_with(".yaml")
}

fn is_backend_runtime_file(path: &str) -> bool {
    path.contains("src-tauri")
        || path.contains("process")
        || path.contains("runner")
        || path.contains("worktree")
        || path.contains("git")
        || path.contains("db/")
        || path.contains("database")
        || path.contains("migration")
        || path.contains("repository")
        || path.contains("service")
}

fn has_security_keyword(path: &str) -> bool {
    path.contains("auth")
        || path.contains("token")
        || path.contains("secret")
        || path.contains("credential")
        || path.contains("permission")
        || path.contains("security")
        || path.contains("infra")
        || path.contains("deploy")
        || path.contains("ci/")
        || path.contains(".github")
}

fn level_for_score(score: u32) -> &'static str {
    if score >= 8 {
        "high"
    } else if score >= 3 {
        "medium"
    } else {
        "low"
    }
}

fn timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}
