//! Working out an artifact's version by reading the jar.
//!
//! The version has to come from inside the file. A release asset can be
//! called anything, and the previous tooling learned this the hard way: it
//! parsed `Name-Version.jar` by splitting on the first dash, so
//! `Multiverse-Core-5.8.1.jar` became the artifact `Multiverse` at version
//! `Core-5.8.1`, and every template that wanted it had to pin
//! `Core-5.8.*`. Reading the metadata makes that whole class of problem go
//! away.

use std::io::{Cursor, Read};

use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum MetaError {
    #[error("not a readable jar or zip: {0}")]
    NotAnArchive(String),
    #[error("no version found: none of {0} were present or parseable")]
    NoVersion(String),
}

/// What the descriptors we understand have in common.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JarMeta {
    pub name: Option<String>,
    pub version: String,
    /// Which file the version came from, so a surprising answer can be
    /// traced without opening the jar by hand.
    pub source_file: &'static str,
}

#[derive(Deserialize)]
struct YamlDescriptor {
    name: Option<String>,
    version: Option<serde_yaml::Value>,
}

#[derive(Deserialize)]
struct VelocityDescriptor {
    id: Option<String>,
    version: Option<String>,
}

#[derive(Deserialize)]
struct FabricDescriptor {
    id: Option<String>,
    version: Option<String>,
}

#[derive(Deserialize)]
struct VersionJson {
    id: Option<String>,
    release_target: Option<String>,
}

/// A version in YAML may be unquoted, in which case `1.2` parses as a
/// float and `26` as an integer. Rendering it back rather than demanding a
/// string is what keeps a perfectly ordinary `plugin.yml` from failing.
fn yaml_version_to_string(value: &serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Reads the plugin descriptors, in the order the platforms are most
/// likely to be authoritative.
pub fn read_plugin_meta(bytes: &[u8]) -> Result<JarMeta, MetaError> {
    const CANDIDATES: [&str; 4] = [
        "bungee.yml",
        "plugin.yml",
        "paper-plugin.yml",
        "velocity-plugin.json",
    ];

    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| MetaError::NotAnArchive(e.to_string()))?;

    for candidate in CANDIDATES {
        let Ok(mut file) = archive.by_name(candidate) else {
            continue;
        };
        let mut text = String::new();
        if file.read_to_string(&mut text).is_err() {
            continue;
        }

        if candidate == "velocity-plugin.json" {
            if let Ok(descriptor) = serde_json::from_str::<VelocityDescriptor>(&text) {
                if let Some(version) = descriptor.version {
                    return Ok(JarMeta {
                        name: descriptor.id,
                        version,
                        source_file: "velocity-plugin.json",
                    });
                }
            }
            continue;
        }

        if let Ok(descriptor) = serde_yaml::from_str::<YamlDescriptor>(&text) {
            if let Some(version) = descriptor.version.as_ref().and_then(yaml_version_to_string) {
                return Ok(JarMeta {
                    name: descriptor.name,
                    version,
                    source_file: match candidate {
                        "bungee.yml" => "bungee.yml",
                        "plugin.yml" => "plugin.yml",
                        _ => "paper-plugin.yml",
                    },
                });
            }
        }
    }

    Err(MetaError::NoVersion(CANDIDATES.join(", ")))
}

pub fn read_fabric_meta(bytes: &[u8]) -> Result<JarMeta, MetaError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| MetaError::NotAnArchive(e.to_string()))?;
    let mut text = String::new();
    archive
        .by_name("fabric.mod.json")
        .map_err(|_| MetaError::NoVersion("fabric.mod.json".into()))?
        .read_to_string(&mut text)
        .map_err(|_| MetaError::NoVersion("fabric.mod.json".into()))?;

    // Fabric's descriptor allows comments in practice, which strict JSON
    // does not, so a parse failure here is not necessarily a broken mod.
    let descriptor: FabricDescriptor =
        serde_json::from_str(&text).map_err(|_| MetaError::NoVersion("fabric.mod.json".into()))?;
    let version = descriptor
        .version
        .ok_or_else(|| MetaError::NoVersion("fabric.mod.json".into()))?;
    Ok(JarMeta {
        name: descriptor.id,
        version,
        source_file: "fabric.mod.json",
    })
}

/// How a server engine reports which Minecraft version it is. Purpur and
/// Paper both ship this.
pub fn read_version_json(bytes: &[u8]) -> Result<JarMeta, MetaError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| MetaError::NotAnArchive(e.to_string()))?;
    let mut text = String::new();
    archive
        .by_name("version.json")
        .map_err(|_| MetaError::NoVersion("version.json".into()))?
        .read_to_string(&mut text)
        .map_err(|_| MetaError::NoVersion("version.json".into()))?;

    let descriptor: VersionJson =
        serde_json::from_str(&text).map_err(|_| MetaError::NoVersion("version.json".into()))?;
    // release_target is the Minecraft version a snapshot targets; id is the
    // build's own name. Templates pin the Minecraft version, so prefer it.
    let version = descriptor
        .release_target
        .or(descriptor.id)
        .ok_or_else(|| MetaError::NoVersion("version.json".into()))?;
    Ok(JarMeta {
        name: None,
        version,
        source_file: "version.json",
    })
}

/// The Fabric installer reports the game version in a properties file.
pub fn read_installer_properties(bytes: &[u8]) -> Result<JarMeta, MetaError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| MetaError::NotAnArchive(e.to_string()))?;
    let mut text = String::new();
    archive
        .by_name("install.properties")
        .map_err(|_| MetaError::NoVersion("install.properties".into()))?
        .read_to_string(&mut text)
        .map_err(|_| MetaError::NoVersion("install.properties".into()))?;

    for line in text.lines() {
        if let Some(value) = line.trim().strip_prefix("game-version=") {
            return Ok(JarMeta {
                name: None,
                version: value.trim().to_string(),
                source_file: "install.properties",
            });
        }
    }
    Err(MetaError::NoVersion("install.properties".into()))
}

/// Takes one file out of a zip, for artifacts that ship inside an archive.
pub fn extract_from_zip(bytes: &[u8], filter: &str) -> Result<Vec<u8>, MetaError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| MetaError::NotAnArchive(e.to_string()))?;
    let names: Vec<String> = archive.file_names().map(str::to_string).collect();
    let wanted = names
        .iter()
        .find(|name| crate::catalog::pin_matches(filter, name.rsplit('/').next().unwrap_or(name)))
        .ok_or_else(|| MetaError::NoVersion(format!("nothing in the archive matched {filter}")))?;

    let mut file = archive
        .by_name(wanted)
        .map_err(|e| MetaError::NotAnArchive(e.to_string()))?;
    let mut out = Vec::new();
    file.read_to_end(&mut out)
        .map_err(|e| MetaError::NotAnArchive(e.to_string()))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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
    fn the_version_comes_from_the_jar_not_the_filename() {
        // The case that used to produce the artifact "Multiverse" at
        // version "Core-5.8.1".
        let bytes = jar(&[("plugin.yml", "name: Multiverse-Core\nversion: 5.8.1\n")]);
        let meta = read_plugin_meta(&bytes).unwrap();
        assert_eq!(meta.version, "5.8.1");
        assert_eq!(meta.name.as_deref(), Some("Multiverse-Core"));
    }

    #[test]
    fn bungee_yml_wins_over_plugin_yml() {
        // A proxy plugin that ships both descriptors is a proxy plugin.
        let bytes = jar(&[
            ("plugin.yml", "name: Thing\nversion: 1.0.0\n"),
            ("bungee.yml", "name: Thing\nversion: 2.0.0\n"),
        ]);
        assert_eq!(read_plugin_meta(&bytes).unwrap().version, "2.0.0");
    }

    #[test]
    fn plugin_yml_wins_over_paper_plugin_yml() {
        let bytes = jar(&[
            ("paper-plugin.yml", "name: Thing\nversion: 9.9.9\n"),
            ("plugin.yml", "name: Thing\nversion: 1.0.0\n"),
        ]);
        assert_eq!(read_plugin_meta(&bytes).unwrap().version, "1.0.0");
    }

    #[test]
    fn an_unquoted_version_is_still_a_version() {
        // 1.2 is a float and 26 is an integer as far as YAML is concerned,
        // and plenty of real plugins write them unquoted.
        let bytes = jar(&[("plugin.yml", "name: Thing\nversion: 1.2\n")]);
        assert_eq!(read_plugin_meta(&bytes).unwrap().version, "1.2");
        let bytes = jar(&[("plugin.yml", "name: Thing\nversion: 26\n")]);
        assert_eq!(read_plugin_meta(&bytes).unwrap().version, "26");
    }

    #[test]
    fn velocity_plugins_use_id_instead_of_name() {
        let bytes = jar(&[(
            "velocity-plugin.json",
            r#"{"id":"redcraftchat","version":"0.2.5-SNAPSHOT"}"#,
        )]);
        let meta = read_plugin_meta(&bytes).unwrap();
        assert_eq!(meta.version, "0.2.5-SNAPSHOT");
        assert_eq!(meta.name.as_deref(), Some("redcraftchat"));
    }

    #[test]
    fn engines_report_the_minecraft_version_they_target() {
        let bytes = jar(&[(
            "version.json",
            r#"{"id":"26.2-2622","release_target":"26.2"}"#,
        )]);
        assert_eq!(read_version_json(&bytes).unwrap().version, "26.2");
    }

    #[test]
    fn a_jar_with_no_descriptor_says_so() {
        let bytes = jar(&[("META-INF/MANIFEST.MF", "Manifest-Version: 1.0\n")]);
        assert!(matches!(
            read_plugin_meta(&bytes),
            Err(MetaError::NoVersion(_))
        ));
    }

    #[test]
    fn something_that_is_not_a_zip_says_so() {
        assert!(matches!(
            read_plugin_meta(b"this is not a jar"),
            Err(MetaError::NotAnArchive(_))
        ));
    }

    #[test]
    fn extracting_from_a_zip_matches_on_the_file_name() {
        let bytes = jar(&[
            ("readme.txt", "hello"),
            ("Citizens/Citizens-2.0.35.jar", "jar bytes"),
        ]);
        let out = extract_from_zip(&bytes, "Citizens-*.jar").unwrap();
        assert_eq!(out, b"jar bytes");
    }
}
