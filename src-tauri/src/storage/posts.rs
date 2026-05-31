use crate::error::AppError;
use proscenium_types::{MediaAttachment, Post};
use sqlx::Row;

use super::{FeedQuery, Storage};

impl Storage {
    fn row_to_post(row: &sqlx::sqlite::SqliteRow) -> Result<Post, AppError> {
        let media_json: String = row.get(4);
        let media: Vec<MediaAttachment> = serde_json::from_str(&media_json)?;
        Ok(Post {
            id: row.get(0),
            author: row.get(1),
            content: row.get(2),
            timestamp: row.get::<i64, _>(3) as u64,
            media,
            reply_to: row.get(5),
            reply_to_author: row.get(6),
            quote_of: row.get(7),
            quote_of_author: row.get(8),
            signature: row.get(9),
        })
    }

    pub async fn insert_post(&self, post: &Post) -> Result<(), AppError> {
        let media_json = serde_json::to_string(&post.media)?;
        sqlx::query(
            "INSERT OR IGNORE INTO posts (id, author, content, timestamp, media_json, reply_to, reply_to_author, quote_of, quote_of_author, signature)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )
        .bind(&post.id)
        .bind(&post.author)
        .bind(&post.content)
        .bind(post.timestamp as i64)
        .bind(&media_json)
        .bind(&post.reply_to)
        .bind(&post.reply_to_author)
        .bind(&post.quote_of)
        .bind(&post.quote_of_author)
        .bind(&post.signature)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_post_by_id(&self, id: &str) -> Result<Option<Post>, AppError> {
        let row = sqlx::query(
            "SELECT id, author, content, timestamp, media_json, reply_to, reply_to_author, quote_of, quote_of_author, signature FROM posts WHERE id=?1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(row) => Ok(Some(Self::row_to_post(&row)?)),
            None => Ok(None),
        }
    }

    pub async fn get_feed(&self, q: &FeedQuery) -> Result<Vec<Post>, AppError> {
        let hidden = "AND p.author NOT IN (SELECT pubkey FROM moderation WHERE state='active')";
        let rows = match q.before {
            Some(b) => {
                let sql = format!(
                    "SELECT p.id, p.author, p.content, p.timestamp, p.media_json, p.reply_to, p.reply_to_author, p.quote_of, p.quote_of_author, p.signature FROM posts p
                     WHERE p.timestamp < ?1 {hidden} ORDER BY p.timestamp DESC LIMIT ?2"
                );
                sqlx::query(sqlx::AssertSqlSafe(sql))
                    .bind(b as i64)
                    .bind(q.limit as i64)
                    .fetch_all(&self.pool)
                    .await?
            }
            None => {
                let sql = format!(
                    "SELECT p.id, p.author, p.content, p.timestamp, p.media_json, p.reply_to, p.reply_to_author, p.quote_of, p.quote_of_author, p.signature FROM posts p
                     WHERE 1=1 {hidden} ORDER BY p.timestamp DESC LIMIT ?1"
                );
                sqlx::query(sqlx::AssertSqlSafe(sql))
                    .bind(q.limit as i64)
                    .fetch_all(&self.pool)
                    .await?
            }
        };
        let mut posts = Vec::new();
        for row in &rows {
            posts.push(Self::row_to_post(row)?);
        }
        Ok(posts)
    }

    pub async fn delete_post(&self, id: &str) -> Result<bool, AppError> {
        sqlx::query("DELETE FROM notifications WHERE post_id=?1 OR target_post_id=?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        let result = sqlx::query("DELETE FROM posts WHERE id=?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn delete_posts_by_author(&self, author: &str) -> Result<u64, AppError> {
        sqlx::query(
            "DELETE FROM notifications WHERE post_id IN (SELECT id FROM posts WHERE author=?1) OR target_post_id IN (SELECT id FROM posts WHERE author=?1)",
        )
        .bind(author)
        .execute(&self.pool)
        .await?;
        let result = sqlx::query("DELETE FROM posts WHERE author=?1")
            .bind(author)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    pub async fn delete_repost_by_target(
        &self,
        author: &str,
        quote_of: &str,
    ) -> Result<Option<String>, AppError> {
        let id: Option<String> =
            sqlx::query_scalar("SELECT id FROM posts WHERE author=?1 AND quote_of=?2")
                .bind(author)
                .bind(quote_of)
                .fetch_optional(&self.pool)
                .await?;
        if let Some(ref id) = id {
            sqlx::query("DELETE FROM posts WHERE id=?1")
                .bind(id)
                .execute(&self.pool)
                .await?;
        }
        Ok(id)
    }

    pub async fn get_posts_by_author(
        &self,
        author: &str,
        limit: usize,
        before: Option<u64>,
        media_filter: Option<&str>,
    ) -> Result<Vec<Post>, AppError> {
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

        let rows = match before {
            Some(b) => {
                let sql = format!(
                    "SELECT id, author, content, timestamp, media_json, reply_to, reply_to_author, quote_of, quote_of_author, signature FROM posts
                     WHERE author=?1 AND timestamp < ?2{filter_clause} ORDER BY timestamp DESC LIMIT ?3"
                );
                sqlx::query(sqlx::AssertSqlSafe(sql))
                    .bind(author)
                    .bind(b as i64)
                    .bind(limit as i64)
                    .fetch_all(&self.pool)
                    .await?
            }
            None => {
                let sql = format!(
                    "SELECT id, author, content, timestamp, media_json, reply_to, reply_to_author, quote_of, quote_of_author, signature FROM posts
                     WHERE author=?1{filter_clause} ORDER BY timestamp DESC LIMIT ?2"
                );
                sqlx::query(sqlx::AssertSqlSafe(sql))
                    .bind(author)
                    .bind(limit as i64)
                    .fetch_all(&self.pool)
                    .await?
            }
        };
        let mut posts = Vec::new();
        for row in &rows {
            posts.push(Self::row_to_post(row)?);
        }
        Ok(posts)
    }

    pub async fn get_post_ids_by_author(&self, author: &str) -> Result<Vec<String>, AppError> {
        let rows = sqlx::query("SELECT id FROM posts WHERE author=?1 ORDER BY timestamp ASC")
            .bind(author)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    pub async fn count_posts_by_author(&self, author: &str) -> Result<u64, AppError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM posts WHERE author=?1")
            .bind(author)
            .fetch_one(&self.pool)
            .await?;
        Ok(count as u64)
    }

    pub async fn newest_post_timestamp(&self, author: &str) -> Result<u64, AppError> {
        let ts: Option<i64> =
            sqlx::query_scalar("SELECT MAX(timestamp) FROM posts WHERE author=?1")
                .bind(author)
                .fetch_one(&self.pool)
                .await?;
        Ok(ts.unwrap_or(0) as u64)
    }

    pub async fn count_posts_after(&self, author: &str, after_ts: u64) -> Result<u64, AppError> {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM posts WHERE author=?1 AND timestamp > ?2")
                .bind(author)
                .bind(after_ts as i64)
                .fetch_one(&self.pool)
                .await?;
        Ok(count as u64)
    }

    pub async fn get_posts_after(
        &self,
        author: &str,
        after_ts: u64,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<Post>, AppError> {
        let rows = sqlx::query(
            "SELECT id, author, content, timestamp, media_json, reply_to, reply_to_author, quote_of, quote_of_author, signature
             FROM posts WHERE author=?1 AND timestamp > ?2
             ORDER BY timestamp ASC LIMIT ?3 OFFSET ?4",
        )
        .bind(author)
        .bind(after_ts as i64)
        .bind(limit as i64)
        .bind(offset as i64)
        .fetch_all(&self.pool)
        .await?;
        let mut posts = Vec::new();
        for row in &rows {
            posts.push(Self::row_to_post(row)?);
        }
        Ok(posts)
    }

    pub async fn get_posts_not_in(
        &self,
        author: &str,
        known_ids: &[String],
        limit: usize,
        offset: usize,
    ) -> Result<Vec<Post>, AppError> {
        if known_ids.is_empty() {
            let rows = sqlx::query(
                "SELECT id, author, content, timestamp, media_json, reply_to, reply_to_author, quote_of, quote_of_author, signature
                 FROM posts WHERE author=?1
                 ORDER BY timestamp ASC LIMIT ?2 OFFSET ?3",
            )
            .bind(author)
            .bind(limit as i64)
            .bind(offset as i64)
            .fetch_all(&self.pool)
            .await?;
            let mut posts = Vec::new();
            for row in &rows {
                posts.push(Self::row_to_post(row)?);
            }
            return Ok(posts);
        }

        // Use a dedicated connection for temp table operations
        let mut conn = self.pool.acquire().await?;

        sqlx::query("CREATE TEMP TABLE IF NOT EXISTS _sync_known_ids (id TEXT PRIMARY KEY)")
            .execute(&mut *conn)
            .await?;
        sqlx::query("DELETE FROM _sync_known_ids")
            .execute(&mut *conn)
            .await?;

        for id in known_ids {
            sqlx::query("INSERT OR IGNORE INTO _sync_known_ids (id) VALUES (?1)")
                .bind(id)
                .execute(&mut *conn)
                .await?;
        }

        let rows = sqlx::query(
            "SELECT p.id, p.author, p.content, p.timestamp, p.media_json, p.reply_to, p.reply_to_author, p.quote_of, p.quote_of_author, p.signature FROM posts p
             WHERE p.author=?1 AND p.id NOT IN (SELECT id FROM _sync_known_ids)
             ORDER BY p.timestamp ASC LIMIT ?2 OFFSET ?3",
        )
        .bind(author)
        .bind(limit as i64)
        .bind(offset as i64)
        .fetch_all(&mut *conn)
        .await?;
        let mut posts = Vec::new();
        for row in &rows {
            posts.push(Self::row_to_post(row)?);
        }

        sqlx::query("DROP TABLE IF EXISTS _sync_known_ids")
            .execute(&mut *conn)
            .await?;
        Ok(posts)
    }

    pub async fn get_replies(
        &self,
        parent_post_id: &str,
        limit: usize,
        before: Option<u64>,
    ) -> Result<Vec<Post>, AppError> {
        let hidden = "AND author NOT IN (SELECT pubkey FROM moderation WHERE state='active')";
        let rows = match before {
            Some(b) => {
                let sql = format!(
                    "SELECT id, author, content, timestamp, media_json, reply_to, reply_to_author, quote_of, quote_of_author, signature FROM posts
                     WHERE reply_to=?1 AND timestamp < ?2 {hidden} ORDER BY timestamp ASC LIMIT ?3"
                );
                sqlx::query(sqlx::AssertSqlSafe(sql))
                    .bind(parent_post_id)
                    .bind(b as i64)
                    .bind(limit as i64)
                    .fetch_all(&self.pool)
                    .await?
            }
            None => {
                let sql = format!(
                    "SELECT id, author, content, timestamp, media_json, reply_to, reply_to_author, quote_of, quote_of_author, signature FROM posts
                     WHERE reply_to=?1 {hidden} ORDER BY timestamp ASC LIMIT ?2"
                );
                sqlx::query(sqlx::AssertSqlSafe(sql))
                    .bind(parent_post_id)
                    .bind(limit as i64)
                    .fetch_all(&self.pool)
                    .await?
            }
        };
        let mut posts = Vec::new();
        for row in &rows {
            posts.push(Self::row_to_post(row)?);
        }
        Ok(posts)
    }
}
