use rusqlite::Connection;

pub fn run(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(
            r#"
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS workspaces (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                repo TEXT NOT NULL,
                branch TEXT NOT NULL,
                agent TEXT NOT NULL,
                status TEXT NOT NULL,
                current_step TEXT NOT NULL,
                completed_steps TEXT NOT NULL DEFAULT '[]',
                last_updated TEXT NOT NULL,
                pr_status TEXT,
                pr_number INTEGER,
                description TEXT NOT NULL,
                current_task TEXT NOT NULL,
                ahead_by INTEGER NOT NULL DEFAULT 0,
                behind_by INTEGER NOT NULL DEFAULT 0,
                merge_risk TEXT NOT NULL,
                last_rebase TEXT NOT NULL,
                base_branch TEXT NOT NULL,
                agent_session_id TEXT NOT NULL,
                agent_session_agent TEXT NOT NULL,
                agent_session_status TEXT NOT NULL,
                agent_session_model TEXT NOT NULL,
                agent_session_token_count INTEGER NOT NULL DEFAULT 0,
                agent_session_estimated_cost TEXT NOT NULL,
                agent_session_last_message TEXT NOT NULL,
                agent_session_started_at TEXT NOT NULL,
                repository_id TEXT,
                repository_path TEXT,
                selected_branch TEXT,
                selected_worktree_id TEXT,
                selected_worktree_path TEXT,
                workspace_root_path TEXT,
                worktree_managed_by_forge INTEGER NOT NULL DEFAULT 0,
                workspace_source TEXT NOT NULL DEFAULT 'unknown',
                parent_workspace_id TEXT,
                source_workspace_id TEXT,
                derived_from_branch TEXT,
                run_tests_on_create INTEGER NOT NULL DEFAULT 1,
                create_pr_on_complete INTEGER NOT NULL DEFAULT 1,
                worktree_path TEXT NOT NULL,
                recent_events TEXT NOT NULL DEFAULT '[]',
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );

            CREATE TABLE IF NOT EXISTS changed_files (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                workspace_id TEXT NOT NULL,
                path TEXT NOT NULL,
                additions INTEGER NOT NULL DEFAULT 0,
                deletions INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL,
                sort_order INTEGER NOT NULL DEFAULT 0,
                FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_changed_files_workspace_id
                ON changed_files(workspace_id, sort_order);

            CREATE TABLE IF NOT EXISTS activity_items (
                id TEXT PRIMARY KEY,
                workspace_id TEXT,
                repo TEXT NOT NULL,
                branch TEXT,
                event TEXT NOT NULL,
                level TEXT NOT NULL,
                details TEXT,
                timestamp TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE SET NULL
            );

            CREATE INDEX IF NOT EXISTS idx_activity_items_created_at
                ON activity_items(created_at DESC);

            CREATE TABLE IF NOT EXISTS review_items (
                id TEXT PRIMARY KEY,
                workspace_id TEXT NOT NULL,
                workspace_name TEXT NOT NULL,
                repo TEXT NOT NULL,
                branch TEXT NOT NULL,
                risk TEXT NOT NULL,
                files_changed INTEGER NOT NULL DEFAULT 0,
                additions INTEGER NOT NULL DEFAULT 0,
                deletions INTEGER NOT NULL DEFAULT 0,
                ai_summary TEXT NOT NULL,
                author TEXT NOT NULL,
                created_at_label TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_review_items_created_at
                ON review_items(created_at DESC);

            CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );

            CREATE TABLE IF NOT EXISTS repositories (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                path TEXT NOT NULL UNIQUE,
                current_branch TEXT,
                head TEXT,
                is_dirty INTEGER NOT NULL DEFAULT 0,
                last_scanned_at TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );

            CREATE INDEX IF NOT EXISTS idx_repositories_path
                ON repositories(path);

            CREATE TABLE IF NOT EXISTS discovered_worktrees (
                id TEXT PRIMARY KEY,
                repo_id TEXT NOT NULL,
                path TEXT NOT NULL,
                branch TEXT,
                head TEXT,
                is_dirty INTEGER NOT NULL DEFAULT 0,
                is_detached INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (repo_id) REFERENCES repositories(id) ON DELETE CASCADE,
                UNIQUE(repo_id, path)
            );

            CREATE INDEX IF NOT EXISTS idx_discovered_worktrees_repo_id
                ON discovered_worktrees(repo_id);

            CREATE TABLE IF NOT EXISTS workspace_runs (
                id TEXT PRIMARY KEY,
                workspace_id TEXT NOT NULL,
                agent_type TEXT NOT NULL,
                command TEXT NOT NULL,
                args TEXT NOT NULL DEFAULT '[]',
                cwd TEXT NOT NULL,
                status TEXT NOT NULL,
                pid INTEGER,
                started_at TEXT NOT NULL,
                finished_at TEXT,
                exit_code INTEGER,
                error_message TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_workspace_runs_workspace_status
                ON workspace_runs(workspace_id, status);

            CREATE TABLE IF NOT EXISTS workspace_run_logs (
                id TEXT PRIMARY KEY,
                run_id TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                stream_type TEXT NOT NULL,
                message TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (run_id) REFERENCES workspace_runs(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_workspace_run_logs_run_id
                ON workspace_run_logs(run_id, created_at);

            CREATE TABLE IF NOT EXISTS review_summaries (
                workspace_id TEXT PRIMARY KEY,
                summary TEXT NOT NULL,
                risk_level TEXT NOT NULL,
                risk_reasons TEXT NOT NULL DEFAULT '[]',
                files_changed INTEGER NOT NULL DEFAULT 0,
                files_flagged INTEGER NOT NULL DEFAULT 0,
                additions INTEGER NOT NULL DEFAULT 0,
                deletions INTEGER NOT NULL DEFAULT 0,
                generated_at TEXT NOT NULL,
                file_insights TEXT NOT NULL DEFAULT '[]',
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS merge_readiness (
                workspace_id TEXT PRIMARY KEY,
                merge_ready INTEGER NOT NULL DEFAULT 0,
                readiness_level TEXT NOT NULL,
                reasons TEXT NOT NULL DEFAULT '[]',
                warnings TEXT NOT NULL DEFAULT '[]',
                ahead_count INTEGER,
                behind_count INTEGER,
                active_run_status TEXT,
                review_risk_level TEXT,
                pre_flight_checks TEXT NOT NULL DEFAULT '[]',
                generated_at TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS terminal_sessions (
                id TEXT PRIMARY KEY,
                workspace_id TEXT NOT NULL,
                session_role TEXT NOT NULL DEFAULT 'agent',
                profile TEXT NOT NULL,
                cwd TEXT NOT NULL,
                status TEXT NOT NULL,
                started_at TEXT NOT NULL,
                ended_at TEXT,
                command TEXT NOT NULL,
                args TEXT NOT NULL DEFAULT '[]',
                pid INTEGER,
                stale INTEGER NOT NULL DEFAULT 0,
                closed_at TEXT,
                backend TEXT NOT NULL DEFAULT 'pty',
                tmux_session_name TEXT,
                title TEXT NOT NULL DEFAULT '',
                terminal_kind TEXT NOT NULL DEFAULT 'agent',
                display_order INTEGER NOT NULL DEFAULT 0,
                is_visible INTEGER NOT NULL DEFAULT 1,
                last_attached_at TEXT,
                last_captured_seq INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_terminal_sessions_workspace_status
                ON terminal_sessions(workspace_id, status);

            CREATE TABLE IF NOT EXISTS terminal_output_chunks (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                timestamp TEXT NOT NULL,
                stream_type TEXT NOT NULL,
                data TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (session_id) REFERENCES terminal_sessions(id) ON DELETE CASCADE,
                UNIQUE(session_id, seq)
            );

            CREATE INDEX IF NOT EXISTS idx_terminal_output_chunks_session_seq
                ON terminal_output_chunks(session_id, seq);

            CREATE TABLE IF NOT EXISTS terminal_prompt_entries (
                id TEXT PRIMARY KEY,
                workspace_id TEXT NOT NULL,
                session_id TEXT,
                profile TEXT NOT NULL,
                prompt TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                sent_at TEXT,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE,
                FOREIGN KEY (session_id) REFERENCES terminal_sessions(id) ON DELETE SET NULL
            );

            CREATE INDEX IF NOT EXISTS idx_terminal_prompt_entries_workspace_created
                ON terminal_prompt_entries(workspace_id, created_at DESC);

            CREATE TABLE IF NOT EXISTS workspace_attention_reads (
                workspace_id TEXT PRIMARY KEY,
                last_read_at TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS workspace_linked_worktrees (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                workspace_id TEXT NOT NULL,
                worktree_id TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(workspace_id, worktree_id),
                FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE,
                FOREIGN KEY (worktree_id) REFERENCES discovered_worktrees(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_workspace_linked_worktrees_workspace
                ON workspace_linked_worktrees(workspace_id);

            CREATE TABLE IF NOT EXISTS pr_drafts (
                workspace_id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                summary TEXT NOT NULL,
                key_changes TEXT NOT NULL DEFAULT '[]',
                risks TEXT NOT NULL DEFAULT '[]',
                testing_notes TEXT NOT NULL DEFAULT '[]',
                generated_at TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
            );


            CREATE TABLE IF NOT EXISTS workspace_file_reviews (
                workspace_id TEXT NOT NULL,
                path TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'unreviewed',
                reviewed_at TEXT,
                reviewed_by TEXT NOT NULL DEFAULT 'local',
                notes TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (workspace_id, path),
                FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_workspace_file_reviews_workspace
                ON workspace_file_reviews(workspace_id, status);

            CREATE TABLE IF NOT EXISTS workspace_pr_comments (
                workspace_id TEXT NOT NULL,
                provider TEXT NOT NULL DEFAULT 'github',
                comment_id TEXT NOT NULL,
                author TEXT NOT NULL,
                body TEXT NOT NULL,
                path TEXT,
                line INTEGER,
                url TEXT,
                state TEXT NOT NULL DEFAULT 'open',
                created_at_remote TEXT,
                resolved_at TEXT,
                comment_node_id TEXT,
                thread_id TEXT,
                review_id INTEGER,
                thread_resolved INTEGER NOT NULL DEFAULT 0,
                thread_outdated INTEGER NOT NULL DEFAULT 0,
                thread_resolvable INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (workspace_id, provider, comment_id),
                FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_workspace_pr_comments_workspace_path
                ON workspace_pr_comments(workspace_id, path, line);

            CREATE TABLE IF NOT EXISTS agent_memory (
                id TEXT PRIMARY KEY,
                workspace_id TEXT,
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(workspace_id, key)
            );

            CREATE INDEX IF NOT EXISTS idx_agent_memory_workspace
                ON agent_memory(workspace_id);

            CREATE TABLE IF NOT EXISTS orchestrator_log (
                id TEXT PRIMARY KEY,
                run_at TEXT NOT NULL,
                model TEXT NOT NULL,
                workspace_ids TEXT NOT NULL DEFAULT '[]',
                actions TEXT NOT NULL DEFAULT '[]',
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );

            CREATE INDEX IF NOT EXISTS idx_orchestrator_log_run_at
                ON orchestrator_log(run_at DESC);

            CREATE TABLE IF NOT EXISTS task_runs (
                id TEXT PRIMARY KEY,
                workspace_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                status TEXT NOT NULL,
                source_id TEXT,
                started_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                ended_at TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_task_runs_workspace_status
                ON task_runs(workspace_id, status, updated_at DESC);

            CREATE TABLE IF NOT EXISTS task_events (
                id TEXT PRIMARY KEY,
                task_run_id TEXT NOT NULL,
                workspace_id TEXT NOT NULL,
                ts TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                event_type TEXT NOT NULL,
                payload TEXT NOT NULL DEFAULT '{}',
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (task_run_id) REFERENCES task_runs(id) ON DELETE CASCADE,
                FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_task_events_workspace_ts
                ON task_events(workspace_id, ts DESC);

            CREATE TABLE IF NOT EXISTS workspace_scheduler_jobs (
                id TEXT PRIMARY KEY,
                workspace_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                interval_seconds INTEGER NOT NULL,
                next_run_at INTEGER NOT NULL,
                enabled INTEGER NOT NULL DEFAULT 1,
                jitter_pct INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(workspace_id, kind),
                FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_workspace_scheduler_jobs_due
                ON workspace_scheduler_jobs(enabled, next_run_at);

            CREATE TABLE IF NOT EXISTS workspace_coordinator_runs (
                id TEXT PRIMARY KEY,
                workspace_id TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'running',
                brain_profile_id TEXT NOT NULL,
                coder_profile_id TEXT NOT NULL,
                goal TEXT NOT NULL,
                last_response TEXT,
                last_error TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                completed_at TEXT,
                FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_workspace_coordinator_runs_workspace_status
                ON workspace_coordinator_runs(workspace_id, status, updated_at DESC);

            CREATE TABLE IF NOT EXISTS workspace_coordinator_workers (
                id TEXT PRIMARY KEY,
                run_id TEXT NOT NULL,
                workspace_id TEXT NOT NULL,
                profile_id TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'running',
                last_prompt TEXT,
                last_session_id TEXT,
                notified_status TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (run_id) REFERENCES workspace_coordinator_runs(id) ON DELETE CASCADE,
                FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_workspace_coordinator_workers_run
                ON workspace_coordinator_workers(run_id, created_at ASC);

            CREATE TABLE IF NOT EXISTS workspace_coordinator_actions (
                id TEXT PRIMARY KEY,
                run_id TEXT NOT NULL,
                workspace_id TEXT NOT NULL,
                action_kind TEXT NOT NULL,
                replay_kind TEXT,
                replayed_from_action_id TEXT,
                worker_id TEXT,
                prompt TEXT,
                message TEXT,
                raw_json TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (run_id) REFERENCES workspace_coordinator_runs(id) ON DELETE CASCADE,
                FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_workspace_coordinator_actions_workspace_created
                ON workspace_coordinator_actions(workspace_id, created_at DESC);

            CREATE TABLE IF NOT EXISTS context_symbol_cache (
                blob_oid TEXT NOT NULL,
                parser_version TEXT NOT NULL,
                symbols_json TEXT NOT NULL,
                imports_json TEXT NOT NULL,
                summary TEXT NOT NULL,
                created_at TEXT NOT NULL,
                PRIMARY KEY (blob_oid, parser_version)
            );

            CREATE TABLE IF NOT EXISTS repo_intelligence_state (
                repo_id TEXT PRIMARY KEY,
                repository_path TEXT NOT NULL,
                default_branch TEXT NOT NULL,
                ref_name TEXT NOT NULL,
                indexed_commit TEXT NOT NULL,
                artifact_path TEXT NOT NULL,
                files_indexed INTEGER NOT NULL DEFAULT 0,
                symbol_count INTEGER NOT NULL DEFAULT 0,
                edge_count INTEGER NOT NULL DEFAULT 0,
                generated_at TEXT NOT NULL,
                refreshed_at TEXT NOT NULL,
                stale INTEGER NOT NULL DEFAULT 1,
                last_error TEXT,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );

            CREATE INDEX IF NOT EXISTS idx_repo_intelligence_state_refreshed
                ON repo_intelligence_state(refreshed_at DESC);

            CREATE TABLE IF NOT EXISTS app_repository_relationships (
                id TEXT PRIMARY KEY,
                from_repo_id TEXT NOT NULL,
                to_repo_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                label TEXT,
                notes TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (from_repo_id) REFERENCES repositories(id) ON DELETE CASCADE,
                FOREIGN KEY (to_repo_id) REFERENCES repositories(id) ON DELETE CASCADE,
                UNIQUE(from_repo_id, to_repo_id, kind)
            );

            CREATE INDEX IF NOT EXISTS idx_app_repository_relationships_from
                ON app_repository_relationships(from_repo_id);

            CREATE INDEX IF NOT EXISTS idx_app_repository_relationships_to
                ON app_repository_relationships(to_repo_id);

            CREATE TABLE IF NOT EXISTS coordination_artifacts (
                id TEXT PRIMARY KEY,
                parent_workspace_id TEXT NOT NULL,
                source_workspace_id TEXT NOT NULL,
                target_workspace_id TEXT,
                artifact_kind TEXT NOT NULL,
                title TEXT NOT NULL,
                body TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'active',
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (parent_workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE,
                FOREIGN KEY (source_workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE,
                FOREIGN KEY (target_workspace_id) REFERENCES workspaces(id) ON DELETE SET NULL
            );

            CREATE INDEX IF NOT EXISTS idx_coordination_artifacts_parent_updated
                ON coordination_artifacts(parent_workspace_id, updated_at DESC);

            CREATE INDEX IF NOT EXISTS idx_coordination_artifacts_source
                ON coordination_artifacts(source_workspace_id);

            CREATE INDEX IF NOT EXISTS idx_coordination_artifacts_target
                ON coordination_artifacts(target_workspace_id);

            CREATE TABLE IF NOT EXISTS repository_settings (
                repository_id TEXT NOT NULL,
                key TEXT NOT NULL,
                value TEXT NOT NULL DEFAULT '',
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (repository_id, key)
            );
            "#,
        )
        .map_err(|err| format!("Failed to run SQLite migrations: {err}"))?;

    add_column_if_missing(connection, "workspaces", "repository_id", "TEXT")?;
    add_column_if_missing(connection, "workspaces", "repository_path", "TEXT")?;
    add_column_if_missing(connection, "workspaces", "selected_branch", "TEXT")?;
    add_column_if_missing(connection, "workspaces", "selected_worktree_id", "TEXT")?;
    add_column_if_missing(connection, "workspaces", "selected_worktree_path", "TEXT")?;
    add_column_if_missing(connection, "workspaces", "workspace_root_path", "TEXT")?;
    add_column_if_missing(
        connection,
        "workspaces",
        "worktree_managed_by_forge",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        connection,
        "workspaces",
        "workspace_source",
        "TEXT NOT NULL DEFAULT 'unknown'",
    )?;
    add_column_if_missing(connection, "workspaces", "parent_workspace_id", "TEXT")?;
    add_column_if_missing(connection, "workspaces", "source_workspace_id", "TEXT")?;
    add_column_if_missing(
        connection,
        "workspaces",
        "run_tests_on_create",
        "INTEGER NOT NULL DEFAULT 1",
    )?;
    add_column_if_missing(
        connection,
        "workspaces",
        "create_pr_on_complete",
        "INTEGER NOT NULL DEFAULT 1",
    )?;

    add_column_if_missing(
        connection,
        "workspace_pr_comments",
        "comment_node_id",
        "TEXT",
    )?;
    add_column_if_missing(connection, "workspace_pr_comments", "thread_id", "TEXT")?;
    add_column_if_missing(connection, "workspace_pr_comments", "review_id", "INTEGER")?;
    add_column_if_missing(
        connection,
        "workspace_pr_comments",
        "thread_resolved",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        connection,
        "workspace_pr_comments",
        "thread_outdated",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        connection,
        "workspace_pr_comments",
        "thread_resolvable",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(connection, "workspaces", "derived_from_branch", "TEXT")?;
    add_column_if_missing(
        connection,
        "terminal_sessions",
        "session_role",
        "TEXT NOT NULL DEFAULT 'agent'",
    )?;
    add_column_if_missing(connection, "terminal_sessions", "closed_at", "TEXT")?;
    add_column_if_missing(
        connection,
        "terminal_sessions",
        "backend",
        "TEXT NOT NULL DEFAULT 'pty'",
    )?;
    add_column_if_missing(connection, "terminal_sessions", "tmux_session_name", "TEXT")?;
    add_column_if_missing(
        connection,
        "terminal_sessions",
        "title",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    add_column_if_missing(
        connection,
        "terminal_sessions",
        "terminal_kind",
        "TEXT NOT NULL DEFAULT 'agent'",
    )?;
    add_column_if_missing(
        connection,
        "terminal_sessions",
        "display_order",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        connection,
        "terminal_sessions",
        "is_visible",
        "INTEGER NOT NULL DEFAULT 1",
    )?;
    add_column_if_missing(connection, "terminal_sessions", "last_attached_at", "TEXT")?;
    add_column_if_missing(
        connection,
        "terminal_sessions",
        "last_captured_seq",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(connection, "workspaces", "cost_limit_usd", "REAL")?;
    add_column_if_missing(
        connection,
        "workspace_coordinator_workers",
        "notified_status",
        "TEXT",
    )?;
    add_column_if_missing(
        connection,
        "workspace_coordinator_actions",
        "replay_kind",
        "TEXT",
    )?;
    add_column_if_missing(
        connection,
        "workspace_coordinator_actions",
        "replayed_from_action_id",
        "TEXT",
    )?;
    add_column_if_missing(
        connection,
        "merge_readiness",
        "pre_flight_checks",
        "TEXT NOT NULL DEFAULT '[]'",
    )?;
    add_column_if_missing(
        connection,
        "agent_memory",
        "scope",
        "TEXT NOT NULL DEFAULT 'global'",
    )?;
    add_column_if_missing(
        connection,
        "agent_memory",
        "origin",
        "TEXT NOT NULL DEFAULT 'manual'",
    )?;
    add_column_if_missing(
        connection,
        "agent_memory",
        "confidence",
        "REAL NOT NULL DEFAULT 1.0",
    )?;
    add_column_if_missing(
        connection,
        "agent_memory",
        "status",
        "TEXT NOT NULL DEFAULT 'active'",
    )?;
    add_column_if_missing(connection, "agent_memory", "source_task_run_id", "TEXT")?;
    add_column_if_missing(connection, "agent_memory", "source_label", "TEXT")?;
    add_column_if_missing(connection, "agent_memory", "source_detail", "TEXT")?;
    add_column_if_missing(connection, "agent_memory", "last_used_at", "TEXT")?;
    add_column_if_missing(connection, "terminal_prompt_entries", "model", "TEXT")?;
    add_column_if_missing(connection, "terminal_sessions", "claude_session_id", "TEXT")?;

    // Workspace templates table
    connection
        .execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS workspace_templates (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                task_prompt TEXT NOT NULL DEFAULT '',
                agent TEXT NOT NULL DEFAULT 'Claude Code',
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            "#,
        )
        .map_err(|err| format!("Failed to create workspace_templates table: {err}"))?;

    // Drop deprecated agent_chat tables (child table first to respect FK).
    // Idempotent: IF EXISTS means this is safe on fresh installs too.
    connection
        .execute_batch(
            r#"
            DROP TABLE IF EXISTS agent_chat_events;
            DROP TABLE IF EXISTS agent_chat_sessions;
            "#,
        )
        .map_err(|err| format!("Failed to drop deprecated agent_chat tables: {err}"))?;

    Ok(())
}

fn add_column_if_missing(
    connection: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<(), String> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|err| format!("Failed to inspect table {table}: {err}"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|err| format!("Failed to list columns for {table}: {err}"))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|err| format!("Failed to read columns for {table}: {err}"))?;

    if !columns.iter().any(|existing| existing == column) {
        connection
            .execute(
                &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
                [],
            )
            .map_err(|err| format!("Failed to add column {column} to {table}: {err}"))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_columns(connection: &Connection, table: &str) -> Vec<String> {
        let mut statement = connection
            .prepare(&format!("PRAGMA table_info({table})"))
            .expect("table info query");
        statement
            .query_map([], |row| row.get::<_, String>(1))
            .expect("table info rows")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("table info collect")
    }

    #[test]
    fn backfills_coordinator_action_replay_columns() {
        let connection = Connection::open_in_memory().expect("open in-memory db");
        connection
            .execute_batch(
                r#"
                CREATE TABLE workspace_coordinator_actions (
                    id TEXT PRIMARY KEY,
                    run_id TEXT NOT NULL,
                    workspace_id TEXT NOT NULL,
                    action_kind TEXT NOT NULL,
                    worker_id TEXT,
                    prompt TEXT,
                    message TEXT,
                    raw_json TEXT,
                    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
                );
                CREATE TABLE workspace_coordinator_workers (
                    id TEXT PRIMARY KEY,
                    run_id TEXT NOT NULL,
                    workspace_id TEXT NOT NULL,
                    profile_id TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'running',
                    last_prompt TEXT,
                    last_session_id TEXT,
                    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
                );
                "#,
            )
            .expect("seed old schema");

        run(&connection).expect("run migrations");
        let action_columns = table_columns(&connection, "workspace_coordinator_actions");
        assert!(action_columns.iter().any(|name| name == "replay_kind"));
        assert!(action_columns
            .iter()
            .any(|name| name == "replayed_from_action_id"));
        let worker_columns = table_columns(&connection, "workspace_coordinator_workers");
        assert!(worker_columns.iter().any(|name| name == "notified_status"));
    }

    #[test]
    fn creates_app_repository_relationship_tables() {
        let connection = Connection::open_in_memory().expect("open in-memory db");
        run(&connection).expect("run migrations");

        let columns = table_columns(&connection, "app_repository_relationships");
        assert!(columns.iter().any(|name| name == "from_repo_id"));
        assert!(columns.iter().any(|name| name == "to_repo_id"));
        assert!(columns.iter().any(|name| name == "kind"));
    }

    #[test]
    fn creates_coordination_artifacts_table() {
        let connection = Connection::open_in_memory().expect("open in-memory db");
        run(&connection).expect("run migrations");

        let columns = table_columns(&connection, "coordination_artifacts");
        for expected in [
            "id",
            "parent_workspace_id",
            "source_workspace_id",
            "target_workspace_id",
            "artifact_kind",
            "title",
            "body",
            "status",
            "created_at",
            "updated_at",
        ] {
            assert!(
                columns.iter().any(|name| name == expected),
                "missing {expected}"
            );
        }
    }
}
