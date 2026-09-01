//! Building a server template.
//!
//! A template is a stack of layers. Each layer is a directory of files and
//! a list of artifacts, and a template can inherit from another one, so a
//! `crea_build` template can say "everything the `spigot` layer has, plus
//! these" without repeating it.
//!
//! Layers are applied parent first, and a child replaces a file wholesale
//! rather than merging into it. That is deliberate: merging two YAML files
//! and hoping the result is what either author meant is how you get a
//! config nobody can explain. If a child wants a different `server.properties`
//! it ships the whole file.

pub mod envsubst;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::artifacts::store::{self, ArtifactStore, Manifest};
use crate::catalog::{Catalog, Kind};

#[derive(Debug, thiserror::Error)]
pub enum TemplateError {
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
    #[error("template {0:?} does not exist")]
    Unknown(String),
    #[error("template {0:?} inherits from itself")]
    Cycle(String),
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Store(#[from] store::StoreError),
    #[error("io error: {0}")]
    Io(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateDef {
    /// The template this one builds on top of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherits: Option<String>,

    /// Whether this template can be built on its own. A shared base layer
    /// sets it to false, so nobody deploys "the common spigot bits" to a
    /// server by accident.
    #[serde(default = "yes")]
    pub buildable: bool,

    /// Artifact name to version pin. Pins accumulate down the chain and a
    /// child can repin what a parent set.
    #[serde(default)]
    pub artifacts: BTreeMap<String, String>,

    /// Placeholder to environment variable, for secrets. The placeholder
    /// is the literal string sitting in the config files.
    #[serde(default)]
    pub secrets: BTreeMap<String, String>,
}

fn yes() -> bool {
    true
}

/// A template definition plus where it lives.
#[derive(Debug, Clone)]
pub struct Template {
    pub name: String,
    pub def: TemplateDef,
    pub directory: PathBuf,
}

/// Every template found in the config directory.
#[derive(Debug, Clone, Default)]
pub struct Templates {
    pub by_name: BTreeMap<String, Template>,
    /// The config directory these came from, so a caller can find the file
    /// a template was read out of and edit it.
    pub root: PathBuf,
}

impl Templates {
    /// Loads `<root>/templates/<name>/template.yml` for every subdirectory.
    pub fn load(root: &Path) -> Result<Self, TemplateError> {
        let mut by_name = BTreeMap::new();
        let templates_dir = root.join("templates");
        if !templates_dir.exists() {
            return Ok(Self {
                root: root.to_path_buf(),
                ..Default::default()
            });
        }
        let entries = std::fs::read_dir(&templates_dir).map_err(|source| TemplateError::Read {
            path: templates_dir.display().to_string(),
            source,
        })?;
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let manifest = entry.path().join("template.yml");
            if !manifest.exists() {
                continue;
            }
            let text =
                std::fs::read_to_string(&manifest).map_err(|source| TemplateError::Read {
                    path: manifest.display().to_string(),
                    source,
                })?;
            let def: TemplateDef =
                serde_yaml::from_str(&text).map_err(|source| TemplateError::Parse {
                    path: manifest.display().to_string(),
                    source,
                })?;
            by_name.insert(
                name.clone(),
                Template {
                    name,
                    def,
                    directory: entry.path(),
                },
            );
        }
        let templates = Self {
            by_name,
            root: root.to_path_buf(),
        };
        templates.validate()?;
        Ok(templates)
    }

    fn validate(&self) -> Result<(), TemplateError> {
        for name in self.by_name.keys() {
            self.chain(name)?;
        }
        Ok(())
    }

    /// The layer order for a template, parent first.
    pub fn chain(&self, name: &str) -> Result<Vec<&Template>, TemplateError> {
        let mut chain = Vec::new();
        let mut seen = Vec::new();
        let mut current = Some(name.to_string());

        while let Some(step) = current {
            if seen.contains(&step) {
                return Err(TemplateError::Cycle(step));
            }
            seen.push(step.clone());
            let template = self
                .by_name
                .get(&step)
                .ok_or_else(|| TemplateError::Unknown(step.clone()))?;
            chain.push(template);
            current = template.def.inherits.clone();
        }
        chain.reverse();
        Ok(chain)
    }

    /// The pins and secrets a template ends up with, after inheritance.
    pub fn resolved(&self, name: &str) -> Result<TemplateDef, TemplateError> {
        let mut out = TemplateDef {
            buildable: self
                .by_name
                .get(name)
                .map(|t| t.def.buildable)
                .unwrap_or(true),
            ..Default::default()
        };
        for layer in self.chain(name)? {
            for (artifact, pin) in &layer.def.artifacts {
                out.artifacts.insert(artifact.clone(), pin.clone());
            }
            // Secrets accumulate too. In the tooling this replaces they were
            // only read from the last layer, which quietly meant every
            // template had to restate the ones its parent already declared.
            for (placeholder, variable) in &layer.def.secrets {
                out.secrets.insert(placeholder.clone(), variable.clone());
            }
        }
        Ok(out)
    }
}

/// What a build produced, and what it resolved to get there.
#[derive(Debug, Clone, Serialize)]
pub struct BuildReport {
    pub template: String,
    pub files: u64,
    pub artifacts: Vec<ResolvedArtifact>,
    /// Placeholders that had no value in the environment. These are not an
    /// error, because a template can legitimately be built without every
    /// secret, but a config that lands on a server with an empty password
    /// is worth saying out loud.
    pub missing_secrets: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedArtifact {
    pub name: String,
    pub pin: String,
    pub version: String,
    pub kind: Kind,
    pub sha256: String,
}

/// Builds a template into a directory.
pub async fn build(
    templates: &Templates,
    catalog: &Catalog,
    manifest: &dyn Manifest,
    store: &dyn ArtifactStore,
    name: &str,
    output: &Path,
    environment: &(dyn Fn(&str) -> Option<String> + Send + Sync),
) -> Result<BuildReport, TemplateError> {
    let chain = templates.chain(name)?;
    let resolved = templates.resolved(name)?;
    if !resolved.buildable {
        return Err(TemplateError::Invalid(format!(
            "{name} is a base layer, it is not meant to be built on its own"
        )));
    }

    std::fs::create_dir_all(output).map_err(|e| TemplateError::Io(e.to_string()))?;

    // Layer the config directories, parent first, whole files only.
    let mut files = 0u64;
    for layer in &chain {
        let config = layer.directory.join("config");
        if config.exists() {
            files += copy_tree(&config, output)?;
        }
    }

    // Then the artifacts, resolved against the manifest.
    let mut placed = Vec::new();
    for (artifact_name, pin) in &resolved.artifacts {
        let entry = catalog.get(artifact_name).ok_or_else(|| {
            TemplateError::Invalid(format!(
                "{name} pins {artifact_name:?}, which is not in the catalog"
            ))
        })?;
        let record = manifest.resolve(artifact_name, pin).await?.ok_or_else(|| {
            TemplateError::Invalid(format!(
                "no version of {artifact_name} matches {pin:?}, has it ever been fetched?"
            ))
        })?;

        // Engines become server.jar; everything else keeps its catalog name
        // without a version, so a config referring to `plugins/CMI.jar`
        // keeps working across upgrades.
        let destination = match entry.kind {
            Kind::Engine => output.join("server.jar"),
            kind => output
                .join(kind.directory())
                .join(format!("{artifact_name}.jar")),
        };
        store::place(store, &record.key, &destination).await?;
        placed.push(ResolvedArtifact {
            name: artifact_name.clone(),
            pin: pin.clone(),
            version: record.version.clone(),
            kind: entry.kind,
            sha256: record.sha256.clone(),
        });
    }

    let missing_secrets = envsubst::substitute(output, &resolved.secrets, environment)
        .map_err(|e| TemplateError::Io(e.to_string()))?;

    Ok(BuildReport {
        template: name.to_string(),
        files,
        artifacts: placed,
        missing_secrets,
    })
}

/// Copies a directory over another, replacing whole files.
fn copy_tree(from: &Path, to: &Path) -> Result<u64, TemplateError> {
    let mut count = 0;
    let entries = std::fs::read_dir(from).map_err(|source| TemplateError::Read {
        path: from.display().to_string(),
        source,
    })?;
    for entry in entries.flatten() {
        let source = entry.path();
        let destination = to.join(entry.file_name());
        if source.is_dir() {
            std::fs::create_dir_all(&destination).map_err(|e| TemplateError::Io(e.to_string()))?;
            count += copy_tree(&source, &destination)?;
        } else {
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent).map_err(|e| TemplateError::Io(e.to_string()))?;
            }
            std::fs::copy(&source, &destination).map_err(|e| TemplateError::Io(e.to_string()))?;
            count += 1;
        }
    }
    Ok(count)
}

/// Packs a built directory into a tar.
///
/// The tar is written outside the directory being packed. Writing it
/// inside means the archive contains a partial copy of itself, which the
/// previous builder did and which is exactly as useful as it sounds.
pub fn pack(built: &Path, tar_path: &Path) -> Result<u64, TemplateError> {
    if tar_path.starts_with(built) {
        return Err(TemplateError::Invalid(
            "the tar would be written inside the directory it packs".into(),
        ));
    }
    if let Some(parent) = tar_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| TemplateError::Io(e.to_string()))?;
    }
    let file = std::fs::File::create(tar_path).map_err(|e| TemplateError::Io(e.to_string()))?;
    let mut builder = tar::Builder::new(file);
    builder.follow_symlinks(false);
    builder
        .append_dir_all(".", built)
        .map_err(|e| TemplateError::Io(e.to_string()))?;
    builder
        .into_inner()
        .map_err(|e| TemplateError::Io(e.to_string()))?;
    Ok(std::fs::metadata(tar_path)
        .map_err(|e| TemplateError::Io(e.to_string()))?
        .len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        write(
            &root.join("templates/base/template.yml"),
            "buildable: false\nartifacts:\n  CMI: '9.8.*'\nsecrets:\n  MYSQL_PASSWORD: DB_PASSWORD\n",
        );
        write(
            &root.join("templates/base/config/server.properties"),
            "motd=base\n",
        );
        write(
            &root.join("templates/base/config/bukkit.yml"),
            "settings: {}\n",
        );

        write(
            &root.join("templates/hub/template.yml"),
            "inherits: base\nartifacts:\n  CMI: '9.9.*'\n  LuckPerms: '5.5.*'\n",
        );
        write(
            &root.join("templates/hub/config/server.properties"),
            "motd=hub\n",
        );
        dir
    }

    #[test]
    fn the_chain_runs_parent_first() {
        let dir = fixture();
        let templates = Templates::load(dir.path()).unwrap();
        let chain: Vec<&str> = templates
            .chain("hub")
            .unwrap()
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(chain, vec!["base", "hub"]);
    }

    #[test]
    fn pins_accumulate_and_a_child_can_repin() {
        let dir = fixture();
        let templates = Templates::load(dir.path()).unwrap();
        let resolved = templates.resolved("hub").unwrap();
        assert_eq!(resolved.artifacts.get("CMI").unwrap(), "9.9.*");
        assert_eq!(resolved.artifacts.get("LuckPerms").unwrap(), "5.5.*");
    }

    #[test]
    fn secrets_are_inherited_rather_than_restated() {
        let dir = fixture();
        let templates = Templates::load(dir.path()).unwrap();
        // hub declares none of its own, but it gets the parent's.
        let resolved = templates.resolved("hub").unwrap();
        assert_eq!(
            resolved.secrets.get("MYSQL_PASSWORD").unwrap(),
            "DB_PASSWORD"
        );
    }

    #[test]
    fn a_child_replaces_a_whole_file_and_keeps_the_rest() {
        let dir = fixture();
        let templates = Templates::load(dir.path()).unwrap();
        let out = dir.path().join("built");
        std::fs::create_dir_all(&out).unwrap();
        for layer in templates.chain("hub").unwrap() {
            let config = layer.directory.join("config");
            if config.exists() {
                copy_tree(&config, &out).unwrap();
            }
        }
        assert_eq!(
            std::fs::read_to_string(out.join("server.properties")).unwrap(),
            "motd=hub\n"
        );
        // The parent's other file is still there.
        assert!(out.join("bukkit.yml").exists());
    }

    #[test]
    fn a_base_layer_refuses_to_be_built() {
        let dir = fixture();
        let templates = Templates::load(dir.path()).unwrap();
        assert!(!templates.resolved("base").unwrap().buildable);
    }

    #[test]
    fn a_cycle_is_caught_at_load_rather_than_at_build() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("templates/a/template.yml"),
            "inherits: b\n",
        );
        write(
            &dir.path().join("templates/b/template.yml"),
            "inherits: a\n",
        );
        assert!(matches!(
            Templates::load(dir.path()),
            Err(TemplateError::Cycle(_))
        ));
    }

    #[test]
    fn inheriting_from_something_that_does_not_exist_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("templates/a/template.yml"),
            "inherits: nowhere\n",
        );
        assert!(matches!(
            Templates::load(dir.path()),
            Err(TemplateError::Unknown(_))
        ));
    }

    #[test]
    fn the_tar_is_not_written_inside_itself() {
        let dir = tempfile::tempdir().unwrap();
        let built = dir.path().join("built");
        std::fs::create_dir_all(&built).unwrap();
        std::fs::write(built.join("a.txt"), "x").unwrap();

        assert!(pack(&built, &built.join("self.tar")).is_err());
        let outside = dir.path().join("out/hub.tar");
        assert!(pack(&built, &outside).unwrap() > 0);

        // And it round trips through the applier.
        let restored = dir.path().join("restored");
        std::fs::create_dir_all(&restored).unwrap();
        crate::servers::apply::apply_tar(&outside, &restored).unwrap();
        assert_eq!(
            std::fs::read_to_string(restored.join("a.txt")).unwrap(),
            "x"
        );
    }
}
