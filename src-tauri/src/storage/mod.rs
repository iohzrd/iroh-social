use crate::error::AppError;
mod device_sync;
mod follow_requests;
mod interactions;
mod linked_devices;
mod messaging;
mod moderation;
mod notifications;
mod peer_delegations;
mod posts;
mod preferences;
mod profiles;
mod push;
mod ratchet;
mod servers;
mod social;
#[cfg(test)]
mod tests;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::path::Path;
use std::str::FromStr;

pub struct FeedQuery {
    pub limit: usize,
    pub before: Option<u64>,
}

pub struct Storage {
    pool: SqlitePool,
}

impl std::fmt::Debug for Storage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Storage").finish()
    }
}

impl Storage {
    const MIGRATIONS: &'static [(&'static str, &'static str)] = &[
        (
            "001_profiles",
            include_str!("../../migrations/001_profiles.sql"),
        ),
        (
            "002_peer_delegations",
            include_str!("../../migrations/002_peer_delegations.sql"),
        ),
        (
            "003_linked_devices",
            include_str!("../../migrations/003_linked_devices.sql"),
        ),
        (
            "004_social_graph",
            include_str!("../../migrations/004_social_graph.sql"),
        ),
        (
            "005_content",
            include_str!("../../migrations/005_content.sql"),
        ),
        (
            "006_notifications",
            include_str!("../../migrations/006_notifications.sql"),
        ),
        (
            "007_direct_messages",
            include_str!("../../migrations/007_direct_messages.sql"),
        ),
        (
            "008_bookmarks",
            include_str!("../../migrations/008_bookmarks.sql"),
        ),
        (
            "009_servers",
            include_str!("../../migrations/009_servers.sql"),
        ),
        (
            "010_push_outbox",
            include_str!("../../migrations/010_push_outbox.sql"),
        ),
        (
            "011_preferences",
            include_str!("../../migrations/011_preferences.sql"),
        ),
    ];

    pub async fn open(path: impl AsRef<Path>) -> Result<Self, AppError> {
        let url = format!("sqlite:{}?mode=rwc", path.as_ref().display());
        let opts = SqliteConnectOptions::from_str(&url)?
            .pragma("journal_mode", "WAL")
            .pragma("foreign_keys", "ON")
            .pragma("busy_timeout", "5000");

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await?;

        Self::run_migrations(&pool).await?;
        Ok(Self { pool })
    }

    /// Create an in-memory storage instance for testing.
    #[cfg(test)]
    pub async fn open_in_memory() -> Result<Self, AppError> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")?.pragma("foreign_keys", "ON");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        Self::run_migrations(&pool).await?;
        Ok(Self { pool })
    }

    /// Delete all user data from every table (except schema_migrations).
    pub async fn wipe_all_data(&self) -> Result<(), AppError> {
        let tables = [
            "push_outbox",
            "dm_outbox",
            "dm_ratchet_sessions",
            "dm_messages",
            "dm_conversations",
            "notifications",
            "interactions",
            "posts",
            "bookmarks",
            "follow_requests",
            "moderation",
            "remote_social_meta",
            "social_graph",
            "peer_device_announcements",
            "linked_devices",
            "peer_delegations",
            "profiles",
            "servers",
            "preferences",
        ];
        for table in tables {
            let sql = format!("DELETE FROM {table}");
            sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    async fn run_migrations(pool: &SqlitePool) -> Result<(), AppError> {
        sqlx::raw_sql(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                name TEXT PRIMARY KEY,
                applied_at INTEGER NOT NULL
            )",
        )
        .execute(pool)
        .await?;

        for (name, sql) in Self::MIGRATIONS {
            let already_applied: bool =
                sqlx::query_scalar("SELECT COUNT(*) > 0 FROM schema_migrations WHERE name=?1")
                    .bind(name)
                    .fetch_one(pool)
                    .await?;
            if !already_applied {
                println!("[storage] applying migration: {name}");
                sqlx::raw_sql(sqlx::AssertSqlSafe(*sql))
                    .execute(pool)
                    .await?;
                sqlx::query(
                    "INSERT INTO schema_migrations (name, applied_at) VALUES (?1, strftime('%s', 'now'))",
                )
                .bind(name)
                .execute(pool)
                .await?;
            }
        }
        Ok(())
    }
}
