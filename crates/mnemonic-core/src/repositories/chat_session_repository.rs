use rusqlite::{Row, params};

use crate::db::Database;
use crate::models::ChatSessionRecord;

fn record_from_row(row: &Row) -> rusqlite::Result<ChatSessionRecord> {
    Ok(ChatSessionRecord {
        id: row.get("id")?,
        workspace_id: row.get("workspace_id")?,
        agent: row.get("agent")?,
        title: row.get("title")?,
        acp_session_id: row.get("acp_session_id")?,
        is_review: row.get::<_, i64>("is_review")? != 0,
        position: row.get("position")?,
    })
}

pub fn list_for_workspace(
    db: &Database,
    workspace_id: &str,
) -> Result<Vec<ChatSessionRecord>, String> {
    db.with_connection(|connection| {
        let mut statement = connection.prepare(
            "SELECT * FROM workspace_chat_sessions WHERE workspace_id = ?1 \
             ORDER BY position, rowid",
        )?;
        let records = statement
            .query_map(params![workspace_id], record_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(records)
    })
}

pub fn insert(db: &Database, record: &ChatSessionRecord) -> Result<(), String> {
    db.with_connection_mut(|connection| {
        connection.execute(
            "INSERT INTO workspace_chat_sessions \
             (id, workspace_id, agent, title, acp_session_id, is_review, position) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                record.id,
                record.workspace_id,
                record.agent,
                record.title,
                record.acp_session_id,
                i64::from(record.is_review),
                record.position,
            ],
        )?;
        Ok(())
    })
}

pub fn set_acp_session_id(db: &Database, id: &str, acp_session_id: &str) -> Result<(), String> {
    db.with_connection_mut(|connection| {
        connection.execute(
            "UPDATE workspace_chat_sessions SET acp_session_id = ?1 WHERE id = ?2",
            params![acp_session_id, id],
        )?;
        Ok(())
    })
}

pub fn delete(db: &Database, id: &str) -> Result<(), String> {
    db.with_connection_mut(|connection| {
        connection.execute(
            "DELETE FROM workspace_chat_sessions WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    })
}

pub fn delete_for_workspace(db: &Database, workspace_id: &str) -> Result<(), String> {
    db.with_connection_mut(|connection| {
        connection.execute(
            "DELETE FROM workspace_chat_sessions WHERE workspace_id = ?1",
            params![workspace_id],
        )?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, workspace_id: &str, position: i64) -> ChatSessionRecord {
        ChatSessionRecord {
            id: id.to_string(),
            workspace_id: workspace_id.to_string(),
            agent: "claude-acp".to_string(),
            title: format!("Claude {id}"),
            acp_session_id: None,
            is_review: false,
            position,
        }
    }

    #[test]
    fn chat_sessions_round_trip_per_workspace() -> Result<(), String> {
        let db = Database::in_memory()?;
        insert(&db, &record("later", "ws-a", 1))?;
        insert(&db, &record("first", "ws-a", 0))?;
        insert(&db, &record("other", "ws-b", 0))?;

        let sessions = list_for_workspace(&db, "ws-a")?;
        let ids: Vec<&str> = sessions.iter().map(|session| session.id.as_str()).collect();
        assert_eq!(ids, ["first", "later"]);

        set_acp_session_id(&db, "first", "sess-1")?;
        let sessions = list_for_workspace(&db, "ws-a")?;
        assert_eq!(
            sessions
                .first()
                .and_then(|session| session.acp_session_id.as_deref()),
            Some("sess-1")
        );

        delete(&db, "first")?;
        assert_eq!(list_for_workspace(&db, "ws-a")?.len(), 1);

        delete_for_workspace(&db, "ws-a")?;
        assert!(list_for_workspace(&db, "ws-a")?.is_empty());
        assert_eq!(list_for_workspace(&db, "ws-b")?.len(), 1);
        Ok(())
    }
}
