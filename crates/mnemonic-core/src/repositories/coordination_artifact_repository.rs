use rusqlite::{params, OptionalExtension};

use crate::db::Database;
use crate::models::CoordinationArtifact;

pub struct CoordinationArtifactInsert<'a> {
    pub id: &'a str,
    pub parent_workspace_id: &'a str,
    pub source_workspace_id: &'a str,
    pub target_workspace_id: Option<&'a str>,
    pub artifact_kind: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub status: &'a str,
}

pub struct CoordinationArtifactUpdate<'a> {
    pub id: &'a str,
    pub target_workspace_id: Option<&'a str>,
    pub artifact_kind: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub status: &'a str,
}

pub fn list_by_parent_workspace(
    db: &Database,
    parent_workspace_id: &str,
) -> Result<Vec<CoordinationArtifact>, String> {
    db.with_connection(|connection| {
        let mut statement = connection.prepare(
            r#"
            SELECT id, parent_workspace_id, source_workspace_id, target_workspace_id,
                   artifact_kind, title, body, status, created_at, updated_at
            FROM coordination_artifacts
            WHERE parent_workspace_id = ?1
            ORDER BY updated_at DESC, created_at DESC
            "#,
        )?;
        let artifacts = statement
            .query_map(params![parent_workspace_id], artifact_from_row)?
            .collect();
        artifacts
    })
}

pub fn get(db: &Database, id: &str) -> Result<Option<CoordinationArtifact>, String> {
    db.with_connection(|connection| {
        connection
            .query_row(
                r#"
                SELECT id, parent_workspace_id, source_workspace_id, target_workspace_id,
                       artifact_kind, title, body, status, created_at, updated_at
                FROM coordination_artifacts
                WHERE id = ?1
                "#,
                params![id],
                artifact_from_row,
            )
            .optional()
    })
}

pub fn insert(
    db: &Database,
    input: CoordinationArtifactInsert<'_>,
) -> Result<CoordinationArtifact, String> {
    db.with_connection_mut(|connection| {
        connection.execute(
            r#"
            INSERT INTO coordination_artifacts (
                id, parent_workspace_id, source_workspace_id, target_workspace_id,
                artifact_kind, title, body, status, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
            "#,
            params![
                input.id,
                input.parent_workspace_id,
                input.source_workspace_id,
                input.target_workspace_id,
                input.artifact_kind,
                input.title,
                input.body,
                input.status,
            ],
        )?;
        Ok(())
    })?;

    get(db, input.id)?.ok_or_else(|| "Coordination artifact was not saved".to_string())
}

pub fn update(
    db: &Database,
    input: CoordinationArtifactUpdate<'_>,
) -> Result<CoordinationArtifact, String> {
    let updated = db.with_connection_mut(|connection| {
        connection.execute(
            r#"
            UPDATE coordination_artifacts
            SET target_workspace_id = ?2,
                artifact_kind = ?3,
                title = ?4,
                body = ?5,
                status = ?6,
                updated_at = CURRENT_TIMESTAMP
            WHERE id = ?1
            "#,
            params![
                input.id,
                input.target_workspace_id,
                input.artifact_kind,
                input.title,
                input.body,
                input.status,
            ],
        )
    })?;
    if updated == 0 {
        return Err("Coordination artifact not found".to_string());
    }
    get(db, input.id)?.ok_or_else(|| "Coordination artifact not found".to_string())
}

pub fn delete(db: &Database, id: &str) -> Result<(), String> {
    let deleted = db.with_connection_mut(|connection| {
        connection.execute(
            "DELETE FROM coordination_artifacts WHERE id = ?1",
            params![id],
        )
    })?;
    if deleted == 0 {
        return Err("Coordination artifact not found".to_string());
    }
    Ok(())
}

pub fn update_status(
    db: &Database,
    id: &str,
    status: &str,
) -> Result<CoordinationArtifact, String> {
    let updated = db.with_connection_mut(|connection| {
        connection.execute(
            r#"
            UPDATE coordination_artifacts
            SET status = ?2,
                updated_at = CURRENT_TIMESTAMP
            WHERE id = ?1
            "#,
            params![id, status],
        )
    })?;
    if updated == 0 {
        return Err("Coordination artifact not found".to_string());
    }
    get(db, id)?.ok_or_else(|| "Coordination artifact not found".to_string())
}

fn artifact_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CoordinationArtifact> {
    Ok(CoordinationArtifact {
        id: row.get("id")?,
        parent_workspace_id: row.get("parent_workspace_id")?,
        source_workspace_id: row.get("source_workspace_id")?,
        target_workspace_id: row.get("target_workspace_id")?,
        artifact_kind: row.get("artifact_kind")?,
        title: row.get("title")?,
        body: row.get("body")?,
        status: row.get("status")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    fn seed_workspace(db: &Database, id: &str, parent_workspace_id: Option<&str>) {
        db.with_connection_mut(|connection| {
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
    fn inserts_and_lists_by_parent_workspace() {
        let db = Database::in_memory().expect("db");
        seed_workspace(&db, "parent", None);
        seed_workspace(&db, "child", Some("parent"));

        let artifact = insert(
            &db,
            CoordinationArtifactInsert {
                id: "artifact-1",
                parent_workspace_id: "parent",
                source_workspace_id: "child",
                target_workspace_id: Some("parent"),
                artifact_kind: "api_diff",
                title: "API changed",
                body: "Update the client payload.",
                status: "active",
            },
        )
        .expect("insert");

        assert_eq!(artifact.target_workspace_id.as_deref(), Some("parent"));
        let artifacts = list_by_parent_workspace(&db, "parent").expect("list");
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].id, "artifact-1");
    }

    #[test]
    fn updates_artifact_content() {
        let db = Database::in_memory().expect("db");
        seed_workspace(&db, "parent", None);
        insert(
            &db,
            CoordinationArtifactInsert {
                id: "artifact-1",
                parent_workspace_id: "parent",
                source_workspace_id: "parent",
                target_workspace_id: None,
                artifact_kind: "decision_summary",
                title: "Decision",
                body: "Old body.",
                status: "active",
            },
        )
        .expect("insert");

        let updated = update(
            &db,
            CoordinationArtifactUpdate {
                id: "artifact-1",
                target_workspace_id: Some("parent"),
                artifact_kind: "schema_change",
                title: "Schema",
                body: "New body.",
                status: "draft",
            },
        )
        .expect("update");
        assert_eq!(updated.artifact_kind, "schema_change");
        assert_eq!(updated.target_workspace_id.as_deref(), Some("parent"));
        assert_eq!(updated.body, "New body.");
        assert_eq!(updated.status, "draft");
    }

    #[test]
    fn deletes_artifact() {
        let db = Database::in_memory().expect("db");
        seed_workspace(&db, "parent", None);
        insert(
            &db,
            CoordinationArtifactInsert {
                id: "artifact-1",
                parent_workspace_id: "parent",
                source_workspace_id: "parent",
                target_workspace_id: None,
                artifact_kind: "decision_summary",
                title: "Decision",
                body: "Body.",
                status: "active",
            },
        )
        .expect("insert");

        delete(&db, "artifact-1").expect("delete");
        assert!(get(&db, "artifact-1").expect("get").is_none());
    }

    #[test]
    fn updates_status() {
        let db = Database::in_memory().expect("db");
        seed_workspace(&db, "parent", None);
        insert(
            &db,
            CoordinationArtifactInsert {
                id: "artifact-1",
                parent_workspace_id: "parent",
                source_workspace_id: "parent",
                target_workspace_id: None,
                artifact_kind: "decision_summary",
                title: "Decision",
                body: "Ship together.",
                status: "active",
            },
        )
        .expect("insert");

        let resolved = update_status(&db, "artifact-1", "resolved").expect("resolved");
        assert_eq!(resolved.status, "resolved");
        let dismissed = update_status(&db, "artifact-1", "dismissed").expect("dismissed");
        assert_eq!(dismissed.status, "dismissed");
    }
}
