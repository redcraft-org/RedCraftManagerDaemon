//! Putting secrets into a built template.
//!
//! Config files in the repository carry a placeholder where a password
//! goes, and the value only exists in the environment of the machine doing
//! the build. That is what makes the config repository safe to make public.
//!
//! Substitution happens on the built output, never on the repository, and
//! only in file types that are actually config. Rewriting a jar or a world
//! file because it happened to contain the bytes of a placeholder would be
//! a memorable afternoon.

use std::collections::BTreeMap;
use std::path::Path;

/// File extensions worth substituting in.
const CONFIG_EXTENSIONS: [&str; 9] = [
    "yml",
    "yaml",
    "conf",
    "toml",
    "json",
    "properties",
    "ini",
    "txt",
    "secret",
];

/// Replaces every placeholder with its value from the environment.
///
/// Returns the placeholders that had no value, so the build can report
/// them. A missing secret substitutes to nothing, which is what the
/// previous tooling did silently; here the caller is told.
pub fn substitute(
    root: &Path,
    secrets: &BTreeMap<String, String>,
    environment: &(dyn Fn(&str) -> Option<String> + Send + Sync),
) -> std::io::Result<Vec<String>> {
    if secrets.is_empty() {
        return Ok(vec![]);
    }

    let mut values = BTreeMap::new();
    let mut missing = Vec::new();
    for (placeholder, variable) in secrets {
        match environment(variable) {
            // Trailing whitespace in a secret is invisible and breaks
            // things in ways that take an afternoon to find, so it goes.
            Some(value) => {
                values.insert(placeholder.clone(), value.trim().to_string());
            }
            None => missing.push(placeholder.clone()),
        }
    }

    walk(root, &values)?;
    Ok(missing)
}

fn walk(directory: &Path, values: &BTreeMap<String, String>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(directory)?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, values)?;
            continue;
        }
        let is_config = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| CONFIG_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
            .unwrap_or(false);
        if !is_config {
            continue;
        }
        // Anything that is not valid UTF-8 is not a config file we wrote,
        // whatever its extension claims.
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut replaced = text.clone();
        for (placeholder, value) in values {
            if replaced.contains(placeholder.as_str()) {
                replaced = replaced.replace(placeholder.as_str(), value);
            }
        }
        if replaced != text {
            std::fs::write(&path, replaced)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secrets() -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        map.insert("MYSQL_PASSWORD".to_string(), "DB_PASSWORD".to_string());
        map
    }

    #[test]
    fn placeholders_are_replaced_in_config_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.yml"),
            "password: 'MYSQL_PASSWORD'\n",
        )
        .unwrap();
        let missing = substitute(dir.path(), &secrets(), &|name| {
            (name == "DB_PASSWORD").then(|| "hunter2".to_string())
        })
        .unwrap();
        assert!(missing.is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config.yml")).unwrap(),
            "password: 'hunter2'\n"
        );
    }

    #[test]
    fn whitespace_around_a_secret_is_trimmed() {
        // A value pasted into a CI secret box with a trailing newline is
        // the single most common way this breaks.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("c.yml"), "token: MYSQL_PASSWORD\n").unwrap();
        substitute(dir.path(), &secrets(), &|_| {
            Some("  spaced-token \n".to_string())
        })
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("c.yml")).unwrap(),
            "token: spaced-token\n"
        );
    }

    #[test]
    fn a_missing_secret_is_reported_rather_than_silently_blank() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("c.yml"), "password: MYSQL_PASSWORD\n").unwrap();
        let missing = substitute(dir.path(), &secrets(), &|_| None).unwrap();
        assert_eq!(missing, vec!["MYSQL_PASSWORD".to_string()]);
        // The placeholder is left alone, so the failure is visible on the
        // server rather than looking like an empty password.
        assert!(std::fs::read_to_string(dir.path().join("c.yml"))
            .unwrap()
            .contains("MYSQL_PASSWORD"));
    }

    #[test]
    fn binaries_are_left_alone_whatever_their_extension() {
        let dir = tempfile::tempdir().unwrap();
        let jar = dir.path().join("plugins/Thing.jar");
        std::fs::create_dir_all(jar.parent().unwrap()).unwrap();
        let bytes = b"PK\x03\x04MYSQL_PASSWORD\xff\xfe".to_vec();
        std::fs::write(&jar, &bytes).unwrap();
        // And a .json that is not valid UTF-8.
        let odd = dir.path().join("odd.json");
        std::fs::write(&odd, b"\xff\xfeMYSQL_PASSWORD").unwrap();

        substitute(dir.path(), &secrets(), &|_| Some("secret".into())).unwrap();
        assert_eq!(std::fs::read(&jar).unwrap(), bytes);
        assert_eq!(std::fs::read(&odd).unwrap(), b"\xff\xfeMYSQL_PASSWORD");
    }

    #[test]
    fn nested_directories_are_covered() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("plugins/CMI/config");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("db.yml"), "pass: MYSQL_PASSWORD").unwrap();
        substitute(dir.path(), &secrets(), &|_| Some("v".into())).unwrap();
        assert_eq!(
            std::fs::read_to_string(deep.join("db.yml")).unwrap(),
            "pass: v"
        );
    }
}
