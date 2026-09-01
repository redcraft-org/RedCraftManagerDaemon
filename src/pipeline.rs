//! One run of the whole chain: refresh the catalog, fetch what changed,
//! rebuild the templates that care, and leave the result ready to apply.
//!
//! Doing this in one process is the point of the whole project. The
//! tooling this replaces ran the artifact download and the template build
//! as two scheduled jobs an hour apart, and "an hour" was the entire
//! mechanism ensuring the second one saw the first one's output. When a
//! download ran long, or somebody triggered a build by hand, the templates
//! were quietly built against yesterday's jars.
//!
//! Here the ordering is a function call, so it cannot be skipped.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::artifacts::{self, store::ArtifactStore, store::Manifest, FetchOutcome};
use crate::catalog::Catalog;
use crate::gitconfig::{self, ConfigRepo, Proposal};
use crate::templates::{self, Templates};

#[derive(Debug, Clone, Serialize)]
pub struct PipelineReport {
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: chrono::DateTime<chrono::Utc>,
    pub fetched: Vec<FetchOutcome>,
    pub builds: Vec<BuildOutcome>,
    /// Pin bumps pushed for review. Empty when no config repository is set
    /// up, which is the default.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub proposals: Vec<Proposal>,
    /// True when everything that was attempted worked. A run with failures
    /// is still a useful run: fifty nine plugins updating and one broken
    /// source is not a reason to ship nothing.
    pub clean: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BuildOutcome {
    pub template: String,
    pub succeeded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tar: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tar_sha256: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub missing_secrets: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<templates::ResolvedArtifact>,
}

pub struct Pipeline<'a> {
    pub catalog: &'a Catalog,
    pub templates: &'a Templates,
    pub manifest: &'a dyn Manifest,
    pub store: &'a dyn ArtifactStore,
    pub resolver: &'a artifacts::sources::Resolver,
    /// Where built tars are written.
    pub output: PathBuf,
    /// Where the build happens before being packed.
    pub workspace: PathBuf,
    /// The config repository, when the configuration is in git. Without it
    /// rcmd still runs, it just cannot propose anything.
    pub repo: Option<&'a ConfigRepo>,
}

impl Pipeline<'_> {
    /// Fetches every enabled artifact, then rebuilds the templates whose
    /// resolution actually changed.
    pub async fn run(&self) -> PipelineReport {
        let started_at = chrono::Utc::now();

        let mut fetched = Vec::new();
        for artifact in self.catalog.artifacts.iter().filter(|a| a.enabled) {
            let outcome =
                artifacts::fetch_one(self.resolver, self.store, self.manifest, artifact).await;
            if !outcome.succeeded {
                // One broken source does not stop the run, it just gets
                // reported. The alternative is a single dead URL freezing
                // every other update indefinitely.
                tracing::warn!(
                    "{}: {}",
                    outcome.name,
                    outcome.error.as_deref().unwrap_or("failed")
                );
            }
            fetched.push(outcome);
        }

        // Only rebuild what changed. An artifact that came back unchanged
        // cannot move any pin.
        let changed: BTreeSet<&str> = fetched
            .iter()
            .filter(|outcome| outcome.action == "stored")
            .map(|outcome| outcome.name.as_str())
            .collect();

        let mut builds = Vec::new();
        for name in self.templates_to_build(&changed) {
            builds.push(self.build_one(&name).await);
        }

        let proposals = self.propose_updates().await;

        let clean = fetched.iter().all(|f| f.succeeded) && builds.iter().all(|b| b.succeeded);
        PipelineReport {
            started_at,
            finished_at: chrono::Utc::now(),
            fetched,
            builds,
            proposals,
            clean,
        }
    }

    /// Pushes a branch for every artifact that has moved past its pin.
    ///
    /// A version inside the pin needs no decision from anybody: it gets
    /// built and shipped at the next window. A version outside it is a
    /// choice somebody has to make, so it becomes a pull request rather
    /// than a surprise.
    async fn propose_updates(&self) -> Vec<Proposal> {
        let Some(repo) = self.repo else {
            return vec![];
        };
        let mut proposals = Vec::new();
        for (artifact, newest) in self.versions_past_their_pin().await {
            let mut edits = BTreeMap::new();
            let mut widened = None;
            for name in self.templates.by_name.keys() {
                let Ok(resolved) = self.templates.resolved(name) else {
                    continue;
                };
                let Some(pin) = resolved.artifacts.get(&artifact) else {
                    continue;
                };
                if crate::catalog::pin_matches(pin, &newest) {
                    continue;
                }
                let new_pin = crate::catalog::widen_pin(pin, &newest);
                let path = PathBuf::from("templates").join(name).join("template.yml");
                let Ok(text) = std::fs::read_to_string(self.templates.root.join(&path)) else {
                    continue;
                };
                // A template that inherits its pin has nothing to rewrite
                // here; the parent that declares it does.
                if let Some(rewritten) = gitconfig::rewrite_pins(&text, &artifact, &new_pin) {
                    edits.insert(path, rewritten);
                    widened = Some((pin.clone(), new_pin));
                }
            }
            let Some((old_pin, new_pin)) = widened else {
                continue;
            };
            let body = gitconfig::proposal_body(&artifact, &old_pin, &new_pin, &newest);
            match repo
                .propose_pin_bump(&artifact, &newest, &edits, &body)
                .await
            {
                Ok(proposal) => {
                    if proposal.pushed {
                        tracing::info!("pushed {} for review", proposal.branch);
                    }
                    proposals.push(proposal);
                }
                Err(e) => tracing::warn!("could not propose {artifact} {newest}: {e}"),
            }
        }
        proposals
    }

    /// Artifacts whose newest known version no template pin accepts.
    async fn versions_past_their_pin(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for artifact in self.catalog.artifacts.iter().filter(|a| a.enabled) {
            let pins = self.pins_for(&artifact.name);
            if pins.is_empty() {
                continue;
            }
            let Ok(mut versions) = self.manifest.versions(&artifact.name).await else {
                continue;
            };
            versions.sort_by_key(|record| record.recorded_at);
            let Some(newest) = versions.pop() else {
                continue;
            };
            if !pins
                .iter()
                .any(|pin| crate::catalog::pin_matches(pin, &newest.version))
            {
                out.push((artifact.name.clone(), newest.version));
            }
        }
        out
    }

    /// Every pin any template puts on one artifact.
    fn pins_for(&self, artifact: &str) -> Vec<String> {
        self.templates
            .by_name
            .keys()
            .filter_map(|name| self.templates.resolved(name).ok())
            .filter_map(|resolved| resolved.artifacts.get(artifact).cloned())
            .collect()
    }

    /// Which buildable templates pin at least one artifact that changed.
    fn templates_to_build(&self, changed: &BTreeSet<&str>) -> Vec<String> {
        let mut out = Vec::new();
        for name in self.templates.by_name.keys() {
            let Ok(resolved) = self.templates.resolved(name) else {
                continue;
            };
            if !resolved.buildable {
                continue;
            }
            if resolved
                .artifacts
                .keys()
                .any(|artifact| changed.contains(artifact.as_str()))
            {
                out.push(name.clone());
            }
        }
        out
    }

    /// Builds one template all the way to a packed tar.
    pub async fn build_one(&self, name: &str) -> BuildOutcome {
        let workspace = self.workspace.join(name);
        let _ = std::fs::remove_dir_all(&workspace);

        let report = match templates::build(
            self.templates,
            self.catalog,
            self.manifest,
            self.store,
            name,
            &workspace,
            &|variable| std::env::var(variable).ok(),
        )
        .await
        {
            Ok(report) => report,
            Err(e) => {
                return BuildOutcome {
                    template: name.to_string(),
                    succeeded: false,
                    tar: None,
                    tar_sha256: None,
                    missing_secrets: vec![],
                    error: Some(e.to_string()),
                    artifacts: vec![],
                }
            }
        };

        if !report.missing_secrets.is_empty() {
            tracing::warn!(
                "{name} was built with {} placeholder(s) left unfilled: {}",
                report.missing_secrets.len(),
                report.missing_secrets.join(", ")
            );
        }

        let tar_path = self.output.join(format!("{name}.tar"));
        match templates::pack(&workspace, &tar_path) {
            Ok(_) => {
                let sha = std::fs::read(&tar_path)
                    .ok()
                    .map(|b| artifacts::sha256_hex(&b));
                BuildOutcome {
                    template: name.to_string(),
                    succeeded: true,
                    tar: Some(tar_path.display().to_string()),
                    tar_sha256: sha,
                    missing_secrets: report.missing_secrets,
                    error: None,
                    artifacts: report.artifacts,
                }
            }
            Err(e) => BuildOutcome {
                template: name.to_string(),
                succeeded: false,
                tar: None,
                tar_sha256: None,
                missing_secrets: report.missing_secrets,
                error: Some(e.to_string()),
                artifacts: report.artifacts,
            },
        }
    }
}

/// Loads the catalog and templates out of a config directory.
pub fn load_config(root: &Path) -> anyhow::Result<(Catalog, Templates)> {
    let catalog_path = root.join("catalog.yml");
    let catalog = if catalog_path.exists() {
        Catalog::load(&catalog_path)?
    } else {
        Catalog::default()
    };
    let templates = Templates::load(root)?;
    Ok((catalog, templates))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Artifact, Kind, PostProcessor, SourceKind};

    fn catalog() -> Catalog {
        Catalog {
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
                    name: "LuckPerms".into(),
                    kind: Kind::Plugin,
                    source: SourceKind::Direct,
                    url: "https://example.invalid/lp.jar".into(),
                    file_filter: None,
                    archive_filter: None,
                    post_processors: vec![PostProcessor::Plugin],
                    enabled: true,
                },
            ],
        }
    }

    fn templates(dir: &Path) -> Templates {
        let write = |path: PathBuf, text: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write(
            dir.join("templates/hub/template.yml"),
            "artifacts:\n  CMI: '*'\n",
        );
        write(
            dir.join("templates/survival/template.yml"),
            "artifacts:\n  LuckPerms: '*'\n",
        );
        write(
            dir.join("templates/base/template.yml"),
            "buildable: false\nartifacts:\n  CMI: '*'\n",
        );
        Templates::load(dir).unwrap()
    }

    #[test]
    fn only_templates_pinning_a_changed_artifact_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let templates = templates(dir.path());
        let catalog = catalog();
        let manifest = crate::artifacts::store::tests_support::MemoryManifest::default();
        let store = crate::artifacts::store::DirectoryStore::new(dir.path().join("store"));
        let resolver = artifacts::sources::Resolver::new(None);
        let pipeline = Pipeline {
            catalog: &catalog,
            templates: &templates,
            manifest: &manifest,
            store: &store,
            resolver: &resolver,
            output: dir.path().join("out"),
            workspace: dir.path().join("work"),
            repo: None,
        };

        let mut changed = BTreeSet::new();
        changed.insert("CMI");
        assert_eq!(pipeline.templates_to_build(&changed), vec!["hub"]);

        let mut changed = BTreeSet::new();
        changed.insert("LuckPerms");
        assert_eq!(pipeline.templates_to_build(&changed), vec!["survival"]);

        // Nothing changed, nothing rebuilds.
        assert!(pipeline.templates_to_build(&BTreeSet::new()).is_empty());
    }

    #[test]
    fn a_base_layer_never_builds_even_when_its_pins_move() {
        let dir = tempfile::tempdir().unwrap();
        let templates = templates(dir.path());
        let catalog = catalog();
        let manifest = crate::artifacts::store::tests_support::MemoryManifest::default();
        let store = crate::artifacts::store::DirectoryStore::new(dir.path().join("store"));
        let resolver = artifacts::sources::Resolver::new(None);
        let pipeline = Pipeline {
            catalog: &catalog,
            templates: &templates,
            manifest: &manifest,
            store: &store,
            resolver: &resolver,
            output: dir.path().join("out"),
            workspace: dir.path().join("work"),
            repo: None,
        };
        let mut changed = BTreeSet::new();
        changed.insert("CMI");
        // base pins CMI too, but it is not buildable.
        assert!(!pipeline
            .templates_to_build(&changed)
            .contains(&"base".to_string()));
    }
}
