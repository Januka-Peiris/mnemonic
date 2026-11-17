use rusqlite::{Row, params};

use crate::db::Database;
use crate::services::web_push_service::PushSubscription;

fn subscription_from_row(row: &Row) -> rusqlite::Result<PushSubscription> {
    Ok(PushSubscription {
        endpoint: row.get("endpoint")?,
        p256dh: row.get("p256dh")?,
        auth: row.get("auth")?,
    })
}

/// A browser that subscribes again keeps its endpoint but may bring new keys.
pub fn upsert(db: &Database, subscription: &PushSubscription) -> Result<(), String> {
    db.with_connection_mut(|connection| {
        connection.execute(
            "INSERT INTO phone_push_subscriptions (endpoint, p256dh, auth) VALUES (?1, ?2, ?3) \
             ON CONFLICT(endpoint) DO UPDATE SET p256dh = excluded.p256dh, auth = excluded.auth",
            params![
                subscription.endpoint,
                subscription.p256dh,
                subscription.auth
            ],
        )?;
        Ok(())
    })
}

pub fn list(db: &Database) -> Result<Vec<PushSubscription>, String> {
    db.with_connection(|connection| {
        let mut statement = connection.prepare(
            "SELECT endpoint, p256dh, auth FROM phone_push_subscriptions ORDER BY rowid",
        )?;
        let subscriptions = statement
            .query_map([], subscription_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(subscriptions)
    })
}

pub fn delete(db: &Database, endpoint: &str) -> Result<(), String> {
    db.with_connection_mut(|connection| {
        connection.execute(
            "DELETE FROM phone_push_subscriptions WHERE endpoint = ?1",
            params![endpoint],
        )?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subscription(endpoint: &str, auth: &str) -> PushSubscription {
        PushSubscription {
            endpoint: endpoint.to_string(),
            p256dh: "BCVxsr7N_eNgVRqvHtD0zTZsEc6".to_string(),
            auth: auth.to_string(),
        }
    }

    #[test]
    fn resubscribing_replaces_keys_and_delete_forgets() -> Result<(), String> {
        let db = Database::in_memory()?;
        upsert(&db, &subscription("https://push.example/phone", "old"))?;
        upsert(&db, &subscription("https://push.example/tablet", "tablet"))?;
        upsert(&db, &subscription("https://push.example/phone", "new"))?;

        assert_eq!(
            list(&db)?,
            [
                subscription("https://push.example/phone", "new"),
                subscription("https://push.example/tablet", "tablet"),
            ]
        );

        delete(&db, "https://push.example/phone")?;
        assert_eq!(
            list(&db)?,
            [subscription("https://push.example/tablet", "tablet")]
        );
        Ok(())
    }
}
