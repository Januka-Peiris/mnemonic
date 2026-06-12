use std::time::{SystemTime, UNIX_EPOCH};

use crate::db::Database;
use crate::models::{
    is_valid_coordination_artifact_kind, is_valid_coordination_artifact_status,
    CoordinationArtifact, CreateCoordinationArtifactInput, UpdateCoordinationArtifactInput,
    UpdateCoordinationArtifactStatusInput,
};
use crate::repositories::{coordination_artifact_repository, workspace_repository};
use crate::state::AppState;

pub fn list_coordination_artifacts(
    state: &AppState,
    workspace_id: &str,
) -> Result<Vec<CoordinationArtifact>, String> {
    list_coordination_artifacts_for_db(&state.db, workspace_id)
}

fn list_coordination_artifacts_for_db(
    db: &Database,
    workspace_id: &str,
) -> Result<Vec<CoordinationArtifact>, String> {
    let parent_workspace_id = resolve_parent_workspace_id(db, workspace_id)?;
    coordination_artifact_repository::list_by_parent_workspace(db, &parent_workspace_id)
}

pub fn create_coordination_artifact(
    state: &AppState,
    input: CreateCoordinationArtifactInput,
) -> Result<CoordinationArtifact, String> {
    create_coordination_artifact_for_db(&state.db, input)
}

fn create_coordination_artifact_for_db(
    db: &Database,
    input: CreateCoordinationArtifactInput,
) -> Result<CoordinationArtifact, String> {
    let source_workspace_id = required(input.source_workspace_id, "Source workspace")?;
    let artifact_kind = required(input.artifact_kind, "Artifact kind")?;
    if !is_valid_coordination_artifact_kind(&artifact_kind) {
        return Err(format!(
            "Unsupported coordination artifact kind `{artifact_kind}`."
        ));
    }
    let title = required(input.title, "Title")?;
    let body = input.body.trim().to_string();
    let status = input.status.unwrap_or_else(|| "active".to_string());
    let status = required(status, "Status")?;
    if !is_valid_coordination_artifact_status(&status) {
        return Err(format!(
            "Unsupported coordination artifact status `{status}`."
        ));
    }

    let parent_workspace_id = resolve_parent_workspace_id(db, &source_workspace_id)?;
    let target_workspace_id = input.target_workspace_id.and_then(|value| {
        let trimmed = value.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    });
    if let Some(target_workspace_id) = target_workspace_id.as_deref() {
        let target_parent_workspace_id = resolve_parent_workspace_id(db, target_workspace_id)?;
        if target_parent_workspace_id != parent_workspace_id {
            return Err("Target workspace must belong to the same federated group.".to_string());
        }
    }

    let id = format!("coord-artifact-{}", timestamp_nanos());
    coordination_artifact_repository::insert(
        db,
        coordination_artifact_repository::CoordinationArtifactInsert {
            id: &id,
            parent_workspace_id: &parent_workspace_id,
            source_workspace_id: &source_workspace_id,
            target_workspace_id: target_workspace_id.as_deref(),
            artifact_kind: &artifact_kind,
            title: &title,
            body: &body,
            status: &status,
        },
    )
}

pub fn update_coordination_artifact(
    state: &AppState,
    input: UpdateCoordinationArtifactInput,
) -> Result<CoordinationArtifact, String> {
    update_coordination_artifact_for_db(&state.db, input)
}

fn update_coordination_artifact_for_db(
    db: &Database,
    input: UpdateCoordinationArtifactInput,
) -> Result<CoordinationArtifact, String> {
    let existing_id = required(input.id, "Artifact")?;
    let existing = coordination_artifact_repository::get(db, &existing_id)?
        .ok_or_else(|| "Coordination artifact not found".to_string())?;
    let artifact_kind = required(input.artifact_kind, "Artifact kind")?;
    if !is_valid_coordination_artifact_kind(&artifact_kind) {
        return Err(format!(
            "Unsupported coordination artifact kind `{artifact_kind}`."
        ));
    }
    let title = required(input.title, "Title")?;
    let body = input.body.trim().to_string();
    let status = required(input.status, "Status")?;
    if !is_valid_coordination_artifact_status(&status) {
        return Err(format!(
            "Unsupported coordination artifact status `{status}`."
        ));
    }
    let target_workspace_id = input.target_workspace_id.and_then(|value| {
        let trimmed = value.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    });
    if let Some(target_workspace_id) = target_workspace_id.as_deref() {
        let target_parent_workspace_id = resolve_parent_workspace_id(db, target_workspace_id)?;
        if target_parent_workspace_id != existing.parent_workspace_id {
            return Err("Target workspace must belong to the same federated group.".to_string());
        }
    }

    coordination_artifact_repository::update(
        db,
        coordination_artifact_repository::CoordinationArtifactUpdate {
            id: &existing_id,
            target_workspace_id: target_workspace_id.as_deref(),
            artifact_kind: &artifact_kind,
            title: &title,
            body: &body,
            status: &status,
        },
    )
}

pub fn delete_coordination_artifact(state: &AppState, id: &str) -> Result<(), String> {
    let artifact_id = required(id.to_string(), "Artifact")?;
    coordination_artifact_repository::delete(&state.db, &artifact_id)
}

pub fn update_coordination_artifact_status(
    state: &AppState,
    input: UpdateCoordinationArtifactStatusInput,
) -> Result<CoordinationArtifact, String> {
    update_coordination_artifact_status_for_db(&state.db, input)
}

fn update_coordination_artifact_status_for_db(
    db: &Database,
    input: UpdateCoordinationArtifactStatusInput,
) -> Result<CoordinationArtifact, String> {
    let id = required(input.id, "Artifact")?;
    let status = required(input.status, "Status")?;
    if !is_valid_coordination_artifact_status(&status) {
        return Err(format!(
            "Unsupported coordination artifact status `{status}`."
        ));
    }
    coordination_artifact_repository::update_status(db, &id, &status)
}

fn resolve_parent_workspace_id(db: &Database, workspace_id: &str) -> Result<String, String> {
    let workspace = workspace_repository::get_detail(db, workspace_id)?
        .ok_or_else(|| format!("Workspace {workspace_id} not found"))?;
    Ok(workspace
        .summary
        .parent_workspace_id
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(|| workspace.summary.id.clone()))
}

fn required(value: String, label: &str) -> Result<String, String> {
    let trimmed = value.trim().to_string();
    if trimmed.is_empty() {
        Err(format!("{label} is required."))
    } else {
        Ok(trimmed)
    }
}

fn timestamp_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use rusqlite::params;

    fn seed_workspace(db: &Database, id: &str, parent_workspace_id: Option<&str>) {
        db
            .with_connection_mut(|connection| {
                connection.execute(
                    r#"
                    INSERT INTO workspaces (
                        id, name, repo, branch, agent, status, current_step, completed_steps,
                        last_updated, description, current_task, merge_risk, last_rebase, base_branch,
                        agent_session_id, agent_session_agent, agent_session_status, agent_session_model,
                        agent_session_estimated_cost, agent_session_last_message, agent_session_started_at,
                        worktree_path, recent_events, parent_workspace_id, source_workspace_id
                    ) VALUES (?1, ?1, 'repo', 'branch', 'Codex', 'Waiting', 'Planning', '[]',
                        'now', '', '', 'Low', '', 'main', '', '', '', '', '', '', '',
                        '/tmp', '[]', ?2, ?3)
                    "#,
                    params![id, parent_workspace_id, parent_workspace_id.unwrap_or(id)],
                )?;
                Ok(())
            })
            .expect("seed workspace");
    }

    #[test]
    fn creates_artifact_with_default_active_status() {
        let db = Database::in_memory().expect("db");
        seed_workspace(&db, "parent", None);

        let artifact = create_coordination_artifact_for_db(
            &db,
            CreateCoordinationArtifactInput {
                source_workspace_id: "parent".to_string(),
                target_workspace_id: None,
                artifact_kind: "decision_summary".to_string(),
                title: "Decision".to_string(),
                body: "Use v2 payload.".to_string(),
                status: None,
            },
        )
        .expect("create");

        assert_eq!(artifact.parent_workspace_id, "parent");
        assert_eq!(artifact.status, "active");
    }

    #[test]
    fn lists_artifacts_from_parent_and_child_workspace_ids() {
        let db = Database::in_memory().expect("db");
        seed_workspace(&db, "parent", None);
        seed_workspace(&db, "child", Some("parent"));

        create_coordination_artifact_for_db(
            &db,
            CreateCoordinationArtifactInput {
                source_workspace_id: "child".to_string(),
                target_workspace_id: Some("parent".to_string()),
                artifact_kind: "api_diff".to_string(),
                title: "API changed".to_string(),
                body: "Update caller.".to_string(),
                status: None,
            },
        )
        .expect("create");

        let from_parent = list_coordination_artifacts_for_db(&db, "parent").expect("parent list");
        let from_child = list_coordination_artifacts_for_db(&db, "child").expect("child list");
        assert_eq!(from_parent.len(), 1);
        assert_eq!(from_child.len(), 1);
        assert_eq!(from_child[0].target_workspace_id.as_deref(), Some("parent"));
    }

    #[test]
    fn updates_artifact_content_and_target() {
        let db = Database::in_memory().expect("db");
        seed_workspace(&db, "parent", None);
        seed_workspace(&db, "child", Some("parent"));
        let artifact = create_coordination_artifact_for_db(
            &db,
            CreateCoordinationArtifactInput {
                source_workspace_id: "parent".to_string(),
                target_workspace_id: None,
                artifact_kind: "decision_summary".to_string(),
                title: "Decision".to_string(),
                body: "Old body.".to_string(),
                status: None,
            },
        )
        .expect("create");

        let updated = update_coordination_artifact_for_db(
            &db,
            UpdateCoordinationArtifactInput {
                id: artifact.id,
                target_workspace_id: Some("child".to_string()),
                artifact_kind: "dependency_note".to_string(),
                title: "Dependency".to_string(),
                body: "New body.".to_string(),
                status: "draft".to_string(),
            },
        )
        .expect("update");
        assert_eq!(updated.target_workspace_id.as_deref(), Some("child"));
        assert_eq!(updated.artifact_kind, "dependency_note");
        assert_eq!(updated.status, "draft");
    }

    #[test]
    fn updates_status_to_resolved_and_dismissed() {
        let db = Database::in_memory().expect("db");
        seed_workspace(&db, "parent", None);
        let artifact = create_coordination_artifact_for_db(
            &db,
            CreateCoordinationArtifactInput {
                source_workspace_id: "parent".to_string(),
                target_workspace_id: None,
                artifact_kind: "release_ordering_note".to_string(),
                title: "Release order".to_string(),
                body: "Backend first.".to_string(),
                status: Some("draft".to_string()),
            },
        )
        .expect("create");

        let resolved = update_coordination_artifact_status_for_db(
            &db,
            UpdateCoordinationArtifactStatusInput {
                id: artifact.id.clone(),
                status: "resolved".to_string(),
            },
        )
        .expect("resolved");
        assert_eq!(resolved.status, "resolved");

        let dismissed = update_coordination_artifact_status_for_db(
            &db,
            UpdateCoordinationArtifactStatusInput {
                id: artifact.id,
                status: "dismissed".to_string(),
            },
        )
        .expect("dismissed");
        assert_eq!(dismissed.status, "dismissed");
    }
}
