use rusqlite::{Row, params};

use crate::db::Database;
use crate::services::web_push_service::PushDevice;

fn device_from_row(row: &Row) -> rusqlite::Result<PushDevice> {
    Ok(PushDevice {
        install_id: row.get("install_id")?,
        token: row.get("token")?,
        p256dh: row.get("p256dh")?,
        auth: row.get("auth")?,
        name: row.get("name")?,
    })
}

/// A rotated push token arrives with the same install id and must replace the
/// old row, or the Mac keeps sending to a dead token.
pub fn upsert(db: &Database, device: &PushDevice) -> Result<(), String> {
    db.with_connection_mut(|connection| {
        connection.execute(
            "INSERT INTO phone_push_devices (install_id, token, p256dh, auth, name) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(install_id) DO UPDATE SET token = excluded.token, \
             p256dh = excluded.p256dh, auth = excluded.auth, name = excluded.name",
            params![
                device.install_id,
                device.token,
                device.p256dh,
                device.auth,
                device.name
            ],
        )?;
        Ok(())
    })
}

pub fn list(db: &Database) -> Result<Vec<PushDevice>, String> {
    db.with_connection(|connection| {
        let mut statement = connection.prepare(
            "SELECT install_id, token, p256dh, auth, name FROM phone_push_devices ORDER BY rowid",
        )?;
        let devices = statement
            .query_map([], device_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(devices)
    })
}

pub fn delete(db: &Database, install_id: &str) -> Result<(), String> {
    db.with_connection_mut(|connection| {
        connection.execute(
            "DELETE FROM phone_push_devices WHERE install_id = ?1",
            params![install_id],
        )?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(install_id: &str, token: &str) -> PushDevice {
        PushDevice {
            install_id: install_id.to_string(),
            token: token.to_string(),
            p256dh: "BCVxsr7N_eNgVRqvHtD0zTZsEc6".to_string(),
            auth: "BTBZMqHH6r4Tts7J_aSIgg".to_string(),
            name: "Pixel".to_string(),
        }
    }

    #[test]
    fn push_device_upsert_replaces_same_install() -> Result<(), String> {
        let db = Database::in_memory()?;
        upsert(&db, &device("a", "t1"))?;
        upsert(&db, &device("a", "t2"))?;

        assert_eq!(list(&db)?, [device("a", "t2")]);
        Ok(())
    }

    #[test]
    fn push_device_delete_removes_only_that_install() -> Result<(), String> {
        let db = Database::in_memory()?;
        upsert(&db, &device("a", "t1"))?;
        upsert(&db, &device("b", "t2"))?;

        delete(&db, "a")?;
        assert_eq!(list(&db)?, [device("b", "t2")]);
        Ok(())
    }
}
