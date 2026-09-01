//! Fetching artifacts and recording what was fetched.
//!
//! An artifact store keeps the jars themselves; the manifest keeps what
//! they are. Everything downstream asks the manifest, so nothing else ever
//! has to guess a version out of a filename again.

pub mod jarmeta;
pub mod sources;
pub mod store;

use crate::catalog::{Artifact, PostProcessor};
use jarmeta::JarMeta;

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error(transparent)]
    Source(#[from] sources::SourceError),
    #[error(transparent)]
    Meta(#[from] jarmeta::MetaError),
    #[error(transparent)]
    Store(#[from] store::StoreError),
    #[error("{0}")]
    NoVersion(String),
}

/// What one artifact fetch did. Every fetch produces one of these, success
/// or failure, so a run that touched sixty artifacts can report on all
/// sixty instead of stopping at the first broken one.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FetchOutcome {
    pub name: String,
    pub succeeded: bool,
    /// What happened, for the run report: `unchanged`, `stored` or
    /// `failed`.
    pub action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl FetchOutcome {
    fn failed(name: &str, error: impl std::fmt::Display) -> Self {
        Self {
            name: name.to_string(),
            succeeded: false,
            action: "failed",
            version: None,
            sha256: None,
            error: Some(error.to_string()),
        }
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Runs the post processors and works out the version.
///
/// The chain is ordered: an unzip has to happen before whatever reads the
/// jar inside it.
pub fn process(artifact: &Artifact, mut bytes: Vec<u8>) -> Result<(Vec<u8>, JarMeta), FetchError> {
    let mut meta: Option<JarMeta> = None;

    for processor in &artifact.post_processors {
        match processor {
            PostProcessor::Unzip => {
                let filter = artifact.archive_filter.as_deref().unwrap_or("*");
                bytes = jarmeta::extract_from_zip(&bytes, filter)?;
            }
            PostProcessor::Plugin => meta = Some(jarmeta::read_plugin_meta(&bytes)?),
            PostProcessor::FabricMod => meta = Some(jarmeta::read_fabric_meta(&bytes)?),
            PostProcessor::VersionJson => meta = Some(jarmeta::read_version_json(&bytes)?),
            PostProcessor::FabricInstaller => {
                meta = Some(jarmeta::read_installer_properties(&bytes)?)
            }
        }
    }

    let meta = meta.ok_or_else(|| {
        FetchError::NoVersion(format!(
            "{} has no post processor that reads a version",
            artifact.name
        ))
    })?;
    Ok((bytes, meta))
}

/// Fetches one artifact if it is not already stored.
///
/// The sha decides. Sources lie about versions, rebuild the same release
/// under a new tag, or serve a "latest" URL that has not moved in a year;
/// the bytes are the only thing that settles it.
pub async fn fetch_one(
    resolver: &sources::Resolver,
    store: &dyn store::ArtifactStore,
    manifest: &dyn store::Manifest,
    artifact: &Artifact,
) -> FetchOutcome {
    let release = match resolver.resolve(artifact).await {
        Ok(release) => release,
        Err(e) => return FetchOutcome::failed(&artifact.name, e),
    };

    let bytes = match resolver.download(&release.url).await {
        Ok(bytes) => bytes,
        Err(e) => return FetchOutcome::failed(&artifact.name, e),
    };

    let (bytes, meta) = match process(artifact, bytes) {
        Ok(pair) => pair,
        Err(e) => return FetchOutcome::failed(&artifact.name, e),
    };
    match store_bytes(
        store,
        manifest,
        artifact,
        &meta.version,
        &bytes,
        &release.url,
        release.upstream_id.as_deref(),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(e) => FetchOutcome::failed(&artifact.name, e),
    }
}

/// Records bytes we already have as a version of an artifact.
///
/// Shared by the download path and the upload one, so a jar that arrived by
/// hand is stored, deduplicated and pinned exactly like one that arrived on
/// its own. The comparison is the sha, not the version string: a source
/// rebuilding a release under the same number is normal, and the only
/// honest way to tell whether anything changed is to look at the bytes.
pub async fn store_bytes(
    store: &dyn store::ArtifactStore,
    manifest: &dyn store::Manifest,
    artifact: &Artifact,
    version: &str,
    bytes: &[u8],
    source_url: &str,
    upstream_id: Option<&str>,
) -> Result<FetchOutcome, store::StoreError> {
    let sha = sha256_hex(bytes);

    if let Some(existing) = manifest.has_version(&artifact.name, version).await? {
        if existing.sha256 == sha {
            return Ok(FetchOutcome {
                name: artifact.name.clone(),
                succeeded: true,
                action: "unchanged",
                version: Some(version.to_string()),
                sha256: Some(sha),
                error: None,
            });
        }
    }

    let key = format!("{}-{}.jar", artifact.name, version);
    store.put(&key, bytes).await?;

    let record = store::VersionRecord {
        artifact: artifact.name.clone(),
        kind: artifact.kind,
        version: version.to_string(),
        sha256: sha.clone(),
        size_bytes: bytes.len() as u64,
        key,
        upstream_id: upstream_id.map(str::to_string),
        source_url: source_url.to_string(),
        recorded_at: chrono::Utc::now(),
    };
    manifest.record(&record).await?;

    tracing::info!("stored {} {} ({})", artifact.name, version, &sha[..12]);
    Ok(FetchOutcome {
        name: artifact.name.clone(),
        succeeded: true,
        action: "stored",
        version: Some(version.to_string()),
        sha256: Some(sha),
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::store::tests_support::MemoryManifest;
    use crate::artifacts::store::{ArtifactStore, DirectoryStore, Manifest};

    fn uploadable() -> Artifact {
        Artifact {
            name: "CMI".into(),
            kind: Kind::Plugin,
            source: SourceKind::Direct,
            url: "https://example.invalid/cmi.jar".into(),
            file_filter: None,
            archive_filter: None,
            post_processors: vec![],
            enabled: true,
        }
    }

    #[tokio::test]
    async fn a_jar_uploaded_twice_is_only_stored_once() {
        // The premium plugins arrive by hand, and somebody pasting the same
        // curl twice should not fill the manifest with duplicates.
        let dir = tempfile::tempdir().unwrap();
        let store = DirectoryStore::new(dir.path());
        let manifest = MemoryManifest::default();
        let artifact = uploadable();

        let first = store_bytes(
            &store,
            &manifest,
            &artifact,
            "9.8.1",
            b"jar bytes",
            "by hand",
            None,
        )
        .await
        .unwrap();
        assert_eq!(first.action, "stored");

        let again = store_bytes(
            &store,
            &manifest,
            &artifact,
            "9.8.1",
            b"jar bytes",
            "by hand",
            None,
        )
        .await
        .unwrap();
        assert_eq!(again.action, "unchanged");
        assert_eq!(manifest.versions("CMI").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_same_version_with_different_bytes_replaces_it() {
        // A source that rebuilds a release under the same number is normal,
        // and the sha is the only honest way to notice.
        let dir = tempfile::tempdir().unwrap();
        let store = DirectoryStore::new(dir.path());
        let manifest = MemoryManifest::default();
        let artifact = uploadable();

        store_bytes(
            &store, &manifest, &artifact, "9.8.1", b"first", "by hand", None,
        )
        .await
        .unwrap();
        let rebuilt = store_bytes(
            &store, &manifest, &artifact, "9.8.1", b"second", "by hand", None,
        )
        .await
        .unwrap();
        assert_eq!(rebuilt.action, "stored");
        assert_eq!(store.get("CMI-9.8.1.jar").await.unwrap(), b"second");
    }

    use crate::catalog::{Kind, SourceKind};
    use std::io::{Cursor, Write};

    fn artifact(post: Vec<PostProcessor>, archive_filter: Option<&str>) -> Artifact {
        Artifact {
            name: "Thing".into(),
            kind: Kind::Plugin,
            source: SourceKind::Direct,
            url: "https://example.invalid/thing.jar".into(),
            file_filter: None,
            archive_filter: archive_filter.map(str::to_string),
            post_processors: post,
            enabled: true,
        }
    }

    fn jar(files: &[(&str, &str)]) -> Vec<u8> {
        let mut buffer = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buffer);
            for (name, contents) in files {
                writer
                    .start_file::<_, ()>(*name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                writer.write_all(contents.as_bytes()).unwrap();
            }
            writer.finish().unwrap();
        }
        buffer.into_inner()
    }

    #[test]
    fn unzip_runs_before_the_reader_that_needs_it() {
        let inner = jar(&[("plugin.yml", "name: Citizens\nversion: 2.0.35\n")]);
        let mut outer = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut outer);
            writer
                .start_file::<_, ()>(
                    "Citizens-2.0.35.jar",
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            writer.write_all(&inner).unwrap();
            writer.finish().unwrap();
        }
        let outer = outer.into_inner();

        let artifact = artifact(
            vec![PostProcessor::Unzip, PostProcessor::Plugin],
            Some("Citizens-*.jar"),
        );
        let (bytes, meta) = process(&artifact, outer).unwrap();
        assert_eq!(meta.version, "2.0.35");
        // The stored bytes are the inner jar, not the wrapper.
        assert_eq!(bytes, inner);
    }

    #[test]
    fn an_entry_with_no_version_reader_is_an_error_not_a_guess() {
        let artifact = artifact(vec![], None);
        let err = process(&artifact, jar(&[("a", "b")])).unwrap_err();
        assert!(matches!(err, FetchError::NoVersion(_)));
    }

    #[test]
    fn the_same_bytes_hash_the_same_way() {
        assert_eq!(sha256_hex(b"abc"), sha256_hex(b"abc"));
        assert_ne!(sha256_hex(b"abc"), sha256_hex(b"abd"));
        assert_eq!(sha256_hex(b"").len(), 64);
    }
}
