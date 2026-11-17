use rusqlite::{params, Row};

use crate::db::Database;
use crate::models::{DiscoveredRepository, DiscoveredWorktree};

pub fn list(db: &Database) -> Result<Vec<DiscoveredRepository>, String> {
    db.with_connection(|connection| {
        let mut statement = connection.prepare(
            r#"
            SELECT id, name, path, current_branch, head, is_dirty, last_scanned_at
            FROM repositories
            ORDER BY name ASC, path ASC
            "#,
        )?;

        let repos = statement
            .query_map([], |row| repository_from_row(row, connection))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(repos)
    })
}

pub fn replace_all(db: &Database, repositories: &[DiscoveredRepository]) -> Result<(), String> {
    db.with_connection_mut(|connection| {
        let transaction = connection.transaction()?;
        transaction.execute("DELETE FROM discovered_worktrees", [])?;
        transaction.execute("DELETE FROM repositories", [])?;

        for repo in repositories {
            transaction.execute(
                r#"
                INSERT INTO repositories (
                    id, name, path, current_branch, head, is_dirty, last_scanned_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, CURRENT_TIMESTAMP)
                "#,
                params![
                    repo.id,
                    repo.name,
                    repo.path,
                    repo.current_branch,
                    repo.head,
                    repo.is_dirty as i64,
                    repo.last_scanned_at,
                ],
            )?;

            for worktree in &repo.worktrees {
                transaction.execute(
                    r#"
                    INSERT INTO discovered_worktrees (
                        id, repo_id, path, branch, head, is_dirty, is_detached, updated_at
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, CURRENT_TIMESTAMP)
                    "#,
                    params![
                        worktree.id,
                        repo.id,
                        worktree.path,
                        worktree.branch,
                        worktree.head,
                        worktree.is_dirty as i64,
                        worktree.is_detached as i64,
                    ],
                )?;
            }
        }

        transaction.commit()
    })
}

fn repository_from_row(
    row: &Row<'_>,
    connection: &rusqlite::Connection,
) -> rusqlite::Result<DiscoveredRepository> {
    let id: String = row.get("id")?;

    Ok(DiscoveredRepository {
        id: id.clone(),
        name: row.get("name")?,
        path: row.get("path")?,
        current_branch: row.get("current_branch")?,
        head: row.get("head")?,
        is_dirty: row.get::<_, i64>("is_dirty")? != 0,
        worktrees: list_worktrees(connection, &id)?,
        last_scanned_at: row.get("last_scanned_at")?,
    })
}

fn list_worktrees(
    connection: &rusqlite::Connection,
    repo_id: &str,
) -> rusqlite::Result<Vec<DiscoveredWorktree>> {
    let mut statement = connection.prepare(
        r#"
        SELECT id, repo_id, path, branch, head, is_dirty, is_detached
        FROM discovered_worktrees
        WHERE repo_id = ?1
        ORDER BY path ASC
        "#,
    )?;

    let worktrees = statement
        .query_map(params![repo_id], |row| {
            Ok(DiscoveredWorktree {
                id: row.get("id")?,
                repo_id: row.get("repo_id")?,
                path: row.get("path")?,
                branch: row.get("branch")?,
                head: row.get("head")?,
                is_dirty: row.get::<_, i64>("is_dirty")? != 0,
                is_detached: row.get::<_, i64>("is_detached")? != 0,
            })
        })?
        .collect();

    worktrees
}

pub fn get(db: &Database, id: &str) -> Result<Option<DiscoveredRepository>, String> {
    db.with_connection(|connection| {
        let mut statement = connection.prepare(
            r#"
            SELECT id, name, path, current_branch, head, is_dirty, last_scanned_at
            FROM repositories
            WHERE id = ?1
            "#,
        )?;

        let mut rows = statement.query(params![id])?;
        if let Some(row) = rows.next()? {
            repository_from_row(row, connection).map(Some)
        } else {
            Ok(None)
        }
    })
}

pub fn get_worktree(db: &Database, id: &str) -> Result<Option<DiscoveredWorktree>, String> {
    db.with_connection(|connection| {
        let mut statement = connection.prepare(
            r#"
            SELECT id, repo_id, path, branch, head, is_dirty, is_detached
            FROM discovered_worktrees
            WHERE id = ?1
            "#,
        )?;

        let mut rows = statement.query(params![id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(DiscoveredWorktree {
                id: row.get("id")?,
                repo_id: row.get("repo_id")?,
                path: row.get("path")?,
                branch: row.get("branch")?,
                head: row.get("head")?,
                is_dirty: row.get::<_, i64>("is_dirty")? != 0,
                is_detached: row.get::<_, i64>("is_detached")? != 0,
            }))
        } else {
            Ok(None)
        }
    })
}

pub fn remove(db: &Database, id: &str) -> Result<(), String> {
    db.with_connection_mut(|connection| {
        connection.execute(
            "DELETE FROM discovered_worktrees WHERE repo_id = ?1",
            params![id],
        )?;
        connection.execute("DELETE FROM repositories WHERE id = ?1", params![id])?;
        Ok(())
    })
}

pub fn upsert(db: &Database, repo: &DiscoveredRepository) -> Result<(), String> {
    db.with_connection_mut(|connection| {
        let transaction = connection.transaction()?;
        transaction.execute(
            r#"
            INSERT INTO repositories (
                id, name, path, current_branch, head, is_dirty, last_scanned_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, CURRENT_TIMESTAMP)
            ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                path = excluded.path,
                current_branch = excluded.current_branch,
                head = excluded.head,
                is_dirty = excluded.is_dirty,
                last_scanned_at = excluded.last_scanned_at,
                updated_at = CURRENT_TIMESTAMP
            "#,
            params![
                repo.id,
                repo.name,
                repo.path,
                repo.current_branch,
                repo.head,
                repo.is_dirty as i64,
                repo.last_scanned_at,
            ],
        )?;
        transaction.execute(
            "DELETE FROM discovered_worktrees WHERE repo_id = ?1",
            params![repo.id],
        )?;
        for worktree in &repo.worktrees {
            transaction.execute(
                r#"
                INSERT INTO discovered_worktrees (
                    id, repo_id, path, branch, head, is_dirty, is_detached, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, CURRENT_TIMESTAMP)
                "#,
                params![
                    worktree.id,
                    repo.id,
                    worktree.path,
                    worktree.branch,
                    worktree.head,
                    worktree.is_dirty as i64,
                    worktree.is_detached as i64,
                ],
            )?;
        }
        transaction.commit()
    })
}
