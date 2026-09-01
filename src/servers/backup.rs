//! Server backups: the whitelisted directories, compressed.
//!
//! Ported from rcsm/backup.go with one deliberate fix: entry names are
//! recorded relative to the server directory. rcsm stored the absolute walk
//! path in every header, which made its archives restore to literal
//! `/mnt/minecraft/servers/...` paths or nothing at all. Existing old
//! archives keep that shape; anything rcmd writes restores with a plain
//! `tar -xzf backup.tar.gz -C <server_dir>`.

use std::fs;
use std::path::Path;

use flate2::write::GzEncoder;
use flate2::Compression;

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("nothing to back up: no configured directory exists")]
    NothingToBackUp,
    #[error("io error during backup: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BackupReport {
    /// Where the archive ended up. A path when it stayed local, an s3://
    /// url when it was uploaded.
    pub archive: String,
    pub bytes: u64,
    pub entries: u64,
    /// Configured directories that did not exist on disk; reported rather
    /// than silently skipped, because a typo in directories_to_backup
    /// otherwise looks identical to success.
    pub missing: Vec<String>,
    /// True once the archive has reached the bucket.
    pub uploaded: bool,
}

/// Writes `<destination_dir>/<server>.tar.gz` from the whitelist.
///
/// The archive is built to a temp name and renamed into place, so a crash
/// mid-backup never leaves a truncated file under the name the previous good
/// backup had.
pub fn backup_server(
    server: &str,
    server_dir: &Path,
    directories_to_backup: &[String],
    destination_dir: &Path,
) -> Result<BackupReport, BackupError> {
    fs::create_dir_all(destination_dir)?;
    let final_path = destination_dir.join(format!("{server}.tar.gz"));
    let temp_path = destination_dir.join(format!(".{server}.tar.gz.partial"));

    let mut missing = Vec::new();
    let mut entries = 0u64;
    {
        let file = fs::File::create(&temp_path)?;
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = tar::Builder::new(encoder);
        builder.follow_symlinks(false);

        let mut any = false;
        for dir in directories_to_backup {
            let source = server_dir.join(dir);
            if !source.exists() {
                missing.push(dir.clone());
                continue;
            }
            any = true;
            // Relative name inside the archive == the configured path.
            builder.append_dir_all(dir, &source)?;
            entries += 1;
        }
        if !any {
            drop(builder);
            let _ = fs::remove_file(&temp_path);
            return Err(BackupError::NothingToBackUp);
        }
        builder.into_inner()?.finish()?;
    }

    fs::rename(&temp_path, &final_path)?;
    let bytes = fs::metadata(&final_path)?.len();
    Ok(BackupReport {
        archive: final_path.display().to_string(),
        bytes,
        entries,
        missing,
        uploaded: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archives_are_relative_and_restorable() {
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("hub");
        fs::create_dir_all(server.join("world/region")).unwrap();
        fs::write(server.join("world/level.dat"), b"data").unwrap();
        fs::create_dir_all(server.join("logs")).unwrap();
        fs::write(server.join("logs/latest.log"), b"log").unwrap();

        let dest = dir.path().join("backups");
        let report = backup_server(
            "hub",
            &server,
            &["world".into(), "does_not_exist".into()],
            &dest,
        )
        .unwrap();
        assert_eq!(report.missing, vec!["does_not_exist".to_string()]);

        // Restore into a fresh dir with plain tar semantics.
        let restore = dir.path().join("restore");
        fs::create_dir_all(&restore).unwrap();
        let file = fs::File::open(&report.archive).unwrap();
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
        archive.unpack(&restore).unwrap();
        assert_eq!(fs::read(restore.join("world/level.dat")).unwrap(), b"data");
        // The non-whitelisted logs dir was not archived.
        assert!(!restore.join("logs").exists());
    }

    #[test]
    fn all_missing_is_an_error_not_an_empty_archive() {
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("s");
        fs::create_dir_all(&server).unwrap();
        let err = backup_server("s", &server, &["nope".into()], &dir.path().join("b")).unwrap_err();
        assert!(matches!(err, BackupError::NothingToBackUp));
    }
}
