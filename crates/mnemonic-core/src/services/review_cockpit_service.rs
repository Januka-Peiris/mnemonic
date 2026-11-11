use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::models::{
    AgentPromptEntry, MarkWorkspaceFileReviewedInput, QueueAgentPromptInput,
    QueueReviewAgentPromptInput, ReviewCockpitFile, WorkspacePrComment, WorkspaceReviewCockpit,
};
use crate::repositories::{review_cockpit_repository, workspace_repository};
use crate::services::{
    agent_context_service, environment_service, git_review_service, merge_readiness_service,
    review_summary_service, terminal_service,
};
use crate::state::AppState;

pub fn get_workspace_review_cockpit(
    state: &AppState,
    workspace_id: &str,
) -> Result<WorkspaceReviewCockpit, String> {
    build_cockpit(state, workspace_id, None, false)
}

pub fn refresh_workspace_review_cockpit(
    state: &AppState,
    workspace_id: &str,
) -> Result<WorkspaceReviewCockpit, String> {
    build_cockpit(state, workspace_id, None, true)
}

pub fn get_workspace_review_cockpit_for_path(
    state: &AppState,
    workspace_id: &str,
    path: Option<&str>,
) -> Result<WorkspaceReviewCockpit, String> {
    build_cockpit(state, workspace_id, path, false)
}

pub fn mark_workspace_file_reviewed(
    state: &AppState,
    input: MarkWorkspaceFileReviewedInput,
) -> Result<WorkspaceReviewCockpit, String> {
    workspace_repository::get_detail(&state.db, &input.workspace_id)?
        .ok_or_else(|| format!("Workspace {} was not found", input.workspace_id))?;
    let content_hash = if input.reviewed {
        let root = workspace_root(state, &input.workspace_id)?;
        Some(file_content_hash(&root, &input.path))
    } else {
        None
    };
    review_cockpit_repository::set_file_reviewed(
        &state.db,
        &input.workspace_id,
        &input.path,
        input.reviewed,
        &timestamp(),
        input.notes.as_deref(),
        content_hash.as_deref(),
    )?;
    build_cockpit(state, &input.workspace_id, Some(&input.path), false)
}

/// Marks a changed file viewed (or not) against its current content, without
/// rebuilding the review cockpit.
pub fn set_workspace_file_viewed(
    state: &AppState,
    workspace_id: &str,
    path: &str,
    viewed: bool,
) -> Result<(), String> {
    let content_hash = if viewed {
        let root = workspace_root(state, workspace_id)?;
        Some(file_content_hash(&root, path))
    } else {
        None
    };
    review_cockpit_repository::set_file_reviewed(
        &state.db,
        workspace_id,
        path,
        viewed,
        &timestamp(),
        None,
        content_hash.as_deref(),
    )
}

/// Which of `changed_paths` are marked viewed and unchanged since the mark.
/// Only marked files are hashed.
pub fn list_workspace_viewed_files(
    state: &AppState,
    workspace_id: &str,
    changed_paths: &[String],
) -> Result<HashSet<String>, String> {
    let root = workspace_root(state, workspace_id)?;
    let changed_paths = changed_paths
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let review_states =
        review_cockpit_repository::list_file_review_states(&state.db, workspace_id)?;
    Ok(review_states
        .into_iter()
        .filter(|review| {
            review.status == "reviewed" && changed_paths.contains(review.path.as_str())
        })
        .filter(|review| {
            review.content_hash.as_deref() == Some(file_content_hash(&root, &review.path).as_str())
        })
        .map(|review| review.path)
        .collect())
}

fn file_content_hash(root: &Path, path: &str) -> String {
    match std::fs::read(root.join(path)) {
        Ok(bytes) => format!("sha256:{:x}", Sha256::digest(&bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "deleted".to_string(),
        Err(error) => format!("unreadable:{:?}", error.kind()),
    }
}

pub fn refresh_workspace_pr_comments(
    state: &AppState,
    workspace_id: &str,
) -> Result<WorkspaceReviewCockpit, String> {
    let mut cockpit = build_cockpit(state, workspace_id, None, false)?;
    match fetch_github_pr_comments(state, workspace_id) {
        Ok((comments, warning)) => {
            review_cockpit_repository::upsert_pr_comments(&state.db, workspace_id, &comments)?;
            cockpit.pr_comments =
                review_cockpit_repository::list_pr_comments(&state.db, workspace_id)?;
            if let Some(warning) = warning {
                cockpit.warnings.push(warning);
            }
        }
        Err(err) => cockpit.warnings.push(err),
    }
    Ok(cockpit)
}

pub fn mark_workspace_pr_comment_resolved_local(
    state: &AppState,
    workspace_id: &str,
    comment_id: &str,
) -> Result<WorkspaceReviewCockpit, String> {
    review_cockpit_repository::mark_pr_comment_resolved_local(
        &state.db,
        workspace_id,
        comment_id,
        &timestamp(),
    )?;
    build_cockpit(state, workspace_id, None, false)
}

pub fn resolve_workspace_pr_thread(
    state: &AppState,
    workspace_id: &str,
    comment_id: &str,
) -> Result<WorkspaceReviewCockpit, String> {
    let comment = review_cockpit_repository::get_pr_comment(&state.db, workspace_id, comment_id)?
        .ok_or_else(|| format!("PR comment {comment_id} was not found"))?;
    let thread_id = comment.thread_id.ok_or_else(|| {
        "This comment is not part of a resolvable GitHub review thread.".to_string()
    })?;
    run_thread_mutation(state, workspace_id, &thread_id, "resolveReviewThread")?;
    refresh_workspace_pr_comments(state, workspace_id)
}

pub fn reopen_workspace_pr_thread(
    state: &AppState,
    workspace_id: &str,
    comment_id: &str,
) -> Result<WorkspaceReviewCockpit, String> {
    let comment = review_cockpit_repository::get_pr_comment(&state.db, workspace_id, comment_id)?
        .ok_or_else(|| format!("PR comment {comment_id} was not found"))?;
    let thread_id = comment.thread_id.ok_or_else(|| {
        "This comment is not part of a resolvable GitHub review thread.".to_string()
    })?;
    run_thread_mutation(state, workspace_id, &thread_id, "unresolveReviewThread")?;
    refresh_workspace_pr_comments(state, workspace_id)
}

/// Fetches the unresolved review threads of the PR at `pr_url`, one entry per
/// thread, straight from GitHub GraphQL. Replies are folded into the body.
pub fn list_unresolved_review_threads(
    state: &AppState,
    workspace_id: &str,
    pr_url: &str,
) -> Result<Vec<WorkspacePrComment>, String> {
    let (owner, repo, number) = parse_pr_url(pr_url)
        .ok_or_else(|| format!("Could not read owner/repo/number from PR URL {pr_url}"))?;
    let root = workspace_root(state, workspace_id)?;
    let gh = find_gh_binary()?;
    let query = r#"
query($owner: String!, $repo: String!, $number: Int!) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      reviewThreads(first: 100) {
        nodes {
          id
          isResolved
          isOutdated
          viewerCanResolve
          path
          line
          originalLine
          comments(first: 50) {
            nodes {
              id
              body
              url
              createdAt
              author { login }
            }
          }
        }
      }
    }
  }
}
"#;
    let output = Command::new(&gh)
        .current_dir(&root)
        .arg("api")
        .arg("graphql")
        .arg("-f")
        .arg(format!("query={query}"))
        .arg("-f")
        .arg(format!("owner={owner}"))
        .arg("-f")
        .arg(format!("repo={repo}"))
        .arg("-F")
        .arg(format!("number={number}"))
        .output()
        .map_err(|err| format!("Failed to run gh api graphql: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            "Failed to fetch GitHub review threads.".to_string()
        } else {
            stderr
        });
    }
    let value = serde_json::from_slice::<Value>(&output.stdout)
        .map_err(|err| format!("Failed to parse GitHub review threads response: {err}"))?;
    Ok(parse_unresolved_review_threads(workspace_id, &value))
}

/// Resolves a GitHub review thread by its GraphQL node id.
pub fn resolve_review_thread(
    state: &AppState,
    workspace_id: &str,
    thread_id: &str,
) -> Result<(), String> {
    run_thread_mutation(state, workspace_id, thread_id, "resolveReviewThread")
}

pub fn parse_unresolved_review_threads(
    workspace_id: &str,
    value: &Value,
) -> Vec<WorkspacePrComment> {
    let Some(threads) = value
        .pointer("/data/repository/pullRequest/reviewThreads/nodes")
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    threads
        .iter()
        .filter_map(|thread| {
            if thread.get("isResolved").and_then(Value::as_bool) != Some(false) {
                return None;
            }
            let thread_id = thread.get("id")?.as_str()?.to_string();
            let comments = thread.pointer("/comments/nodes")?.as_array()?;
            let (first, replies) = comments.split_first()?;
            let author_of = |comment: &Value| {
                comment
                    .pointer("/author/login")
                    .and_then(Value::as_str)
                    .unwrap_or("ghost")
                    .to_string()
            };
            let body_of = |comment: &Value| {
                comment
                    .get("body")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string()
            };
            let mut body = body_of(first);
            for reply in replies {
                let reply_body = body_of(reply);
                if !reply_body.is_empty() {
                    body.push_str(&format!(
                        "\n\nReply from {}:\n{reply_body}",
                        author_of(reply)
                    ));
                }
            }
            let outdated = thread
                .get("isOutdated")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Some(WorkspacePrComment {
                workspace_id: workspace_id.to_string(),
                provider: "github".to_string(),
                comment_id: thread_id.clone(),
                author: author_of(first),
                body,
                path: thread
                    .get("path")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                line: thread
                    .get("line")
                    .and_then(Value::as_u64)
                    .or_else(|| thread.get("originalLine").and_then(Value::as_u64))
                    .and_then(|line| u32::try_from(line).ok()),
                url: first.get("url").and_then(Value::as_str).map(str::to_string),
                state: "open".to_string(),
                created_at: first
                    .get("createdAt")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                resolved_at: None,
                comment_node_id: first.get("id").and_then(Value::as_str).map(str::to_string),
                thread_id: Some(thread_id),
                review_id: None,
                thread_resolved: false,
                thread_outdated: outdated,
                thread_resolvable: thread
                    .get("viewerCanResolve")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
}

/// Splits `https://<host>/<owner>/<repo>/pull/<number>` into its parts.
fn parse_pr_url(url: &str) -> Option<(String, String, u64)> {
    let path = url.split_once("://").map_or(url, |(_, rest)| rest);
    let segments = path
        .split(['/', '?', '#'])
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let pull_index = segments.iter().position(|segment| *segment == "pull")?;
    // Segment 0 is the host, so owner and repo need indices 1 and 2 at least.
    if pull_index < 3 {
        return None;
    }
    let owner = segments.get(pull_index - 2)?;
    let repo = segments.get(pull_index - 1)?;
    let number = segments.get(pull_index + 1)?.parse::<u64>().ok()?;
    Some((owner.to_string(), repo.to_string(), number))
}

pub fn queue_review_agent_prompt(
    state: &AppState,
    input: QueueReviewAgentPromptInput,
) -> Result<AgentPromptEntry, String> {
    let prompt = build_review_prompt(state, &input)?;
    terminal_service::queue_workspace_agent_prompt(
        state,
        QueueAgentPromptInput {
            session_id: None,
            workspace_id: input.workspace_id,
            prompt,
            profile: None,
            profile_id: input.profile_id,
            extra_args: None,
        },
    )
}

fn build_cockpit(
    state: &AppState,
    workspace_id: &str,
    selected_path: Option<&str>,
    refresh: bool,
) -> Result<WorkspaceReviewCockpit, String> {
    let started = Instant::now();
    workspace_repository::get_detail(&state.db, workspace_id)?
        .ok_or_else(|| format!("Workspace {workspace_id} was not found"))?;
    let changed_files = git_review_service::get_workspace_changed_files(state, workspace_id)?;
    let review_states =
        review_cockpit_repository::list_file_review_states(&state.db, workspace_id)?;
    let by_path = review_states
        .into_iter()
        .map(|state| (state.path.clone(), state))
        .collect::<HashMap<_, _>>();
    let files = changed_files
        .iter()
        .cloned()
        .map(|file| ReviewCockpitFile {
            review: by_path.get(&file.path).cloned(),
            file,
        })
        .collect::<Vec<_>>();
    let selected = selected_path
        .map(str::to_string)
        .or_else(|| changed_files.first().map(|file| file.path.clone()));
    let selected_diff = selected.as_deref().and_then(|path| {
        git_review_service::get_workspace_file_diff(state, workspace_id, path).ok()
    });
    let review_summary = if refresh {
        review_summary_service::refresh_workspace_review_summary(state, workspace_id).ok()
    } else {
        review_summary_service::get_workspace_review_summary(state, workspace_id).ok()
    };
    let merge_readiness = if refresh {
        merge_readiness_service::refresh_workspace_merge_readiness(state, workspace_id).ok()
    } else {
        merge_readiness_service::get_workspace_merge_readiness(state, workspace_id).ok()
    };
    let pr_comments = review_cockpit_repository::list_pr_comments(&state.db, workspace_id)?;
    let cockpit = WorkspaceReviewCockpit {
        workspace_id: workspace_id.to_string(),
        files,
        selected_diff,
        review_summary,
        merge_readiness,
        pr_comments,
        warnings: Vec::new(),
    };
    log::debug!(
        target: "mnemonic_lib",
        "build_review_cockpit workspace={} files={} comments={} refresh={} elapsed_ms={}",
        workspace_id,
        cockpit.files.len(),
        cockpit.pr_comments.len(),
        refresh,
        started.elapsed().as_millis()
    );
    Ok(cockpit)
}

fn fetch_github_pr_comments(
    state: &AppState,
    workspace_id: &str,
) -> Result<(Vec<WorkspacePrComment>, Option<String>), String> {
    let root = workspace_root(state, workspace_id)?;
    let gh = find_gh_binary()?;
    let auth = Command::new(&gh).args(["auth", "status"]).output();
    if !auth.map(|output| output.status.success()).unwrap_or(false) {
        return Ok((
            Vec::new(),
            Some("GitHub CLI is not authenticated. Run: gh auth login".to_string()),
        ));
    }

    let pr_output = Command::new(&gh)
        .current_dir(&root)
        .args(["pr", "view", "--json", "number,url,comments,reviews"])
        .output()
        .map_err(|err| format!("Failed to run gh pr view: {err}"))?;
    if !pr_output.status.success() {
        return Ok((
            Vec::new(),
            Some("No GitHub PR found for this branch.".to_string()),
        ));
    }
    let pr_json = String::from_utf8_lossy(&pr_output.stdout);
    let value = serde_json::from_str::<Value>(&pr_json)
        .map_err(|err| format!("Failed to parse gh pr view output: {err}"))?;
    let number = value.get("number").and_then(Value::as_i64).unwrap_or(0);
    let mut comments = parse_pr_view_comments(workspace_id, &value);

    if number > 0 {
        let mut thread_by_comment_node_id = HashMap::<String, ThreadInfo>::new();
        if let Some((owner, repo)) = git_remote_owner_repo(&root) {
            if let Ok(threads) = fetch_review_threads(&gh, &root, &owner, &repo, number as u64) {
                thread_by_comment_node_id = threads;
            }
            let endpoint = format!("repos/{owner}/{repo}/pulls/{number}/comments");
            let output = Command::new(&gh)
                .current_dir(&root)
                .args(["api", &endpoint])
                .output()
                .map_err(|err| format!("Failed to run gh api for PR comments: {err}"))?;
            if output.status.success() {
                let json = String::from_utf8_lossy(&output.stdout);
                if let Ok(value) = serde_json::from_str::<Value>(&json) {
                    comments.extend(parse_inline_comments(workspace_id, &value));
                }
            }
        }
        attach_thread_metadata(&mut comments, &thread_by_comment_node_id);
    }
    dedupe_comments(&mut comments);
    Ok((comments, None))
}

#[derive(Debug, Clone)]
struct ThreadInfo {
    id: String,
    review_id: Option<u64>,
    resolved: bool,
    outdated: bool,
    resolvable: bool,
}

pub fn parse_pr_view_comments(workspace_id: &str, value: &Value) -> Vec<WorkspacePrComment> {
    let mut comments = Vec::new();
    if let Some(items) = value.get("comments").and_then(Value::as_array) {
        for item in items {
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .or_else(|| item.get("url").and_then(Value::as_str))
                .unwrap_or("comment");
            let author = item
                .get("author")
                .and_then(|author| author.get("login"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let body = item.get("body").and_then(Value::as_str).unwrap_or("");
            if body.trim().is_empty() {
                continue;
            }
            comments.push(WorkspacePrComment {
                workspace_id: workspace_id.to_string(),
                provider: "github".to_string(),
                comment_id: format!("issue-{id}"),
                author: author.to_string(),
                body: body.to_string(),
                path: None,
                line: None,
                url: item.get("url").and_then(Value::as_str).map(str::to_string),
                state: "open".to_string(),
                created_at: item
                    .get("createdAt")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                resolved_at: None,
                comment_node_id: item.get("id").and_then(Value::as_str).map(str::to_string),
                thread_id: None,
                review_id: None,
                thread_resolved: false,
                thread_outdated: false,
                thread_resolvable: false,
            });
        }
    }
    if let Some(items) = value.get("reviews").and_then(Value::as_array) {
        for item in items {
            let body = item.get("body").and_then(Value::as_str).unwrap_or("");
            if body.trim().is_empty() {
                continue;
            }
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .or_else(|| item.get("url").and_then(Value::as_str))
                .unwrap_or("review");
            let author = item
                .get("author")
                .and_then(|author| author.get("login"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            comments.push(WorkspacePrComment {
                workspace_id: workspace_id.to_string(),
                provider: "github".to_string(),
                comment_id: format!("review-{id}"),
                author: author.to_string(),
                body: body.to_string(),
                path: None,
                line: None,
                url: item.get("url").and_then(Value::as_str).map(str::to_string),
                state: "open".to_string(),
                created_at: item
                    .get("submittedAt")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                resolved_at: None,
                comment_node_id: item.get("id").and_then(Value::as_str).map(str::to_string),
                thread_id: None,
                review_id: None,
                thread_resolved: false,
                thread_outdated: false,
                thread_resolvable: false,
            });
        }
    }
    comments
}

pub fn parse_inline_comments(workspace_id: &str, value: &Value) -> Vec<WorkspacePrComment> {
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let body = item.get("body")?.as_str()?.to_string();
            if body.trim().is_empty() {
                return None;
            }
            let id = item
                .get("id")
                .and_then(Value::as_i64)
                .map(|id| id.to_string())
                .or_else(|| {
                    item.get("node_id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| format!("inline-{}", body.len()));
            Some(WorkspacePrComment {
                workspace_id: workspace_id.to_string(),
                provider: "github".to_string(),
                comment_id: format!("inline-{id}"),
                author: item
                    .get("user")
                    .and_then(|user| user.get("login"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                body,
                path: item.get("path").and_then(Value::as_str).map(str::to_string),
                line: item
                    .get("line")
                    .or_else(|| item.get("original_line"))
                    .and_then(Value::as_u64)
                    .map(|line| line as u32),
                url: item
                    .get("html_url")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                state: "open".to_string(),
                created_at: item
                    .get("created_at")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                resolved_at: None,
                comment_node_id: item
                    .get("node_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                thread_id: None,
                review_id: item.get("pull_request_review_id").and_then(Value::as_u64),
                thread_resolved: false,
                thread_outdated: false,
                thread_resolvable: false,
            })
        })
        .collect()
}

fn fetch_review_threads(
    gh: &str,
    root: &Path,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<HashMap<String, ThreadInfo>, String> {
    let query = r#"
query($owner: String!, $repo: String!, $number: Int!) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      reviewThreads(first: 100) {
        nodes {
          id
          isResolved
          isOutdated
          comments(first: 100) {
            nodes {
              id
              pullRequestReview { databaseId }
            }
          }
        }
      }
    }
  }
}
"#;
    let output = Command::new(gh)
        .current_dir(root)
        .arg("api")
        .arg("graphql")
        .arg("-f")
        .arg(format!("query={query}"))
        .arg("-F")
        .arg(format!("owner={owner}"))
        .arg("-F")
        .arg(format!("repo={repo}"))
        .arg("-F")
        .arg(format!("number={number}"))
        .output()
        .map_err(|err| format!("Failed to fetch GitHub review threads: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            "Failed to fetch GitHub review threads.".to_string()
        } else {
            stderr
        });
    }
    let parsed = serde_json::from_slice::<Value>(&output.stdout)
        .map_err(|err| format!("Failed to parse GitHub review threads response: {err}"))?;
    let nodes = parsed
        .get("data")
        .and_then(|d| d.get("repository"))
        .and_then(|d| d.get("pullRequest"))
        .and_then(|d| d.get("reviewThreads"))
        .and_then(|d| d.get("nodes"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut by_comment_node_id = HashMap::new();
    for node in nodes {
        let thread_id = node
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if thread_id.is_empty() {
            continue;
        }
        let resolved = node
            .get("isResolved")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let outdated = node
            .get("isOutdated")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let comment_nodes = node
            .get("comments")
            .and_then(|d| d.get("nodes"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for comment_node in comment_nodes {
            let comment_node_id = comment_node
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if comment_node_id.is_empty() {
                continue;
            }
            let review_id = comment_node
                .get("pullRequestReview")
                .and_then(|d| d.get("databaseId"))
                .and_then(Value::as_u64);
            by_comment_node_id.insert(
                comment_node_id,
                ThreadInfo {
                    id: thread_id.clone(),
                    review_id,
                    resolved,
                    outdated,
                    resolvable: !outdated,
                },
            );
        }
    }
    Ok(by_comment_node_id)
}

fn attach_thread_metadata(
    comments: &mut [WorkspacePrComment],
    thread_by_comment_node_id: &HashMap<String, ThreadInfo>,
) {
    for comment in comments.iter_mut() {
        let Some(comment_node_id) = comment.comment_node_id.as_deref() else {
            continue;
        };
        let Some(thread) = thread_by_comment_node_id.get(comment_node_id) else {
            continue;
        };
        comment.thread_id = Some(thread.id.clone());
        comment.review_id = thread.review_id;
        comment.thread_resolved = thread.resolved;
        comment.thread_outdated = thread.outdated;
        comment.thread_resolvable = thread.resolvable;
        if thread.resolved && comment.state == "open" {
            comment.state = "resolved".to_string();
        }
    }
}

fn run_thread_mutation(
    state: &AppState,
    workspace_id: &str,
    thread_id: &str,
    mutation_name: &str,
) -> Result<(), String> {
    let root = workspace_root(state, workspace_id)?;
    let gh = find_gh_binary()?;
    let query = format!(
        "mutation($threadId: ID!) {{ {mutation_name}(input: {{threadId: $threadId}}) {{ thread {{ id isResolved }} }} }}"
    );
    let output = Command::new(&gh)
        .current_dir(&root)
        .arg("api")
        .arg("graphql")
        .arg("-f")
        .arg(format!("query={query}"))
        .arg("-f")
        .arg(format!("threadId={thread_id}"))
        .output()
        .map_err(|err| format!("Failed to run GitHub {mutation_name}: {err}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        format!("GitHub {mutation_name} failed.")
    } else {
        stderr
    })
}

fn dedupe_comments(comments: &mut Vec<WorkspacePrComment>) {
    let mut seen = HashSet::new();
    comments.retain(|comment| seen.insert(comment.comment_id.clone()));
}

fn build_review_prompt(
    state: &AppState,
    input: &QueueReviewAgentPromptInput,
) -> Result<String, String> {
    let detail = workspace_repository::get_detail(&state.db, &input.workspace_id)?
        .ok_or_else(|| format!("Workspace {} was not found", input.workspace_id))?;
    let root = detail
        .summary
        .workspace_root_path
        .clone()
        .unwrap_or(detail.worktree_path);
    let mut sections = Vec::new();
    sections.push("Mnemonic review task".to_string());
    sections.push(format!("Primary writable workspace: {root}"));
    if let Ok(context) =
        agent_context_service::get_workspace_agent_context(state, &input.workspace_id)
    {
        if !context.prompt_preamble.trim().is_empty() {
            sections.push(context.prompt_preamble);
        }
    }
    sections.push("Modify only the primary writable workspace unless I explicitly ask you to edit linked repositories.".to_string());
    sections.push(match input.action.as_str() {
        "fix_file" => "Task: fix the selected file based on the diff below.".to_string(),
        "explain_diff" => "Task: explain the selected diff and call out review risks. Do not edit files unless necessary.".to_string(),
        "address_comment" => "Task: address the selected PR review comment.".to_string(),
        "prepare_pr_summary" => "Task: prepare a concise PR summary from the accepted/reviewed files and current diff.".to_string(),
        other => format!("Task: {other}"),
    });
    if let Some(path) = input.path.as_deref() {
        sections.push(format!("Selected file: {path}"));
        if let Ok(diff) =
            git_review_service::get_workspace_file_diff(state, &input.workspace_id, path)
        {
            sections.push(format!(
                "Diff:\n```diff\n{}\n```",
                truncate(&diff.diff, 20000)
            ));
        }
    }
    if let Some(comment_id) = input.comment_id.as_deref() {
        if let Some(comment) =
            review_cockpit_repository::get_pr_comment(&state.db, &input.workspace_id, comment_id)?
        {
            sections.push(format!(
                "PR comment from {}{}:\n{}",
                comment.author,
                comment
                    .path
                    .as_deref()
                    .map(|path| format!(
                        " on {path}{}",
                        comment
                            .line
                            .map(|line| format!(":{line}"))
                            .unwrap_or_default()
                    ))
                    .unwrap_or_default(),
                truncate(&comment.body, 12000)
            ));
        }
    }
    Ok(sections.join("\n\n"))
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value.to_string()
    } else {
        format!(
            "{}\n...[truncated]",
            value.chars().take(max_chars).collect::<String>()
        )
    }
}

fn workspace_root(state: &AppState, workspace_id: &str) -> Result<PathBuf, String> {
    let detail = workspace_repository::get_detail(&state.db, workspace_id)?
        .ok_or_else(|| format!("Workspace {workspace_id} was not found"))?;
    let root = detail
        .summary
        .workspace_root_path
        .clone()
        .unwrap_or(detail.worktree_path);
    Ok(PathBuf::from(root))
}

fn find_gh_binary() -> Result<String, String> {
    environment_service::find_gh_path().ok_or_else(|| {
        "GitHub CLI is required for PR comments. Install with: brew install gh".to_string()
    })
}

fn git_remote_owner_repo(root: &Path) -> Option<(String, String)> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_github_remote(String::from_utf8_lossy(&output.stdout).trim())
}

fn parse_github_remote(remote: &str) -> Option<(String, String)> {
    let trimmed = remote.trim().trim_end_matches(".git");
    if let Some(rest) = trimmed.strip_prefix("git@github.com:") {
        let (owner, repo) = rest.split_once('/')?;
        return Some((owner.to_string(), repo.to_string()));
    }
    if let Some(rest) = trimmed.strip_prefix("https://github.com/") {
        let (owner, repo) = rest.split_once('/')?;
        return Some((owner.to_string(), repo.to_string()));
    }
    None
}

fn timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_inline_github_comments() {
        let value = serde_json::json!([{
            "id": 42,
            "body": "Please fix this",
            "path": "src/lib.rs",
            "line": 12,
            "html_url": "https://example.test/comment",
            "created_at": "now",
            "user": { "login": "greptile" }
        }]);
        let comments = parse_inline_comments("ws", &value);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].path.as_deref(), Some("src/lib.rs"));
        assert_eq!(comments[0].line, Some(12));
        assert_eq!(comments[0].author, "greptile");
    }

    #[test]
    fn parses_issue_and_review_comments() {
        let value = serde_json::json!({
            "comments": [{ "id": "c1", "body": "general", "author": { "login": "alice" }, "url": "u", "createdAt": "t" }],
            "reviews": [{ "id": "r1", "body": "review", "author": { "login": "bot" }, "url": "r", "submittedAt": "t" }]
        });
        let comments = parse_pr_view_comments("ws", &value);
        assert_eq!(comments.len(), 2);
        assert!(comments.iter().any(|comment| comment.author == "alice"));
        assert!(comments.iter().any(|comment| comment.author == "bot"));
    }

    #[test]
    fn parses_unresolved_review_threads_from_graphql() {
        let value = serde_json::json!({
            "data": { "repository": { "pullRequest": { "reviewThreads": { "nodes": [
                {
                    "id": "PRRT_open",
                    "isResolved": false,
                    "isOutdated": false,
                    "viewerCanResolve": true,
                    "path": "src/lib.rs",
                    "line": 12,
                    "originalLine": 10,
                    "comments": { "nodes": [
                        { "id": "PRRC_1", "body": "Handle the error here", "url": "https://github.com/o/r/pull/7#discussion_r1", "createdAt": "2026-11-01T00:00:00Z", "author": { "login": "greptile-apps" } },
                        { "id": "PRRC_2", "body": "Agreed", "url": "u2", "createdAt": "2026-11-01T01:00:00Z", "author": null }
                    ]}
                },
                {
                    "id": "PRRT_outdated",
                    "isResolved": false,
                    "isOutdated": true,
                    "viewerCanResolve": true,
                    "path": "src/old.rs",
                    "line": null,
                    "originalLine": 40,
                    "comments": { "nodes": [
                        { "id": "PRRC_3", "body": "Rename this", "url": "u3", "createdAt": "t", "author": { "login": "alice" } }
                    ]}
                },
                {
                    "id": "PRRT_done",
                    "isResolved": true,
                    "isOutdated": false,
                    "viewerCanResolve": true,
                    "path": "src/main.rs",
                    "line": 3,
                    "originalLine": 3,
                    "comments": { "nodes": [
                        { "id": "PRRC_4", "body": "Fixed", "url": "u4", "createdAt": "t", "author": { "login": "bob" } }
                    ]}
                }
            ]}}}}
        });
        let threads = parse_unresolved_review_threads("ws", &value);
        assert_eq!(threads.len(), 2);
        assert_eq!(threads[0].thread_id.as_deref(), Some("PRRT_open"));
        assert_eq!(threads[0].author, "greptile-apps");
        assert_eq!(threads[0].path.as_deref(), Some("src/lib.rs"));
        assert_eq!(threads[0].line, Some(12));
        assert!(threads[0].body.starts_with("Handle the error here"));
        assert!(threads[0].body.contains("Reply from ghost:\nAgreed"));
        assert!(threads[0].thread_resolvable);
        assert_eq!(threads[1].line, Some(40));
        assert!(threads[1].thread_outdated);
    }

    #[test]
    fn parses_pr_urls() {
        assert_eq!(
            parse_pr_url("https://github.com/owner/repo/pull/123"),
            Some(("owner".to_string(), "repo".to_string(), 123))
        );
        assert_eq!(
            parse_pr_url("https://ghe.example.com/team/app/pull/9#discussion"),
            Some(("team".to_string(), "app".to_string(), 9))
        );
        assert_eq!(parse_pr_url("https://github.com/owner/repo"), None);
    }

    #[test]
    fn parses_github_remotes() {
        assert_eq!(
            parse_github_remote("git@github.com:owner/repo.git"),
            Some(("owner".to_string(), "repo".to_string()))
        );
        assert_eq!(
            parse_github_remote("https://github.com/owner/repo.git"),
            Some(("owner".to_string(), "repo".to_string()))
        );
    }

    #[test]
    fn attaches_thread_metadata_to_inline_comments() {
        let mut comments = vec![WorkspacePrComment {
            workspace_id: "ws".to_string(),
            provider: "github".to_string(),
            comment_id: "inline-1".to_string(),
            author: "bot".to_string(),
            body: "body".to_string(),
            path: Some("src/lib.rs".to_string()),
            line: Some(7),
            url: None,
            state: "open".to_string(),
            created_at: None,
            resolved_at: None,
            comment_node_id: Some("node-1".to_string()),
            thread_id: None,
            review_id: None,
            thread_resolved: false,
            thread_outdated: false,
            thread_resolvable: false,
        }];
        let mut map = HashMap::new();
        map.insert(
            "node-1".to_string(),
            ThreadInfo {
                id: "thread-1".to_string(),
                review_id: Some(42),
                resolved: true,
                outdated: false,
                resolvable: true,
            },
        );
        attach_thread_metadata(&mut comments, &map);
        assert_eq!(comments[0].thread_id.as_deref(), Some("thread-1"));
        assert!(comments[0].thread_resolved);
        assert_eq!(comments[0].state, "resolved");
    }

    #[test]
    fn viewed_mark_clears_when_file_content_changes() {
        let root = std::env::temp_dir().join(format!(
            "mnemonic-core-viewed-{}-{}",
            std::process::id(),
            timestamp()
        ));
        std::fs::create_dir_all(&root).expect("worktree dir");
        let state = AppState::initialize(
            root.join("data"),
            root.join("cache"),
            std::sync::Arc::new(crate::state::NoopEventEmitter),
        )
        .expect("app state");
        let worktree = root.join("worktree");
        std::fs::create_dir_all(&worktree).expect("worktree");
        std::fs::write(worktree.join("lib.rs"), "fn first() {}").expect("write");
        state
            .db
            .with_connection(|connection| {
                connection.execute(
                    "INSERT INTO workspaces (id, name, repo, branch, agent, status, current_step, completed_steps, last_updated, description, current_task, merge_risk, last_rebase, base_branch, agent_session_id, agent_session_agent, agent_session_status, agent_session_model, agent_session_estimated_cost, agent_session_last_message, agent_session_started_at, worktree_path, recent_events) VALUES ('ws', 'WS', 'repo', 'b', 'Codex', 'Waiting', 'Planning', '[]', 'now', '', '', 'Low', '', 'main', '', '', '', '', '', '', '', ?1, '[]')",
                    [worktree.to_string_lossy().to_string()],
                )?;
                Ok(())
            })
            .expect("workspace");
        let changed = vec!["lib.rs".to_string()];

        set_workspace_file_viewed(&state, "ws", "lib.rs", true).expect("mark viewed");
        assert!(list_workspace_viewed_files(&state, "ws", &changed)
            .expect("viewed")
            .contains("lib.rs"));

        std::fs::write(worktree.join("lib.rs"), "fn second() {}").expect("rewrite");
        assert!(list_workspace_viewed_files(&state, "ws", &changed)
            .expect("viewed")
            .is_empty());

        if let Err(error) = std::fs::remove_dir_all(&root) {
            log::warn!("failed to remove {}: {error}", root.display());
        }
    }
}
