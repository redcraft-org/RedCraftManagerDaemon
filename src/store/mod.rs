//! The database.
//!
//! rcmd keeps runtime state here and nothing else. What artifacts exist,
//! what a template pins and which servers there are is configuration, and
//! configuration lives in git where it can be read and reviewed. The
//! database holds what happened: which versions were fetched, what a build
//! resolved them to, and what is deployed where.
//!
//! SQLite by default because a single server should not need to install a
//! database to run one binary. MySQL when you already have one, which on a
//! Minecraft host you usually do.
//!
//! The schema is written to work on both, so timestamps are ISO strings and
//! there is no AUTOINCREMENT anywhere.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::any::{AnyPoolOptions, AnyRow};
use sqlx::{AnyPool, Row};

use crate::artifacts::store::{Manifest, StoreError, VersionRecord};
use crate::catalog::Kind;

#[derive(Debug, thiserror::Error)]
pub enum DatabaseError {
    #[error("could not connect to the database: {0}")]
    Connect(String),
    #[error("could not create the schema: {0}")]
    Schema(String),
}

pub struct Database {
    pool: AnyPool,
}

impl Database {
    /// Opens the database and makes sure the schema is there.
    ///
    /// A SQLite url pointing at a file whose directory does not exist yet
    /// is the normal first run, so the directory is created rather than
    /// reported as a failure.
    pub async fn connect(url: &str) -> Result<Self, DatabaseError> {
        sqlx::any::install_default_drivers();

        // sqlite urls come in two shapes: `sqlite:/path/to.db` and
        // `sqlite:///path/to.db`. In-memory is `sqlite::memory:` and has no
        // path at all.
        if let Some(rest) = url.strip_prefix("sqlite:") {
            let path = rest.trim_start_matches('/');
            let path = path.split('?').next().unwrap_or(path);
            if !path.is_empty() && !rest.starts_with(":memory:") {
                let path = format!("/{path}");
                let path = path.as_str();
                if let Some(parent) = std::path::Path::new(path).parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if !std::path::Path::new(path).exists() {
                    let _ = std::fs::File::create(path);
                }
            }
        }

        // An in-memory sqlite database belongs to the connection that
        // opened it, so a pool of five gives you five empty databases and a
        // very confusing "no such table". One connection is the only
        // correct pool size for it.
        let in_memory = url.contains(":memory:");
        let pool = AnyPoolOptions::new()
            .max_connections(if in_memory { 1 } else { 5 })
            .connect(url)
            .await
            .map_err(|e| DatabaseError::Connect(e.to_string()))?;

        let database = Self { pool };
        database.migrate().await?;
        Ok(database)
    }

    async fn migrate(&self) -> Result<(), DatabaseError> {
        let statements = [
            "CREATE TABLE IF NOT EXISTS artifact_versions (
                artifact VARCHAR(190) NOT NULL,
                version VARCHAR(190) NOT NULL,
                kind VARCHAR(16) NOT NULL,
                sha256 VARCHAR(64) NOT NULL,
                size_bytes BIGINT NOT NULL,
                storage_key VARCHAR(255) NOT NULL,
                upstream_id VARCHAR(190),
                source_url TEXT NOT NULL,
                recorded_at VARCHAR(40) NOT NULL,
                PRIMARY KEY (artifact, version)
            )",
            "CREATE TABLE IF NOT EXISTS template_builds (
                id VARCHAR(64) NOT NULL,
                template VARCHAR(190) NOT NULL,
                built_at VARCHAR(40) NOT NULL,
                tar_sha256 VARCHAR(64),
                resolved TEXT NOT NULL,
                PRIMARY KEY (id)
            )",
            "CREATE TABLE IF NOT EXISTS deployments (
                id VARCHAR(64) NOT NULL,
                server VARCHAR(190) NOT NULL,
                build_id VARCHAR(64),
                applied_at VARCHAR(40) NOT NULL,
                trigger_reason VARCHAR(40) NOT NULL,
                PRIMARY KEY (id)
            )",
            "CREATE TABLE IF NOT EXISTS audit_log (
                id VARCHAR(64) NOT NULL,
                at VARCHAR(40) NOT NULL,
                actor VARCHAR(190) NOT NULL,
                surface VARCHAR(32) NOT NULL,
                action VARCHAR(64) NOT NULL,
                target VARCHAR(190),
                decision VARCHAR(16) NOT NULL,
                detail TEXT,
                PRIMARY KEY (id)
            )",
            "CREATE TABLE IF NOT EXISTS api_tokens (
                id VARCHAR(64) NOT NULL,
                name VARCHAR(190) NOT NULL,
                token_sha256 VARCHAR(64) NOT NULL,
                token_prefix VARCHAR(32),
                role VARCHAR(16) NOT NULL,
                created_at VARCHAR(40) NOT NULL,
                last_used_at VARCHAR(40),
                revoked_at VARCHAR(40),
                PRIMARY KEY (id)
            )",
        ];
        for statement in statements {
            sqlx::query(statement)
                .execute(&self.pool)
                .await
                .map_err(|e| DatabaseError::Schema(e.to_string()))?;
        }
        Ok(())
    }

    pub fn pool(&self) -> &AnyPool {
        &self.pool
    }
}

fn row_to_record(row: &AnyRow) -> VersionRecord {
    let kind: String = row.get("kind");
    let recorded: String = row.get("recorded_at");
    VersionRecord {
        artifact: row.get("artifact"),
        kind: match kind.as_str() {
            "engine" => Kind::Engine,
            "mod" => Kind::Mod,
            _ => Kind::Plugin,
        },
        version: row.get("version"),
        sha256: row.get("sha256"),
        size_bytes: row.get::<i64, _>("size_bytes") as u64,
        key: row.get("storage_key"),
        upstream_id: row.get("upstream_id"),
        source_url: row.get("source_url"),
        recorded_at: recorded
            .parse::<DateTime<Utc>>()
            .unwrap_or_else(|_| Utc::now()),
    }
}

fn kind_str(kind: Kind) -> &'static str {
    match kind {
        Kind::Engine => "engine",
        Kind::Plugin => "plugin",
        Kind::Mod => "mod",
    }
}

#[async_trait]
impl Manifest for Database {
    async fn record(&self, record: &VersionRecord) -> Result<(), StoreError> {
        // A re-fetch of the same version replaces the row rather than
        // failing, because a source rebuilding a release under the same
        // number is normal and the sha is what we actually compare.
        sqlx::query("DELETE FROM artifact_versions WHERE artifact = ? AND version = ?")
            .bind(&record.artifact)
            .bind(&record.version)
            .execute(&self.pool)
            .await
            .map_err(|e| StoreError::Database(e.to_string()))?;

        sqlx::query(
            "INSERT INTO artifact_versions
             (artifact, version, kind, sha256, size_bytes, storage_key, upstream_id, source_url, recorded_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&record.artifact)
        .bind(&record.version)
        .bind(kind_str(record.kind))
        .bind(&record.sha256)
        .bind(record.size_bytes as i64)
        .bind(&record.key)
        .bind(&record.upstream_id)
        .bind(&record.source_url)
        .bind(record.recorded_at.to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Database(e.to_string()))?;
        Ok(())
    }

    async fn has_version(
        &self,
        artifact: &str,
        version: &str,
    ) -> Result<Option<VersionRecord>, StoreError> {
        let row = sqlx::query("SELECT * FROM artifact_versions WHERE artifact = ? AND version = ?")
            .bind(artifact)
            .bind(version)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| StoreError::Database(e.to_string()))?;
        Ok(row.as_ref().map(row_to_record))
    }

    async fn versions(&self, artifact: &str) -> Result<Vec<VersionRecord>, StoreError> {
        let rows = sqlx::query(
            "SELECT * FROM artifact_versions WHERE artifact = ? ORDER BY recorded_at ASC",
        )
        .bind(artifact)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| StoreError::Database(e.to_string()))?;
        Ok(rows.iter().map(row_to_record).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn memory() -> Database {
        Database::connect("sqlite::memory:").await.unwrap()
    }

    fn record(artifact: &str, version: &str) -> VersionRecord {
        VersionRecord {
            artifact: artifact.into(),
            kind: Kind::Plugin,
            version: version.into(),
            sha256: format!("sha-{version}"),
            size_bytes: 42,
            key: format!("{artifact}-{version}.jar"),
            upstream_id: Some("v1".into()),
            source_url: "https://example.invalid/x.jar".into(),
            recorded_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn a_record_round_trips() {
        let db = memory().await;
        db.record(&record("CMI", "9.8.1")).await.unwrap();
        let found = db.has_version("CMI", "9.8.1").await.unwrap().unwrap();
        assert_eq!(found.version, "9.8.1");
        assert_eq!(found.size_bytes, 42);
        assert_eq!(found.kind, Kind::Plugin);
        assert!(db.has_version("CMI", "9.9.9").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn refetching_a_version_replaces_it_instead_of_failing() {
        let db = memory().await;
        db.record(&record("CMI", "9.8.1")).await.unwrap();
        let mut rebuilt = record("CMI", "9.8.1");
        rebuilt.sha256 = "different".into();
        db.record(&rebuilt).await.unwrap();

        assert_eq!(db.versions("CMI").await.unwrap().len(), 1);
        assert_eq!(
            db.has_version("CMI", "9.8.1")
                .await
                .unwrap()
                .unwrap()
                .sha256,
            "different"
        );
    }

    #[tokio::test]
    async fn resolution_over_the_database_matches_the_pin_rules() {
        let db = memory().await;
        for version in ["5.5.40", "5.5.44", "5.6.0"] {
            db.record(&record("LuckPerms", version)).await.unwrap();
            // The recorded_at ordering is what "newest" means.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let picked = db.resolve("LuckPerms", "5.5.*").await.unwrap().unwrap();
        assert_eq!(picked.version, "5.5.44");
    }

    #[tokio::test]
    async fn an_in_memory_database_is_still_one_database() {
        // Each connection to :memory: gets its own, so a pool bigger than
        // one migrates a database nobody then queries.
        let db = memory().await;
        db.record(&record("A", "1")).await.unwrap();
        for _ in 0..10 {
            assert!(db.has_version("A", "1").await.unwrap().is_some());
        }
    }

    #[tokio::test]
    async fn the_schema_is_created_more_than_once_without_complaining() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}/rcmd.db", dir.path().display());
        let first = Database::connect(&url).await.unwrap();
        first.record(&record("Thing", "1.0")).await.unwrap();
        drop(first);
        // Reopening an existing database must not wipe or fail.
        let second = Database::connect(&url).await.unwrap();
        assert!(second.has_version("Thing", "1.0").await.unwrap().is_some());
    }
}
