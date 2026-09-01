//! The catalog: what artifacts exist and where they come from.
//!
//! This is configuration, so it lives in YAML in a git repository rather
//! than in the database. Somebody can read it, diff it and open a pull
//! request against it without touching the running service.
//!
//! One field is new compared to the JSON catalog this replaces: `kind`.
//! Nothing in the old format said whether a row was a server engine, a
//! plugin or a Fabric mod; the distinction only existed in whichever
//! template happened to reference it. A template that wants to put mods in
//! `mods/` and plugins in `plugins/` has to know, so the catalog says.

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("could not read {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("could not parse {path}: {source}")]
    Parse {
        path: String,
        source: serde_yaml::Error,
    },
    #[error("{0}")]
    Invalid(String),
}

/// What an artifact is for. The template decides where a jar lands from
/// this, so it is required rather than guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// The server jar itself: Purpur, Velocity, Fabric.
    Engine,
    /// A Bukkit, Paper, Velocity or BungeeCord plugin.
    Plugin,
    /// A Fabric or Quilt mod.
    Mod,
}

impl Kind {
    /// The directory a template puts this kind in.
    pub fn directory(self) -> &'static str {
        match self {
            Kind::Engine => "",
            Kind::Plugin => "plugins",
            Kind::Mod => "mods",
        }
    }
}

/// Where to look for new versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// GitHub releases, matching an asset name.
    Github,
    /// A Modrinth project.
    Modrinth,
    /// A Jenkins job's last successful build.
    Jenkins,
    /// The PaperMC fill API.
    Papermc,
    /// A URL that always serves the latest build.
    Direct,
    /// The Spiget API, which is SpigotMC without the browser challenge.
    Spiget,
    /// The Fabric meta API, which needs a loader version and an installer
    /// version put together before there is a URL to fetch.
    Fabricmc,
    /// Hangar, PaperMC's plugin site.
    Hangar,
    /// Nothing to fetch: somebody uploads this one.
    ///
    /// A handful of plugins sit behind a login and a bot check. Pretending
    /// rcmd can download them means a failed job every single night, which
    /// is how a report full of red stops being read. Saying so instead
    /// leaves the run clean and the jar arrives through the upload
    /// endpoint.
    Manual,
}

/// What to do with the file once it is downloaded. The important job is
/// working out the version, which almost always means reading it out of
/// the jar rather than trusting a filename.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostProcessor {
    /// Reads `plugin.yml`, `bungee.yml`, `paper-plugin.yml` or
    /// `velocity-plugin.json` out of the jar.
    Plugin,
    /// Reads `fabric.mod.json`.
    FabricMod,
    /// Reads `version.json`, which is how Purpur reports its Minecraft
    /// version.
    VersionJson,
    /// Reads `install.properties`, which is how the Fabric installer does.
    FabricInstaller,
    /// Pulls a single file out of a zip before the rest of the chain runs.
    Unzip,
    /// Takes the version from whatever the source called the release.
    ///
    /// For the handful of things that ship no version anywhere inside the
    /// file. A GitHub tag or a PaperMC build is not as good as reading it
    /// out of the jar, but it beats parsing a file name, which is how the
    /// old tooling decided a plugin called Multiverse-Core was at version
    /// "Core" for several months.
    FromSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub name: String,
    pub kind: Kind,
    pub source: SourceKind,
    pub url: String,

    /// Which file to take when the source offers several. `*` is the only
    /// wildcard, everything else is literal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_filter: Option<String>,

    /// Which file to take out of the archive, for `unzip`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_filter: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub post_processors: Vec<PostProcessor>,

    /// Set to false to keep the entry around without checking it. Handy for
    /// a source that broke and that you don't want failing every run.
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
}

impl Catalog {
    pub fn load(path: &Path) -> Result<Self, CatalogError> {
        let text = std::fs::read_to_string(path).map_err(|source| CatalogError::Read {
            path: path.display().to_string(),
            source,
        })?;
        let catalog: Catalog =
            serde_yaml::from_str(&text).map_err(|source| CatalogError::Parse {
                path: path.display().to_string(),
                source,
            })?;
        catalog.validate()?;
        Ok(catalog)
    }

    /// Catches the mistakes a pull request is most likely to contain,
    /// before anything downloads a gigabyte to find out.
    pub fn validate(&self) -> Result<(), CatalogError> {
        let mut seen = BTreeSet::new();
        for artifact in &self.artifacts {
            if artifact.name.trim().is_empty() {
                return Err(CatalogError::Invalid(
                    "an artifact has an empty name".into(),
                ));
            }
            // The name becomes a filename in the template, so it may not
            // contain anything that would climb out of a directory.
            if artifact.name.contains('/') || artifact.name.contains("..") {
                return Err(CatalogError::Invalid(format!(
                    "artifact name {:?} may not contain a path",
                    artifact.name
                )));
            }
            if !seen.insert(&artifact.name) {
                return Err(CatalogError::Invalid(format!(
                    "artifact {:?} is listed twice",
                    artifact.name
                )));
            }
            if artifact.post_processors.contains(&PostProcessor::Unzip)
                && artifact.archive_filter.is_none()
            {
                return Err(CatalogError::Invalid(format!(
                    "artifact {:?} unzips but has no archive_filter",
                    artifact.name
                )));
            }
        }
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&Artifact> {
        self.artifacts.iter().find(|a| a.name == name)
    }
}

/// Matches a version against a pin like `5.5.*`.
///
/// Only `*` is a wildcard and it matches at least one character;
/// everything else is literal, so a dot is a dot. This is the same rule
/// the previous template builder used, kept identical because every pin in
/// the existing templates was written against it.
pub fn pin_matches(pin: &str, version: &str) -> bool {
    if pin == "*" {
        return true;
    }
    let mut pattern = String::from("^");
    for part in pin.split('*').map(regex::escape) {
        if pattern.len() > 1 {
            pattern.push_str(".+");
        }
        pattern.push_str(&part);
    }
    pattern.push('$');
    regex::Regex::new(&pattern)
        .map(|re| re.is_match(version))
        .unwrap_or(false)
}

/// Widens a pin so it keeps its wildcards but points at a new version.
///
/// `5.5.*` plus `5.6.1` gives `5.6.*`. If the new version has fewer parts
/// than the pin, the pin loses a level rather than inventing one.
pub fn widen_pin(pin: &str, new_version: &str) -> String {
    if pin == "*" {
        return pin.to_string();
    }
    let pin_parts: Vec<&str> = pin.split('.').collect();
    let new_parts: Vec<&str> = new_version.split('.').collect();

    let Some(star) = pin_parts.iter().position(|part| *part == "*") else {
        // A pin with no wildcard is an exact pin, so it just moves.
        return new_version.to_string();
    };

    // The star has to land on a component the new version actually has.
    // Widening `1.21.*` with `26.2` cannot give `26.2.*`, because that pin
    // matches `26.2.1` and not `26.2` itself, which is the very version it
    // was widened for.
    let keep = star.min(new_parts.len().saturating_sub(1));
    let mut out: Vec<String> = new_parts[..keep].iter().map(|p| (*p).to_string()).collect();
    out.push("*".to_string());
    out.join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_treat_only_star_as_special() {
        assert!(pin_matches("5.5.*", "5.5.44"));
        assert!(pin_matches("5.5.*", "5.5.1-SNAPSHOT"));
        assert!(!pin_matches("5.5.*", "5.6.0"));
        // A dot is a literal dot, not "any character".
        assert!(!pin_matches("1.2.3", "1x2x3"));
        assert!(pin_matches("*", "anything at all"));
    }

    #[test]
    fn a_star_needs_something_to_match() {
        // The old builder used `.+`, so a bare prefix does not match the pin
        // with nothing after it. Kept identical so existing pins behave.
        assert!(!pin_matches("5.5.*", "5.5."));
        assert!(pin_matches("5.5.*", "5.5.0"));
    }

    #[test]
    fn widening_keeps_the_wildcards_where_they_were() {
        assert_eq!(widen_pin("5.5.*", "5.6.1"), "5.6.*");
        assert_eq!(widen_pin("2.*", "3.4.5"), "3.*");
        assert_eq!(widen_pin("1.2.3", "1.2.4"), "1.2.4");
    }

    #[test]
    fn widening_backs_off_when_the_new_version_is_shorter() {
        // 26.2 arriving against a 1.21.4 style pin must not become 26.2.*
        // with a part that does not exist.
        assert_eq!(widen_pin("1.21.*", "26.2"), "26.*");
        assert_eq!(widen_pin("*", "26.2"), "*");
    }

    #[test]
    fn duplicate_names_are_refused() {
        let catalog = Catalog {
            artifacts: vec![
                Artifact {
                    name: "CMI".into(),
                    kind: Kind::Plugin,
                    source: SourceKind::Direct,
                    url: "https://example.invalid/cmi.jar".into(),
                    file_filter: None,
                    archive_filter: None,
                    post_processors: vec![PostProcessor::Plugin],
                    enabled: true,
                },
                Artifact {
                    name: "CMI".into(),
                    kind: Kind::Plugin,
                    source: SourceKind::Direct,
                    url: "https://example.invalid/other.jar".into(),
                    file_filter: None,
                    archive_filter: None,
                    post_processors: vec![],
                    enabled: true,
                },
            ],
        };
        assert!(catalog.validate().is_err());
    }

    #[test]
    fn a_name_may_not_be_a_path() {
        let catalog = Catalog {
            artifacts: vec![Artifact {
                name: "../../etc/cron.d/oops".into(),
                kind: Kind::Plugin,
                source: SourceKind::Direct,
                url: "https://example.invalid/x.jar".into(),
                file_filter: None,
                archive_filter: None,
                post_processors: vec![],
                enabled: true,
            }],
        };
        assert!(catalog.validate().is_err());
    }

    #[test]
    fn kinds_know_where_they_belong() {
        assert_eq!(Kind::Plugin.directory(), "plugins");
        assert_eq!(Kind::Mod.directory(), "mods");
        assert_eq!(Kind::Engine.directory(), "");
    }
}
