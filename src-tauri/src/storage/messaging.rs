use crate::error::AppError;
use proscenium_types::{ConversationMeta, MediaAttachment, StoredMessage};
use sha2::{Digest, Sha256};
use sqlx::Row;

use super::Storage;

impl Storage {
    pub fn conversation_id(pubkey_a: &str, pubkey_b: &str) -> String {
        let mut keys = [pubkey_a, pubkey_b];
        keys.sort();
        let mut hasher = Sha256::new();
        hasher.update(b"proscenium-dm-v1:");
        hasher.update(keys[0].as_bytes());
        hasher.update(b":");
        hasher.update(keys[1].as_bytes());
        hex::encode(hasher.finalize())
    }

    pub async fn upsert_conversation(
        &self,
        peer_pubkey: &str,
        my_pubkey: &str,
        last_message_at: u64,
        preview: &str,
    ) -> Result<(), AppError> {
        let conv_id = Self::conversation_id(my_pubkey, peer_pubkey);
        sqlx::query(
            "INSERT INTO dm_conversations (conversation_id, peer_pubkey, last_message_at, last_message_preview, created_at)
             VALUES (?1, ?2, ?3, ?4, ?3)
             ON CONFLICT(conversation_id) DO UPDATE SET last_message_at=?3, last_message_preview=?4",
        )
        .bind(&conv_id)
        .bind(peer_pubkey)
        .bind(last_message_at as i64)
        .bind(preview)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_conversations(&self) -> Result<Vec<ConversationMeta>, AppError> {
        let rows = sqlx::query(
            "SELECT peer_pubkey, last_message_at, last_message_preview, unread_count
             FROM dm_conversations ORDER BY last_message_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut convos = Vec::new();
        for row in &rows {
            convos.push(ConversationMeta {
                peer_pubkey: row.get(0),
                last_message_at: row.get::<i64, _>(1) as u64,
                last_message_preview: row.get(2),
                unread_count: row.get::<i32, _>(3) as u32,
            });
        }
        Ok(convos)
    }

    pub async fn mark_conversation_read(
        &self,
        peer_pubkey: &str,
        my_pubkey: &str,
    ) -> Result<(), AppError> {
        let conv_id = Self::conversation_id(my_pubkey, peer_pubkey);
        sqlx::query("UPDATE dm_conversations SET unread_count = 0 WHERE conversation_id=?1")
            .bind(&conv_id)
            .execute(&self.pool)
            .await?;
        sqlx::query("UPDATE dm_messages SET read = 1 WHERE conversation_id=?1 AND read = 0")
            .bind(&conv_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn insert_dm_message(&self, msg: &StoredMessage) -> Result<(), AppError> {
        let media_json = serde_json::to_string(&msg.media)?;
        sqlx::query(
            "INSERT OR IGNORE INTO dm_messages (id, conversation_id, from_pubkey, to_pubkey, content, timestamp, media_json, read, delivered, reply_to)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )
        .bind(&msg.id)
        .bind(&msg.conversation_id)
        .bind(&msg.from_pubkey)
        .bind(&msg.to_pubkey)
        .bind(&msg.content)
        .bind(msg.timestamp as i64)
        .bind(&media_json)
        .bind(msg.read as i32)
        .bind(msg.delivered as i32)
        .bind(&msg.reply_to)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_dm_messages(
        &self,
        conversation_id: &str,
        limit: usize,
        before: Option<u64>,
    ) -> Result<Vec<StoredMessage>, AppError> {
        let rows = match before {
            Some(b) => {
                sqlx::query(
                    "SELECT id, conversation_id, from_pubkey, to_pubkey, content, timestamp, media_json, read, delivered, reply_to
                     FROM dm_messages WHERE conversation_id=?1 AND timestamp < ?2
                     ORDER BY timestamp DESC LIMIT ?3",
                )
                .bind(conversation_id)
                .bind(b as i64)
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "SELECT id, conversation_id, from_pubkey, to_pubkey, content, timestamp, media_json, read, delivered, reply_to
                     FROM dm_messages WHERE conversation_id=?1
                     ORDER BY timestamp DESC LIMIT ?2",
                )
                .bind(conversation_id)
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await?
            }
        };
        let mut messages = Vec::new();
        for row in &rows {
            messages.push(Self::row_to_stored_message(row)?);
        }
        messages.reverse();
        Ok(messages)
    }

    fn row_to_stored_message(row: &sqlx::sqlite::SqliteRow) -> Result<StoredMessage, AppError> {
        let media_json: String = row.get(6);
        let media: Vec<MediaAttachment> = serde_json::from_str(&media_json)?;
        Ok(StoredMessage {
            id: row.get(0),
            conversation_id: row.get(1),
            from_pubkey: row.get(2),
            to_pubkey: row.get(3),
            content: row.get(4),
            timestamp: row.get::<i64, _>(5) as u64,
            media,
            read: row.get::<i32, _>(7) != 0,
            delivered: row.get::<i32, _>(8) != 0,
            reply_to: row.get(9),
        })
    }

    pub async fn mark_dm_delivered(&self, message_id: &str) -> Result<(), AppError> {
        sqlx::query("UPDATE dm_messages SET delivered = 1 WHERE id=?1")
            .bind(message_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn mark_dm_read_by_id(&self, message_id: &str) -> Result<(), AppError> {
        sqlx::query("UPDATE dm_messages SET read = 1 WHERE id=?1")
            .bind(message_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_dm_message(&self, message_id: &str) -> Result<bool, AppError> {
        let result = sqlx::query("DELETE FROM dm_messages WHERE id=?1")
            .bind(message_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn get_total_unread_count(&self) -> Result<u32, AppError> {
        let count: i64 =
            sqlx::query_scalar("SELECT COALESCE(SUM(unread_count), 0) FROM dm_conversations")
                .fetch_one(&self.pool)
                .await?;
        Ok(count as u32)
    }

    pub async fn insert_outbox_message(
        &self,
        id: &str,
        peer_pubkey: &str,
        envelope_json: &str,
        created_at: u64,
        message_id: &str,
    ) -> Result<(), AppError> {
        sqlx::query(
            "INSERT INTO dm_outbox (id, peer_pubkey, envelope_json, created_at, message_id)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(id)
        .bind(peer_pubkey)
        .bind(envelope_json)
        .bind(created_at as i64)
        .bind(message_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Returns all pending dm_outbox entries ordered oldest-first.
    pub async fn get_all_outbox_messages(
        &self,
    ) -> Result<Vec<(String, String, String, String)>, AppError> {
        let rows = sqlx::query(
            "SELECT id, peer_pubkey, envelope_json, message_id FROM dm_outbox ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3)))
            .collect())
    }

    pub async fn remove_outbox_message(&self, id: &str) -> Result<(), AppError> {
        sqlx::query("DELETE FROM dm_outbox WHERE id=?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn receive_dm_message_atomically(
        &self,
        peer_signing_pubkey: &str,
        peer_master_pubkey: &str,
        ratchet_state: &str,
        ratchet_updated_at: u64,
        message: &StoredMessage,
        preview: &str,
    ) -> Result<(), AppError> {
        let conv_id = &message.conversation_id;
        let media_json = serde_json::to_string(&message.media)?;

        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO dm_ratchet_sessions (peer_pubkey, state_json, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(peer_pubkey) DO UPDATE SET state_json=?2, updated_at=?3",
        )
        .bind(peer_signing_pubkey)
        .bind(ratchet_state)
        .bind(ratchet_updated_at as i64)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO dm_conversations
                 (conversation_id, peer_pubkey, last_message_at, last_message_preview, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?3)
                 ON CONFLICT(conversation_id)
                 DO UPDATE SET last_message_at=?3, last_message_preview=?4",
        )
        .bind(conv_id)
        .bind(peer_master_pubkey)
        .bind(message.timestamp as i64)
        .bind(preview)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT OR IGNORE INTO dm_messages
                 (id, conversation_id, from_pubkey, to_pubkey, content, timestamp,
                  media_json, read, delivered, reply_to)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )
        .bind(&message.id)
        .bind(conv_id)
        .bind(&message.from_pubkey)
        .bind(&message.to_pubkey)
        .bind(&message.content)
        .bind(message.timestamp as i64)
        .bind(&media_json)
        .bind(message.read as i32)
        .bind(message.delivered as i32)
        .bind(&message.reply_to)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "UPDATE dm_conversations SET unread_count = unread_count + 1
             WHERE conversation_id=?1",
        )
        .bind(conv_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }
}
