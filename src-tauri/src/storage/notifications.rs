use crate::error::AppError;
use sha2::{Digest, Sha256};
use sqlx::Row;

use proscenium_types::Notification;

use super::Storage;

impl Storage {
    pub async fn insert_notification(
        &self,
        kind: &str,
        actor: &str,
        target_post_id: Option<&str>,
        post_id: Option<&str>,
        timestamp: u64,
    ) -> Result<(), AppError> {
        let mut hasher = Sha256::new();
        hasher.update(kind.as_bytes());
        hasher.update(actor.as_bytes());
        hasher.update(target_post_id.unwrap_or("").as_bytes());
        hasher.update(post_id.unwrap_or("").as_bytes());
        let id = hex::encode(hasher.finalize());
        sqlx::query(
            "INSERT OR IGNORE INTO notifications (id, kind, actor, target_post_id, post_id, timestamp, read)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
        )
        .bind(&id)
        .bind(kind)
        .bind(actor)
        .bind(target_post_id)
        .bind(post_id)
        .bind(timestamp as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_notifications(
        &self,
        limit: usize,
        before: Option<u64>,
    ) -> Result<Vec<Notification>, AppError> {
        let hidden = "AND n.actor NOT IN (SELECT pubkey FROM moderation WHERE state='active')";
        let rows = match before {
            Some(b) => {
                let sql = format!(
                    "SELECT id, kind, actor, target_post_id, post_id, timestamp, read
                     FROM notifications n
                     WHERE n.timestamp < ?1 {hidden}
                     ORDER BY n.timestamp DESC LIMIT ?2"
                );
                sqlx::query(sqlx::AssertSqlSafe(sql))
                    .bind(b as i64)
                    .bind(limit as i64)
                    .fetch_all(&self.pool)
                    .await?
            }
            None => {
                let sql = format!(
                    "SELECT id, kind, actor, target_post_id, post_id, timestamp, read
                     FROM notifications n
                     WHERE 1=1 {hidden}
                     ORDER BY n.timestamp DESC LIMIT ?1"
                );
                sqlx::query(sqlx::AssertSqlSafe(sql))
                    .bind(limit as i64)
                    .fetch_all(&self.pool)
                    .await?
            }
        };
        let mut notifications = Vec::new();
        for row in &rows {
            notifications.push(Self::row_to_notification(row));
        }
        Ok(notifications)
    }

    fn row_to_notification(row: &sqlx::sqlite::SqliteRow) -> Notification {
        let read_int: i32 = row.get(6);
        Notification {
            id: row.get(0),
            kind: row.get(1),
            actor: row.get(2),
            target_post_id: row.get(3),
            post_id: row.get(4),
            timestamp: row.get::<i64, _>(5) as u64,
            read: read_int != 0,
        }
    }

    pub async fn get_unread_notification_count(&self) -> Result<u32, AppError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notifications WHERE read=0")
            .fetch_one(&self.pool)
            .await?;
        Ok(count as u32)
    }

    pub async fn mark_notifications_read(&self) -> Result<(), AppError> {
        sqlx::query("UPDATE notifications SET read=1 WHERE read=0")
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
