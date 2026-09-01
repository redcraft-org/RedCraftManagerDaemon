//! Template tar application, ported from rcsm/provisioning.go.
//!
//! The wipe contract is deliberate and must not soften: for every entry in
//! the tar, whatever lives at that path is removed wholesale and replaced.
//! Because template tars carry directory entries (`tar.add(dir, arcname='.')`
//! on the build side), `./plugins` is removed and rebuilt on every apply,
//! which is exactly how a template pull heals a mangled plugins directory.
//! Anything NOT named in the tar survives: worlds, logs, caches,
//! banned-players.json.
//!
//! Two guards, one old and one new:
//! - The server root itself is never removed (rcsm learned this the hard way,
//!   commit 9407014, after a `./` entry deleted a whole server).
//! - Entries may not escape the server directory (new): rcsm joined
//!   `header.Name` unchecked and then called RemoveAll on it, so a tar with a
//!   `../../` entry could delete outside the server as the minecraft user.
//!   The bucket was trusted; the code should not have to trust it.

use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("could not open template tar: {0}")]
    Open(std::io::Error),
    #[error("tar entry {0:?} escapes the server directory")]
    Traversal(String),
    #[error("io error applying {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
}

/// What one apply did, for the deployment record and the logs.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ApplyReport {
    pub files_written: u64,
    pub dirs_replaced: u64,
    pub bytes_written: u64,
}

/// Normalizes a tar entry name into a relative path that stays inside the
/// root. `./`-prefixed entries (what `arcname='.'` produces) are accepted;
/// absolute paths and any `..` component are refused.
fn safe_relative(name: &str) -> Result<PathBuf, ApplyError> {
    let path = Path::new(name);
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => clean.push(part),
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => {
                return Err(ApplyError::Traversal(name.to_string()));
            }
        }
    }
    Ok(clean)
}

/// Applies a template tar onto a server directory.
pub fn apply_tar(tar_path: &Path, server_dir: &Path) -> Result<ApplyReport, ApplyError> {
    let file = fs::File::open(tar_path).map_err(ApplyError::Open)?;
    apply_tar_reader(file, server_dir)
}

pub fn apply_tar_reader<R: Read>(reader: R, server_dir: &Path) -> Result<ApplyReport, ApplyError> {
    let mut archive = tar::Archive::new(reader);
    let mut report = ApplyReport::default();

    let entries = archive.entries().map_err(ApplyError::Open)?;
    for entry in entries {
        let mut entry = entry.map_err(ApplyError::Open)?;
        let name = entry.path_bytes();
        let name = String::from_utf8_lossy(&name).into_owned();

        let relative = safe_relative(&name)?;
        // The `./` root entry itself: replacing it would delete the server.
        if relative.as_os_str().is_empty() {
            continue;
        }
        let destination = server_dir.join(&relative);

        // The wipe-then-write contract: whatever is at this path goes away.
        if destination.exists() || destination.is_symlink() {
            let removed = if destination.is_dir() && !destination.is_symlink() {
                report.dirs_replaced += 1;
                fs::remove_dir_all(&destination)
            } else {
                fs::remove_file(&destination)
            };
            removed.map_err(|source| ApplyError::Io {
                path: destination.display().to_string(),
                source,
            })?;
        }

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|source| ApplyError::Io {
                path: parent.display().to_string(),
                source,
            })?;
        }

        match entry.header().entry_type() {
            tar::EntryType::Directory => {
                fs::create_dir_all(&destination).map_err(|source| ApplyError::Io {
                    path: destination.display().to_string(),
                    source,
                })?;
            }
            tar::EntryType::Regular => {
                let mut out = fs::File::create(&destination).map_err(|source| ApplyError::Io {
                    path: destination.display().to_string(),
                    source,
                })?;
                let written =
                    std::io::copy(&mut entry, &mut out).map_err(|source| ApplyError::Io {
                        path: destination.display().to_string(),
                        source,
                    })?;
                report.files_written += 1;
                report.bytes_written += written;
                // Templates ship executables (server.jar wrappers, scripts);
                // preserve the mode the build recorded.
                #[cfg(unix)]
                if let Ok(mode) = entry.header().mode() {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(&destination, fs::Permissions::from_mode(mode));
                }
            }
            // Symlinks, fifos and friends are not something a template build
            // produces; skipping beats writing attacker-shaped surprises.
            other => {
                tracing::warn!("skipping tar entry {name:?} of unsupported type {other:?}");
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an in-memory tar the way the template builder does:
    /// `./`-prefixed names, directory entries included.
    fn build_tar(entries: &[(&str, Option<&str>)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, contents) in entries {
            match contents {
                Some(text) => {
                    let mut header = tar::Header::new_gnu();
                    header.set_size(text.len() as u64);
                    header.set_mode(0o644);
                    header.set_cksum();
                    builder
                        .append_data(&mut header, name, text.as_bytes())
                        .unwrap();
                }
                None => {
                    let mut header = tar::Header::new_gnu();
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_size(0);
                    header.set_mode(0o755);
                    header.set_cksum();
                    builder
                        .append_data(&mut header, name, std::io::empty())
                        .unwrap();
                }
            }
        }
        builder.into_inner().unwrap()
    }

    #[test]
    fn entries_in_tar_replace_and_others_survive() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Live state: a world (not in tar), a plugins dir with a data file
        // (dir IS in tar), and a stale jar.
        std::fs::create_dir_all(root.join("world")).unwrap();
        std::fs::write(root.join("world/level.dat"), b"precious").unwrap();
        std::fs::create_dir_all(root.join("plugins/SomePlugin")).unwrap();
        std::fs::write(root.join("plugins/SomePlugin/data.db"), b"runtime data").unwrap();
        std::fs::write(root.join("plugins/Old.jar"), b"old").unwrap();

        let tar = build_tar(&[
            ("./plugins", None),
            ("./plugins/New.jar", Some("new jar")),
            ("./server.properties", Some("motd=hi")),
        ]);
        let report = apply_tar_reader(&tar[..], root).unwrap();

        // The world survived untouched.
        assert_eq!(
            std::fs::read(root.join("world/level.dat")).unwrap(),
            b"precious"
        );
        // plugins/ was wiped wholesale: the data dir and the old jar are gone.
        assert!(!root.join("plugins/SomePlugin").exists());
        assert!(!root.join("plugins/Old.jar").exists());
        assert_eq!(
            std::fs::read(root.join("plugins/New.jar")).unwrap(),
            b"new jar"
        );
        assert_eq!(
            std::fs::read(root.join("server.properties")).unwrap(),
            b"motd=hi"
        );
        assert_eq!(report.files_written, 2);
    }

    #[test]
    fn the_root_entry_does_not_delete_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("keep.txt"), b"keep").unwrap();

        // `arcname='.'` produces a literal "./" root entry first.
        let tar = build_tar(&[("./", None), ("./new.txt", Some("x"))]);
        apply_tar_reader(&tar[..], root).unwrap();

        assert!(root.join("keep.txt").exists());
        assert!(root.join("new.txt").exists());
    }

    /// Forges a tar with a raw entry name the crate's own builder would
    /// refuse (it rejects `..`), so the guard is exercised against a hostile
    /// archive rather than one we could not have built.
    fn forge_named_entry(name: &str, contents: &[u8]) -> Vec<u8> {
        let mut header = tar::Header::new_ustar();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        // Write the name straight into the 100-byte name field, bypassing
        // set_path's validation, then fix the checksum.
        let bytes = header.as_mut_bytes();
        let raw = name.as_bytes();
        bytes[..raw.len()].copy_from_slice(raw);
        header.set_cksum();

        let mut out = Vec::new();
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(contents);
        // Pad the data to a 512-byte boundary, then two zero blocks to end.
        let pad = (512 - contents.len() % 512) % 512;
        out.extend(std::iter::repeat_n(0, pad));
        out.extend(std::iter::repeat_n(0, 1024));
        out
    }

    #[test]
    fn parent_traversal_is_refused_before_anything_is_touched() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim.txt");
        std::fs::write(&victim, b"outside").unwrap();
        let server = dir.path().join("server");
        std::fs::create_dir(&server).unwrap();

        let tar = forge_named_entry("../victim.txt", b"pwned");
        let err = apply_tar_reader(&tar[..], &server).unwrap_err();
        assert!(matches!(err, ApplyError::Traversal(_)));
        assert_eq!(std::fs::read(&victim).unwrap(), b"outside");
    }

    #[test]
    fn absolute_paths_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(4);
        header.set_mode(0o644);
        header.set_cksum();
        // append_data normalizes; write the raw name through a path override.
        builder
            .append_data(&mut header, "tmp/abs.txt", &b"boom"[..])
            .unwrap();
        let tar = builder.into_inner().unwrap();
        // Sanity: relative form applies fine...
        apply_tar_reader(&tar[..], dir.path()).unwrap();
        // ...and the guard itself rejects an absolute name.
        assert!(matches!(
            safe_relative("/etc/passwd"),
            Err(ApplyError::Traversal(_))
        ));
        assert!(matches!(
            safe_relative("./a/../../b"),
            Err(ApplyError::Traversal(_))
        ));
    }

    #[test]
    fn dot_prefixed_names_normalize_like_rcst_produces_them() {
        assert_eq!(
            safe_relative("./plugins/X.jar").unwrap(),
            PathBuf::from("plugins/X.jar")
        );
        assert_eq!(safe_relative("./").unwrap(), PathBuf::new());
        assert_eq!(safe_relative(".").unwrap(), PathBuf::new());
    }
}
