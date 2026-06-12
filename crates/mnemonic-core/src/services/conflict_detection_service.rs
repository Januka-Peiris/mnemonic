use crate::db::Database;
use crate::models::{WorkspaceConflict, WorkspaceConflicts};

/// Find files that are modified in more than one active workspace simultaneously.
/// Only non-merged workspaces are considered (status != 'Merged').
pub fn detect_workspace_conflicts(db: &Database) -> Result<WorkspaceConflicts, String> {
    db.with_connection(|connection| {
        // Find all file paths touched by more than one active workspace.
        let mut statement = connection.prepare(
            r#"
            SELECT cf1.workspace_id  AS workspace_id_a,
                   cf2.workspace_id  AS workspace_id_b,
                   cf1.path          AS path
            FROM   changed_files cf1
            JOIN   changed_files cf2
                   ON  cf1.path = cf2.path
                   AND cf1.workspace_id < cf2.workspace_id
            WHERE  cf1.workspace_id IN (
                       SELECT id FROM workspaces
                       WHERE  status != 'Merged'
                   )
              AND  cf2.workspace_id IN (
                       SELECT id FROM workspaces
                       WHERE  status != 'Merged'
                   )
            ORDER  BY cf1.workspace_id, cf2.workspace_id, cf1.path
            "#,
        )?;

        // Aggregate rows into (workspace_id_a, workspace_id_b) → Vec<path>
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut pairs: std::collections::HashMap<(String, String), Vec<String>> =
            std::collections::HashMap::new();
        for (a, b, path) in rows {
            pairs.entry((a, b)).or_default().push(path);
        }

        let mut conflicts: Vec<WorkspaceConflict> = pairs
            .into_iter()
            .map(|((a, b), files)| {
                let file_count = files.len();
                WorkspaceConflict {
                    workspace_id_a: a,
                    workspace_id_b: b,
                    shared_files: files,
                    file_count,
                }
            })
            .collect();

        // Deterministic ordering.
        conflicts.sort_by(|x, y| {
            x.workspace_id_a
                .cmp(&y.workspace_id_a)
                .then(x.workspace_id_b.cmp(&y.workspace_id_b))
        });

        // Flat set of all workspace IDs that appear in at least one conflict.
        let mut conflicting_ids: Vec<String> = conflicts
            .iter()
            .flat_map(|c| [c.workspace_id_a.clone(), c.workspace_id_b.clone()])
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        conflicting_ids.sort();

        Ok(WorkspaceConflicts {
            conflicts,
            conflicting_workspace_ids: conflicting_ids,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    fn setup_db() -> Database {
        let db = Database::in_memory().unwrap();
        db.with_connection_mut(|conn| {
            conn.execute_batch(
                r#"
                INSERT INTO workspaces (
                    id, name, repo, branch, agent, status, current_step, completed_steps,
                    last_updated, pr_status, description, current_task,
                    ahead_by, behind_by, merge_risk, last_rebase, base_branch,
                    agent_session_id, agent_session_agent, agent_session_status,
                    agent_session_model, agent_session_token_count, agent_session_estimated_cost,
                    agent_session_last_message, agent_session_started_at,
                    worktree_path, recent_events
                ) VALUES
                    ('ws-1','A','r','b1','claude_code','Running','Planning','[]',
                     '1','null','','',0,0,'Low','','main',
                     '','','idle','',0,'$0.00','','','','[]'),
                    ('ws-2','B','r','b2','claude_code','Running','Planning','[]',
                     '1','null','','',0,0,'Low','','main',
                     '','','idle','',0,'$0.00','','','','[]');
                INSERT INTO changed_files (workspace_id, path, additions, deletions, status, sort_order)
                VALUES
                    ('ws-1', 'src/auth.rs', 10, 2, 'modified', 0),
                    ('ws-1', 'src/lib.rs',   5, 0, 'modified', 1),
                    ('ws-2', 'src/auth.rs',  3, 1, 'modified', 0),
                    ('ws-2', 'src/main.rs',  1, 0, 'added',    1);
                "#,
            )?;
            Ok(())
        })
        .unwrap();
        db
    }

    #[test]
    fn detects_shared_file() {
        let db = setup_db();
        let result = detect_workspace_conflicts(&db).unwrap();
        assert_eq!(result.conflicts.len(), 1);
        let conflict = &result.conflicts[0];
        assert_eq!(conflict.workspace_id_a, "ws-1");
        assert_eq!(conflict.workspace_id_b, "ws-2");
        assert_eq!(conflict.shared_files, vec!["src/auth.rs"]);
        assert!(result
            .conflicting_workspace_ids
            .contains(&"ws-1".to_string()));
        assert!(result
            .conflicting_workspace_ids
            .contains(&"ws-2".to_string()));
    }

    #[test]
    fn no_conflicts_when_files_disjoint() {
        let db = Database::in_memory().unwrap();
        db.with_connection_mut(|conn| {
            conn.execute_batch(
                r#"
                INSERT INTO workspaces (
                    id, name, repo, branch, agent, status, current_step, completed_steps,
                    last_updated, pr_status, description, current_task,
                    ahead_by, behind_by, merge_risk, last_rebase, base_branch,
                    agent_session_id, agent_session_agent, agent_session_status,
                    agent_session_model, agent_session_token_count, agent_session_estimated_cost,
                    agent_session_last_message, agent_session_started_at,
                    worktree_path, recent_events
                ) VALUES
                    ('ws-3','C','r','b3','claude_code','Running','Planning','[]',
                     '1','null','','',0,0,'Low','','main',
                     '','','idle','',0,'$0.00','','','','[]'),
                    ('ws-4','D','r','b4','claude_code','Running','Planning','[]',
                     '1','null','','',0,0,'Low','','main',
                     '','','idle','',0,'$0.00','','','','[]');
                INSERT INTO changed_files (workspace_id, path, additions, deletions, status, sort_order)
                VALUES
                    ('ws-3', 'src/foo.rs', 1, 0, 'added', 0),
                    ('ws-4', 'src/bar.rs', 1, 0, 'added', 0);
                "#,
            )?;
            Ok(())
        })
        .unwrap();
        let result = detect_workspace_conflicts(&db).unwrap();
        assert_eq!(result.conflicts.len(), 0);
        assert!(result.conflicting_workspace_ids.is_empty());
    }
}
