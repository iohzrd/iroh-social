use iroh_social_types::{MediaAttachment, Post};
use rusqlite::params;

use super::{FeedQuery, Storage};

impl Storage {
    pub(crate) fn row_to_post(row: &rusqlite::Row) -> anyhow::Result<Post> {
        let media_json: String = row.get(4)?;
        let media: Vec<MediaAttachment> = serde_json::from_str(&media_json)?;
        Ok(Post {
            id: row.get(0)?,
            author: row.get(1)?,
            content: row.get(2)?,
            timestamp: row.get::<_, i64>(3)? as u64,
            media,
            reply_to: row.get(5)?,
            reply_to_author: row.get(6)?,
            quote_of: row.get(7)?,
            quote_of_author: row.get(8)?,
            signature: row.get(9)?,
        })
    }

    pub fn insert_post(&self, post: &Post) -> anyhow::Result<()> {
        let db = self.db.lock().unwrap();
        let media_json = serde_json::to_string(&post.media)?;
        db.execute(
            "INSERT OR IGNORE INTO posts (id, author, content, timestamp, media_json, reply_to, reply_to_author, quote_of, quote_of_author, signature)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                post.id,
                post.author,
                post.content,
                post.timestamp as i64,
                media_json,
                post.reply_to,
                post.reply_to_author,
                post.quote_of,
                post.quote_of_author,
                post.signature,
            ],
        )?;
        Ok(())
    }

    pub fn get_post_by_id(&self, id: &str) -> anyhow::Result<Option<Post>> {
        let db = self.db.lock().unwrap();
        let mut stmt =
            db.prepare("SELECT id, author, content, timestamp, media_json, reply_to, reply_to_author, quote_of, quote_of_author, signature FROM posts WHERE id=?1")?;
        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(row) => Ok(Some(Self::row_to_post(row)?)),
            None => Ok(None),
        }
    }

    pub fn get_feed(&self, q: &FeedQuery) -> anyhow::Result<Vec<Post>> {
        let db = self.db.lock().unwrap();
        let hidden =
            "AND p.author NOT IN (SELECT pubkey FROM mutes UNION SELECT pubkey FROM blocks)";

        let (sql, p): (String, Vec<Box<dyn rusqlite::types::ToSql>>) = match q.before {
            Some(b) => (
                format!(
                    "SELECT p.id, p.author, p.content, p.timestamp, p.media_json, p.reply_to, p.reply_to_author, p.quote_of, p.quote_of_author, p.signature FROM posts p
                     WHERE p.timestamp < ?1 {hidden} ORDER BY p.timestamp DESC LIMIT ?2"
                ),
                vec![Box::new(b as i64), Box::new(q.limit as i64)],
            ),
            None => (
                format!(
                    "SELECT p.id, p.author, p.content, p.timestamp, p.media_json, p.reply_to, p.reply_to_author, p.quote_of, p.quote_of_author, p.signature FROM posts p
                     WHERE 1=1 {hidden} ORDER BY p.timestamp DESC LIMIT ?1"
                ),
                vec![Box::new(q.limit as i64)],
            ),
        };
        let mut stmt = db.prepare(&sql)?;
        let p_refs: Vec<&dyn rusqlite::types::ToSql> = p.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_and_then(p_refs.as_slice(), Self::row_to_post)?;
        let mut posts = Vec::new();
        for row in rows {
            posts.push(row?);
        }
        Ok(posts)
    }

    pub fn delete_post(&self, id: &str) -> anyhow::Result<bool> {
        let db = self.db.lock().unwrap();
        db.execute(
            "DELETE FROM notifications WHERE post_id=?1 OR target_post_id=?1",
            params![id],
        )?;
        let count = db.execute("DELETE FROM posts WHERE id=?1", params![id])?;
        Ok(count > 0)
    }

    pub fn delete_repost_by_target(
        &self,
        author: &str,
        quote_of: &str,
    ) -> anyhow::Result<Option<String>> {
        let db = self.db.lock().unwrap();
        let id: Option<String> = db
            .query_row(
                "SELECT id FROM posts WHERE author=?1 AND quote_of=?2",
                params![author, quote_of],
                |row| row.get(0),
            )
            .ok();
        if let Some(ref id) = id {
            db.execute("DELETE FROM posts WHERE id=?1", params![id])?;
        }
        Ok(id)
    }

    pub fn get_posts_by_author(
        &self,
        author: &str,
        limit: usize,
        before: Option<u64>,
        media_filter: Option<&str>,
    ) -> anyhow::Result<Vec<Post>> {
        let filter_clause = match media_filter {
            Some("images") => " AND media_json LIKE '%image/%'",
            Some("videos") => " AND media_json LIKE '%video/%'",
            Some("audio") => " AND media_json LIKE '%audio/%'",
            Some("files") => {
                " AND media_json != '[]' AND media_json NOT LIKE '%image/%' AND media_json NOT LIKE '%video/%' AND media_json NOT LIKE '%audio/%'"
            }
            Some("text") => " AND media_json = '[]'",
            _ => "",
        };

        let db = self.db.lock().unwrap();
        let mut posts = Vec::new();
        match before {
            Some(b) => {
                let sql = format!(
                    "SELECT id, author, content, timestamp, media_json, reply_to, reply_to_author, quote_of, quote_of_author, signature FROM posts
                     WHERE author=?1 AND timestamp < ?2{filter_clause} ORDER BY timestamp DESC LIMIT ?3"
                );
                let mut stmt = db.prepare(&sql)?;
                let mut rows = stmt.query(params![author, b as i64, limit as i64])?;
                while let Some(row) = rows.next()? {
                    posts.push(Self::row_to_post(row)?);
                }
            }
            None => {
                let sql = format!(
                    "SELECT id, author, content, timestamp, media_json, reply_to, reply_to_author, quote_of, quote_of_author, signature FROM posts
                     WHERE author=?1{filter_clause} ORDER BY timestamp DESC LIMIT ?2"
                );
                let mut stmt = db.prepare(&sql)?;
                let mut rows = stmt.query(params![author, limit as i64])?;
                while let Some(row) = rows.next()? {
                    posts.push(Self::row_to_post(row)?);
                }
            }
        }
        Ok(posts)
    }

    pub fn get_post_ids_by_author(&self, author: &str) -> anyhow::Result<Vec<String>> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare("SELECT id FROM posts WHERE author=?1 ORDER BY timestamp ASC")?;
        let mut rows = stmt.query(params![author])?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next()? {
            ids.push(row.get(0)?);
        }
        Ok(ids)
    }

    pub fn count_posts_by_author(&self, author: &str) -> anyhow::Result<u64> {
        let db = self.db.lock().unwrap();
        let count: i64 = db.query_row(
            "SELECT COUNT(*) FROM posts WHERE author=?1",
            params![author],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    pub fn newest_post_timestamp(&self, author: &str) -> anyhow::Result<u64> {
        let db = self.db.lock().unwrap();
        let ts: Option<i64> = db.query_row(
            "SELECT MAX(timestamp) FROM posts WHERE author=?1",
            params![author],
            |row| row.get(0),
        )?;
        Ok(ts.unwrap_or(0) as u64)
    }

    pub fn count_posts_after(&self, author: &str, after_ts: u64) -> anyhow::Result<u64> {
        let db = self.db.lock().unwrap();
        let count: i64 = db.query_row(
            "SELECT COUNT(*) FROM posts WHERE author=?1 AND timestamp > ?2",
            params![author, after_ts as i64],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    pub fn get_posts_after(
        &self,
        author: &str,
        after_ts: u64,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<Post>> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare(
            "SELECT id, author, content, timestamp, media_json, reply_to, reply_to_author, quote_of, quote_of_author, signature
             FROM posts WHERE author=?1 AND timestamp > ?2
             ORDER BY timestamp ASC LIMIT ?3 OFFSET ?4",
        )?;
        let mut rows = stmt.query(params![
            author,
            after_ts as i64,
            limit as i64,
            offset as i64
        ])?;
        let mut posts = Vec::new();
        while let Some(row) = rows.next()? {
            posts.push(Self::row_to_post(row)?);
        }
        Ok(posts)
    }

    pub fn get_posts_not_in(
        &self,
        author: &str,
        known_ids: &[String],
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<Post>> {
        let db = self.db.lock().unwrap();
        if known_ids.is_empty() {
            let mut stmt = db.prepare(
                "SELECT id, author, content, timestamp, media_json, reply_to, reply_to_author, quote_of, quote_of_author, signature
                 FROM posts WHERE author=?1
                 ORDER BY timestamp ASC LIMIT ?2 OFFSET ?3",
            )?;
            let mut rows = stmt.query(params![author, limit as i64, offset as i64])?;
            let mut posts = Vec::new();
            while let Some(row) = rows.next()? {
                posts.push(Self::row_to_post(row)?);
            }
            return Ok(posts);
        }

        db.execute_batch("CREATE TEMP TABLE IF NOT EXISTS _sync_known_ids (id TEXT PRIMARY KEY)")?;
        db.execute_batch("DELETE FROM _sync_known_ids")?;

        let mut insert = db.prepare("INSERT OR IGNORE INTO _sync_known_ids (id) VALUES (?1)")?;
        for id in known_ids {
            insert.execute(params![id])?;
        }
        drop(insert);

        let mut stmt = db.prepare(
            "SELECT p.id, p.author, p.content, p.timestamp, p.media_json, p.reply_to, p.reply_to_author, p.quote_of, p.quote_of_author, p.signature             FROM posts p
             WHERE p.author=?1 AND p.id NOT IN (SELECT id FROM _sync_known_ids)
             ORDER BY p.timestamp ASC LIMIT ?2 OFFSET ?3",
        )?;
        let mut rows = stmt.query(params![author, limit as i64, offset as i64])?;
        let mut posts = Vec::new();
        while let Some(row) = rows.next()? {
            posts.push(Self::row_to_post(row)?);
        }

        db.execute_batch("DROP TABLE IF EXISTS _sync_known_ids")?;
        Ok(posts)
    }

    pub fn get_replies(
        &self,
        parent_post_id: &str,
        limit: usize,
        before: Option<u64>,
    ) -> anyhow::Result<Vec<Post>> {
        let db = self.db.lock().unwrap();
        let hidden = "AND author NOT IN (SELECT pubkey FROM mutes UNION SELECT pubkey FROM blocks)";
        let mut posts = Vec::new();
        match before {
            Some(b) => {
                let sql = format!(
                    "SELECT id, author, content, timestamp, media_json, reply_to, reply_to_author, quote_of, quote_of_author, signature FROM posts
                     WHERE reply_to=?1 AND timestamp < ?2 {hidden} ORDER BY timestamp ASC LIMIT ?3"
                );
                let mut stmt = db.prepare(&sql)?;
                let mut rows = stmt.query(params![parent_post_id, b as i64, limit as i64])?;
                while let Some(row) = rows.next()? {
                    posts.push(Self::row_to_post(row)?);
                }
            }
            None => {
                let sql = format!(
                    "SELECT id, author, content, timestamp, media_json, reply_to, reply_to_author, quote_of, quote_of_author, signature FROM posts
                     WHERE reply_to=?1 {hidden} ORDER BY timestamp ASC LIMIT ?2"
                );
                let mut stmt = db.prepare(&sql)?;
                let mut rows = stmt.query(params![parent_post_id, limit as i64])?;
                while let Some(row) = rows.next()? {
                    posts.push(Self::row_to_post(row)?);
                }
            }
        }
        Ok(posts)
    }
}
