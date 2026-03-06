use iroh_social_types::{Interaction, InteractionKind};
use rusqlite::params;

use super::{PostCounts, Storage};

impl Storage {
    pub(crate) fn row_to_interaction(row: &rusqlite::Row) -> anyhow::Result<Interaction> {
        let kind_str: String = row.get(2)?;
        let kind = match kind_str.to_lowercase().as_str() {
            "like" => InteractionKind::Like,
            other => anyhow::bail!("unknown interaction kind: {other}"),
        };
        Ok(Interaction {
            id: row.get(0)?,
            author: row.get(1)?,
            kind,
            target_post_id: row.get(3)?,
            target_author: row.get(4)?,
            timestamp: row.get::<_, i64>(5)? as u64,
            signature: row.get(6)?,
        })
    }

    pub fn save_interaction(&self, interaction: &Interaction) -> anyhow::Result<()> {
        let db = self.db.lock().unwrap();
        let kind_str = match interaction.kind {
            InteractionKind::Like => "Like",
        };
        db.execute(
            "INSERT OR IGNORE INTO interactions (id, author, kind, target_post_id, target_author, timestamp, signature)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                interaction.id,
                interaction.author,
                kind_str,
                interaction.target_post_id,
                interaction.target_author,
                interaction.timestamp as i64,
                interaction.signature,
            ],
        )?;
        Ok(())
    }

    pub fn delete_interaction(&self, id: &str, author: &str) -> anyhow::Result<bool> {
        let db = self.db.lock().unwrap();
        let count = db.execute(
            "DELETE FROM interactions WHERE id=?1 AND author=?2",
            params![id, author],
        )?;
        Ok(count > 0)
    }

    pub fn delete_interaction_by_target(
        &self,
        author: &str,
        kind: &str,
        target_post_id: &str,
    ) -> anyhow::Result<Option<String>> {
        let db = self.db.lock().unwrap();
        let id: Option<String> = db
            .query_row(
                "SELECT id FROM interactions WHERE author=?1 AND kind=?2 AND target_post_id=?3",
                params![author, kind, target_post_id],
                |row| row.get(0),
            )
            .ok();
        if let Some(ref id) = id {
            db.execute(
                "DELETE FROM interactions WHERE id=?1 AND author=?2",
                params![id, author],
            )?;
        }
        Ok(id)
    }

    pub fn get_post_counts(
        &self,
        my_pubkey: &str,
        target_post_id: &str,
    ) -> anyhow::Result<PostCounts> {
        let db = self.db.lock().unwrap();
        let likes: i64 = db.query_row(
            "SELECT COUNT(*) FROM interactions WHERE target_post_id=?1 AND kind='Like'",
            params![target_post_id],
            |row| row.get(0),
        )?;
        let reposts: i64 = db.query_row(
            "SELECT COUNT(*) FROM posts WHERE quote_of=?1",
            params![target_post_id],
            |row| row.get(0),
        )?;
        let replies: i64 = db.query_row(
            "SELECT COUNT(*) FROM posts WHERE reply_to=?1",
            params![target_post_id],
            |row| row.get(0),
        )?;
        let liked_by_me: bool = db.query_row(
            "SELECT COUNT(*) > 0 FROM interactions WHERE author=?1 AND kind='Like' AND target_post_id=?2",
            params![my_pubkey, target_post_id],
            |row| row.get(0),
        )?;
        let reposted_by_me: bool = db.query_row(
            "SELECT COUNT(*) > 0 FROM posts WHERE author=?1 AND quote_of=?2",
            params![my_pubkey, target_post_id],
            |row| row.get(0),
        )?;
        Ok(PostCounts {
            likes: likes as u32,
            replies: replies as u32,
            reposts: reposts as u32,
            liked_by_me,
            reposted_by_me,
        })
    }

    pub fn count_interactions_by_author(&self, author: &str) -> anyhow::Result<u64> {
        let db = self.db.lock().unwrap();
        let count: i64 = db.query_row(
            "SELECT COUNT(*) FROM interactions WHERE author=?1",
            params![author],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    pub fn newest_interaction_timestamp(&self, author: &str) -> anyhow::Result<u64> {
        let db = self.db.lock().unwrap();
        let ts: Option<i64> = db.query_row(
            "SELECT MAX(timestamp) FROM interactions WHERE author=?1",
            params![author],
            |row| row.get(0),
        )?;
        Ok(ts.unwrap_or(0) as u64)
    }

    pub fn count_interactions_after(&self, author: &str, after_ts: u64) -> anyhow::Result<u64> {
        let db = self.db.lock().unwrap();
        let count: i64 = db.query_row(
            "SELECT COUNT(*) FROM interactions WHERE author=?1 AND timestamp > ?2",
            params![author, after_ts as i64],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    pub fn get_interactions_after(
        &self,
        author: &str,
        after_ts: u64,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<Interaction>> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare(
            "SELECT id, author, kind, target_post_id, target_author, timestamp, signature
             FROM interactions WHERE author=?1 AND timestamp > ?2
             ORDER BY timestamp ASC LIMIT ?3 OFFSET ?4",
        )?;
        let mut rows = stmt.query(params![
            author,
            after_ts as i64,
            limit as i64,
            offset as i64
        ])?;
        let mut interactions = Vec::new();
        while let Some(row) = rows.next()? {
            interactions.push(Self::row_to_interaction(row)?);
        }
        Ok(interactions)
    }

    pub fn get_interactions_paged(
        &self,
        author: &str,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<Interaction>> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare(
            "SELECT id, author, kind, target_post_id, target_author, timestamp, signature
             FROM interactions WHERE author=?1
             ORDER BY timestamp ASC LIMIT ?2 OFFSET ?3",
        )?;
        let mut rows = stmt.query(params![author, limit as i64, offset as i64])?;
        let mut interactions = Vec::new();
        while let Some(row) = rows.next()? {
            interactions.push(Self::row_to_interaction(row)?);
        }
        Ok(interactions)
    }
}
