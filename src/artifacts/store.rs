//! Where the jars live and what we know about them.
//!
//! Two traits, because they answer different questions. The store holds
//! bytes and can be a directory or a bucket. The manifest holds facts and
//! is the thing that replaces reading a version out of a filename.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::catalog::Kind;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io error at {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("{0} is not in the store")]
    Missing(String),
    #[error("database error: {0}")]
    Database(String),
}

/// One version of one artifact, as recorded when it was stored.
#[derive(Debug, Clone, Serialize)]
pub struct VersionRecord {
    pub artifact: String,
    pub kind: Kind,
    pub version: String,
    pub sha256: String,
    pub size_bytes: u64,
    /// The name the bytes are stored under.
    pub key: String,
    /// What the source called this release, if it said.
    pub upstream_id: Option<String>,
    pub source_url: String,
    pub recorded_at: DateTime<Utc>,
}

#[async_trait]
pub trait ArtifactStore: Send + Sync {
    async fn put(&self, key: &str, bytes: &[u8]) -> Result<(), StoreError>;
    async fn get(&self, key: &str) -> Result<Vec<u8>, StoreError>;
    async fn exists(&self, key: &str) -> Result<bool, StoreError>;
}

#[async_trait]
pub trait Manifest: Send + Sync {
    async fn record(&self, record: &VersionRecord) -> Result<(), StoreError>;
    async fn has_version(
        &self,
        artifact: &str,
        version: &str,
    ) -> Result<Option<VersionRecord>, StoreError>;
    async fn versions(&self, artifact: &str) -> Result<Vec<VersionRecord>, StoreError>;

    /// The newest version of an artifact matching a pin.
    ///
    /// Newest means the most recently recorded, not the highest number.
    /// Version strings across sixty plugins do not share a scheme, and
    /// comparing `1.21.4-SNAPSHOT-b31` against `26.2` is not something a
    /// sort order can do honestly. What the manifest can say for certain
    /// is which one arrived last.
    async fn resolve(
        &self,
        artifact: &str,
        pin: &str,
    ) -> Result<Option<VersionRecord>, StoreError> {
        let mut matching: Vec<VersionRecord> = self
            .versions(artifact)
            .await?
            .into_iter()
            .filter(|record| crate::catalog::pin_matches(pin, &record.version))
            .collect();
        matching.sort_by_key(|record| record.recorded_at);
        Ok(matching.pop())
    }
}

/// Jars in a directory. The default, and all a single server needs.
pub struct DirectoryStore {
    root: PathBuf,
}

impl DirectoryStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Keys are filenames and nothing else, so a catalog entry can never
    /// write outside the store.
    fn path_for(&self, key: &str) -> Result<PathBuf, StoreError> {
        if key.contains('/') || key.contains('\\') || key.contains("..") {
            return Err(StoreError::Missing(format!("{key:?} is not a valid key")));
        }
        Ok(self.root.join(key))
    }
}

#[async_trait]
impl ArtifactStore for DirectoryStore {
    async fn put(&self, key: &str, bytes: &[u8]) -> Result<(), StoreError> {
        let path = self.path_for(key)?;
        let root = self.root.clone();
        let bytes = bytes.to_vec();
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&root).map_err(|source| StoreError::Io {
                path: root.display().to_string(),
                source,
            })?;
            // Write beside the target and rename, so an interrupted write
            // never leaves a half a jar under a name that looks complete.
            let temporary = path.with_extension("partial");
            std::fs::write(&temporary, &bytes).map_err(|source| StoreError::Io {
                path: temporary.display().to_string(),
                source,
            })?;
            std::fs::rename(&temporary, &path).map_err(|source| StoreError::Io {
                path: path.display().to_string(),
                source,
            })
        })
        .await
        .map_err(|e| StoreError::Database(e.to_string()))?
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>, StoreError> {
        let path = self.path_for(key)?;
        tokio::task::spawn_blocking(move || {
            std::fs::read(&path).map_err(|source| StoreError::Io {
                path: path.display().to_string(),
                source,
            })
        })
        .await
        .map_err(|e| StoreError::Database(e.to_string()))?
    }

    async fn exists(&self, key: &str) -> Result<bool, StoreError> {
        Ok(self.path_for(key)?.exists())
    }
}

/// Copies an artifact out of the store into a built template.
pub async fn place(
    store: &dyn ArtifactStore,
    key: &str,
    destination: &Path,
) -> Result<u64, StoreError> {
    let bytes = store.get(key).await?;
    let destination = destination.to_path_buf();
    let length = bytes.len() as u64;
    tokio::task::spawn_blocking(move || {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::Io {
                path: parent.display().to_string(),
                source,
            })?;
        }
        std::fs::write(&destination, &bytes).map_err(|source| StoreError::Io {
            path: destination.display().to_string(),
            source,
        })
    })
    .await
    .map_err(|e| StoreError::Database(e.to_string()))??;
    Ok(length)
}

/// An in memory manifest, so the resolution rules and everything built on
/// them can be tested without a database.
#[cfg(test)]
pub mod tests_support {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct MemoryManifest {
        pub records: Mutex<Vec<VersionRecord>>,
    }

    #[async_trait]
    impl Manifest for MemoryManifest {
        async fn record(&self, record: &VersionRecord) -> Result<(), StoreError> {
            self.records.lock().unwrap().push(record.clone());
            Ok(())
        }
        async fn has_version(
            &self,
            artifact: &str,
            version: &str,
        ) -> Result<Option<VersionRecord>, StoreError> {
            Ok(self
                .records
                .lock()
                .unwrap()
                .iter()
                .find(|r| r.artifact == artifact && r.version == version)
                .cloned())
        }
        async fn versions(&self, artifact: &str) -> Result<Vec<VersionRecord>, StoreError> {
            Ok(self
                .records
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.artifact == artifact)
                .cloned()
                .collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::MemoryManifest;
    use super::*;

    fn record(version: &str, minutes: i64) -> VersionRecord {
        VersionRecord {
            artifact: "LuckPerms".into(),
            kind: Kind::Plugin,
            version: version.into(),
            sha256: format!("sha-of-{version}"),
            size_bytes: 1,
            key: format!("LuckPerms-{version}.jar"),
            upstream_id: None,
            source_url: "https://example.invalid".into(),
            recorded_at: Utc::now() + chrono::Duration::minutes(minutes),
        }
    }

    #[tokio::test]
    async fn resolve_picks_the_newest_match_for_the_pin() {
        let manifest = MemoryManifest::default();
        manifest.record(&record("5.5.40", 0)).await.unwrap();
        manifest.record(&record("5.5.44", 1)).await.unwrap();
        manifest.record(&record("5.6.0", 2)).await.unwrap();

        let picked = manifest
            .resolve("LuckPerms", "5.5.*")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(picked.version, "5.5.44");
        let picked = manifest.resolve("LuckPerms", "*").await.unwrap().unwrap();
        assert_eq!(picked.version, "5.6.0");
    }

    #[tokio::test]
    async fn newest_means_most_recently_recorded_not_highest() {
        // A rebuild of an older release arriving late is what the pin
        // resolves to, which is worth knowing when a rollback looks odd.
        let manifest = MemoryManifest::default();
        manifest.record(&record("5.5.44", 0)).await.unwrap();
        manifest.record(&record("5.5.40", 5)).await.unwrap();
        let picked = manifest
            .resolve("LuckPerms", "5.5.*")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(picked.version, "5.5.40");
    }

    #[tokio::test]
    async fn a_pin_matching_nothing_resolves_to_nothing() {
        let manifest = MemoryManifest::default();
        manifest.record(&record("5.5.44", 0)).await.unwrap();
        assert!(manifest
            .resolve("LuckPerms", "9.*")
            .await
            .unwrap()
            .is_none());
        assert!(manifest.resolve("Nothing", "*").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_key_may_not_climb_out_of_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = DirectoryStore::new(dir.path());
        assert!(store.put("../escaped.jar", b"x").await.is_err());
        assert!(store.put("nested/thing.jar", b"x").await.is_err());
        store.put("Fine-1.0.jar", b"x").await.unwrap();
        assert!(store.exists("Fine-1.0.jar").await.unwrap());
    }

    #[tokio::test]
    async fn writes_are_atomic_enough_to_not_leave_half_a_jar() {
        let dir = tempfile::tempdir().unwrap();
        let store = DirectoryStore::new(dir.path());
        store.put("Thing-1.0.jar", b"complete").await.unwrap();
        assert_eq!(store.get("Thing-1.0.jar").await.unwrap(), b"complete");
        // No leftovers from the rename dance.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains("partial"))
            .collect();
        assert!(leftovers.is_empty());
    }
}
