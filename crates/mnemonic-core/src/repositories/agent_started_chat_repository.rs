use rusqlite::{OptionalExtension, Row, params};

use crate::db::Database;
use crate::models::AgentStartedChat;

const MAX_DEPTH_STEPS: u32 = 16;

fn record_from_row(row: &Row) -> rusqlite::Result<AgentStartedChat> {
    Ok(AgentStartedChat {
        chat_id: row.get("chat_id")?,
        started_by_chat_id: row.get("started_by_chat_id")?,
        workspace_id: row.get("workspace_id")?,
        made_workspace: row.get::<_, i64>("made_workspace")? != 0,
    })
}

pub fn insert(db: &Database, record: &AgentStartedChat) -> Result<(), String> {
    db.with_connection_mut(|connection| {
        connection.execute(
            "INSERT INTO agent_started_chats \
             (chat_id, started_by_chat_id, workspace_id, made_workspace) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                record.chat_id,
                record.started_by_chat_id,
                record.workspace_id,
                i64::from(record.made_workspace),
            ],
        )?;
        Ok(())
    })
}

pub fn get(db: &Database, chat_id: &str) -> Result<Option<AgentStartedChat>, String> {
    db.with_connection(|connection| {
        let record = connection
            .query_row(
                "SELECT * FROM agent_started_chats WHERE chat_id = ?1",
                params![chat_id],
                record_from_row,
            )
            .optional()?;
        Ok(record)
    })
}

pub fn started_by(db: &Database, chat_id: &str) -> Result<Vec<AgentStartedChat>, String> {
    db.with_connection(|connection| {
        let mut statement = connection.prepare(
            "SELECT * FROM agent_started_chats WHERE started_by_chat_id = ?1 ORDER BY rowid",
        )?;
        let records = statement
            .query_map(params![chat_id], record_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(records)
    })
}

pub fn workspace_made_by_agent(db: &Database, workspace_id: &str) -> Result<bool, String> {
    db.with_connection(|connection| {
        let found = connection
            .query_row(
                "SELECT 1 FROM agent_started_chats \
                 WHERE workspace_id = ?1 AND made_workspace = 1 LIMIT 1",
                params![workspace_id],
                |_| Ok(()),
            )
            .optional()?;
        Ok(found.is_some())
    })
}

/// Number of agent-started links above this chat; 0 for a chat nobody
/// started. Capped so a corrupt cycle cannot loop.
pub fn depth(db: &Database, chat_id: &str) -> Result<u32, String> {
    let mut current = chat_id.to_string();
    let mut steps = 0;
    while steps < MAX_DEPTH_STEPS {
        match get(db, &current)? {
            Some(record) => {
                current = record.started_by_chat_id;
                steps += 1;
            }
            None => break,
        }
    }
    Ok(steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(chat_id: &str, started_by: &str, workspace_id: &str, made: bool) -> AgentStartedChat {
        AgentStartedChat {
            chat_id: chat_id.to_string(),
            started_by_chat_id: started_by.to_string(),
            workspace_id: workspace_id.to_string(),
            made_workspace: made,
        }
    }

    #[test]
    fn depth_follows_the_started_by_chain() -> Result<(), String> {
        let db = Database::in_memory()?;
        insert(&db, &record("a", "user-chat", "ws-shared", false))?;
        insert(&db, &record("b", "a", "ws-made", true))?;

        assert_eq!(depth(&db, "user-chat")?, 0);
        assert_eq!(depth(&db, "a")?, 1);
        assert_eq!(depth(&db, "b")?, 2);

        assert!(workspace_made_by_agent(&db, "ws-made")?);
        assert!(!workspace_made_by_agent(&db, "ws-shared")?);
        assert_eq!(
            started_by(&db, "a")?,
            vec![record("b", "a", "ws-made", true)]
        );
        assert_eq!(get(&db, "missing")?, None);
        Ok(())
    }
}
