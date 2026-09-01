//! Reading a server's logs.
//!
//! The console is a tmux pane, which holds a few hundred lines and forgets
//! everything before that. Most of the time the thing you want to know
//! happened an hour ago, so rcmd reads the log file when there is one and
//! only falls back to the pane when there is not.
//!
//! Which file is configuration, because "logs/latest.log" is a Minecraft
//! convention and rcmd supervises things that are not Minecraft. The
//! patterns use the same rule as every other pattern in rcmd: `*` is the
//! only wildcard and everything else is literal.

use std::path::{Path, PathBuf};

/// Where the lines came from, so a surprising answer can be explained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogOrigin {
    /// A file on disk, which keeps real history.
    File,
    /// The tmux pane, which is all there is when nothing matched.
    Console,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct LogRead {
    pub origin: LogOrigin,
    /// The file it came from, when it came from one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub content: String,
}

/// Finds the best log file for a server.
///
/// Patterns are tried in order and the first one that matches anything
/// wins, so you can put the specific file first and a catch all after it.
/// When one pattern matches several files the most recently written one is
/// taken, which is what you want when a server rotates its logs.
pub fn find_log(server_dir: &Path, patterns: &[String]) -> Option<PathBuf> {
    for pattern in patterns {
        let mut matches: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
        collect(server_dir, server_dir, pattern, &mut matches, 0);
        if matches.is_empty() {
            continue;
        }
        matches.sort_by_key(|(modified, _)| *modified);
        return matches.pop().map(|(_, path)| path);
    }
    None
}

/// Walks the server directory looking for files whose relative path matches.
///
/// Bounded depth, because a server directory contains worlds, and walking a
/// world to find a log file would be a memorable way to make the API time
/// out.
fn collect(
    root: &Path,
    directory: &Path,
    pattern: &str,
    out: &mut Vec<(std::time::SystemTime, PathBuf)>,
    depth: usize,
) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            collect(root, &path, pattern, out, depth + 1);
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if crate::catalog::pin_matches(pattern, &relative) {
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            out.push((modified, path));
        }
    }
}

/// Reads the last `lines` lines of a file without loading all of it.
///
/// A `latest.log` on a busy server is tens of megabytes by the evening, and
/// reading it into memory to throw away all but the last hundred lines is
/// the kind of thing that works fine until the day it does not.
pub fn tail(path: &Path, lines: usize, contains: &str) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();

    // Read backwards in chunks until there are enough newlines. Filtering
    // needs more raw lines than it returns, so when a filter is set we take
    // a much bigger bite rather than looping forever.
    let wanted = if contains.is_empty() {
        lines
    } else {
        lines * 200
    };
    let mut buffer: Vec<u8> = Vec::new();
    let mut position = size;
    let chunk = 64 * 1024;

    while position > 0 {
        let step = chunk.min(position);
        position -= step;
        file.seek(SeekFrom::Start(position))?;
        let mut piece = vec![0u8; step as usize];
        file.read_exact(&mut piece)?;
        piece.extend_from_slice(&buffer);
        buffer = piece;
        if buffer.iter().filter(|b| **b == b'\n').count() > wanted {
            break;
        }
        // Ten megabytes of scrollback is already more than anybody reads.
        if buffer.len() > 10 * 1024 * 1024 {
            break;
        }
    }

    let text = String::from_utf8_lossy(&buffer);
    let mut kept: Vec<&str> = text
        .lines()
        // A literal substring, not a pattern: somebody searching a log for
        // "[1.21.4]" should find that line, not a regex error.
        .filter(|line| contains.is_empty() || line.contains(contains))
        .collect();
    if kept.len() > lines {
        kept = kept.split_off(kept.len() - lines);
    }
    Ok(kept.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn server(dir: &Path) {
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        std::fs::write(dir.join("logs/latest.log"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(dir.join("logs/2026-08-31-1.log.gz"), "old").unwrap();
        std::fs::write(dir.join("server.properties"), "x=1").unwrap();
    }

    #[test]
    fn the_first_pattern_that_matches_wins() {
        let dir = tempfile::tempdir().unwrap();
        server(dir.path());
        let found = find_log(dir.path(), &["logs/latest.log".into(), "logs/*.log".into()]).unwrap();
        assert!(found.ends_with("logs/latest.log"));
    }

    #[test]
    fn a_pattern_matching_nothing_falls_through_to_the_next() {
        let dir = tempfile::tempdir().unwrap();
        server(dir.path());
        let found = find_log(
            dir.path(),
            &["logs/bluemap.log".into(), "logs/latest.log".into()],
        )
        .unwrap();
        assert!(found.ends_with("logs/latest.log"));
        // And nothing at all means nothing, so the caller can use tmux.
        assert!(find_log(dir.path(), &["nope/*.log".into()]).is_none());
    }

    #[test]
    fn several_matches_take_the_most_recent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        std::fs::write(dir.path().join("logs/a.log"), "older").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.path().join("logs/b.log"), "newer").unwrap();
        let found = find_log(dir.path(), &["logs/*.log".into()]).unwrap();
        assert!(found.ends_with("logs/b.log"), "got {found:?}");
    }

    #[test]
    fn a_world_directory_does_not_get_walked_forever() {
        // Bounded depth. A server directory has a world in it, and walking
        // a world to find a log file is how you make the API time out.
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("world/region/a/b/c/d/e/f");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("buried.log"), "x").unwrap();
        assert!(find_log(dir.path(), &["*.log".into()]).is_none());
    }

    #[test]
    fn the_tail_is_the_end_of_the_file_and_only_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.log");
        let mut file = std::fs::File::create(&path).unwrap();
        for i in 0..5000 {
            writeln!(file, "line {i}").unwrap();
        }
        drop(file);

        let out = tail(&path, 3, "").unwrap();
        assert_eq!(out, "line 4997\nline 4998\nline 4999");
    }

    #[test]
    fn the_filter_is_a_substring_not_a_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.log");
        std::fs::write(&path, "hello\n[1.21.4] version\nworld\n").unwrap();
        assert_eq!(tail(&path, 10, "[1.21.4]").unwrap(), "[1.21.4] version");
        // A regex metacharacter is just a character.
        assert_eq!(tail(&path, 10, "h.llo").unwrap(), "");
    }

    #[test]
    fn a_file_shorter_than_the_ask_comes_back_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.log");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        assert_eq!(tail(&path, 100, "").unwrap(), "one\ntwo");
    }
}
