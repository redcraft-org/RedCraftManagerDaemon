//! Finding out where to download an artifact from.
//!
//! Every source answers the same question: given a catalog entry, what is
//! the newest release and what URL serves it. Resolving is deliberately
//! separate from downloading, so the update check can ask "is there
//! anything new" without pulling a hundred megabytes to find out that
//! there isn't.

use serde::Deserialize;

use crate::catalog::{Artifact, SourceKind};

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("http error talking to {url}: {source}")]
    Http { url: String, source: reqwest::Error },
    #[error("{from} returned {status} for {url}")]
    Status {
        from: &'static str,
        status: u16,
        url: String,
    },
    #[error("could not read the {0} response: {1}")]
    Shape(&'static str, String),
    #[error("nothing matched {filter:?} in the newest {from} release")]
    NoMatch { from: &'static str, filter: String },
    #[error("{0}")]
    Unsupported(String),
}

/// What a source knows about the newest release, before downloading it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The direct download URL.
    pub url: String,
    /// What the source calls this release. Only a hint: the real version
    /// comes from inside the jar. Useful for skipping a download when the
    /// source already says it is the same one we have.
    pub upstream_id: Option<String>,
}

pub struct Resolver {
    client: reqwest::Client,
    github_token: Option<String>,
}

impl Resolver {
    /// The shared HTTP client, so callers outside this module do not spin up
    /// a second connection pool for one request.
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub fn new(github_token: Option<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent(concat!("rcmd/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("http client"),
            github_token,
        }
    }

    pub async fn resolve(&self, artifact: &Artifact) -> Result<Release, SourceError> {
        match artifact.source {
            SourceKind::Github => self.github(artifact).await,
            SourceKind::Modrinth => self.modrinth(artifact).await,
            SourceKind::Jenkins => self.jenkins(artifact).await,
            SourceKind::Papermc => self.papermc(artifact).await,
            SourceKind::Spiget => self.spiget(artifact).await,
            SourceKind::Fabricmc => self.fabricmc(artifact).await,
            SourceKind::Hangar => self.hangar(artifact).await,
            SourceKind::Manual => Err(SourceError::NoMatch {
                from: "manual",
                filter: artifact.name.clone(),
            }),
            // A URL that always points at the latest build. There is
            // nothing to resolve, and no way to know the version without
            // downloading it.
            SourceKind::Direct => Ok(Release {
                url: artifact.url.clone(),
                upstream_id: None,
            }),
        }
    }

    pub async fn download(&self, url: &str) -> Result<Vec<u8>, SourceError> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|source| SourceError::Http {
                url: url.into(),
                source,
            })?;
        if !response.status().is_success() {
            return Err(SourceError::Status {
                from: "download",
                status: response.status().as_u16(),
                url: url.into(),
            });
        }
        Ok(response
            .bytes()
            .await
            .map_err(|source| SourceError::Http {
                url: url.into(),
                source,
            })?
            .to_vec())
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        source: &'static str,
        url: &str,
    ) -> Result<T, SourceError> {
        let mut request = self.client.get(url);
        if source == "github" {
            if let Some(token) = &self.github_token {
                request = request.header("authorization", format!("Bearer {token}"));
            }
        }
        let response = request.send().await.map_err(|e| SourceError::Http {
            url: url.into(),
            source: e,
        })?;
        if !response.status().is_success() {
            return Err(SourceError::Status {
                from: source,
                status: response.status().as_u16(),
                url: url.into(),
            });
        }
        response
            .json()
            .await
            .map_err(|e| SourceError::Shape(source, e.to_string()))
    }

    async fn github(&self, artifact: &Artifact) -> Result<Release, SourceError> {
        #[derive(Deserialize)]
        struct GhAsset {
            name: String,
            browser_download_url: String,
        }
        #[derive(Deserialize)]
        struct GhRelease {
            tag_name: Option<String>,
            #[serde(default)]
            draft: bool,
            #[serde(default)]
            assets: Vec<GhAsset>,
        }

        let repo = github_repo(&artifact.url).ok_or_else(|| {
            SourceError::Unsupported(format!("{:?} is not a github repo url", artifact.url))
        })?;
        let api = format!("https://api.github.com/repos/{repo}/releases?per_page=20");
        let releases: Vec<GhRelease> = self.get_json("github", &api).await?;

        let filter = artifact.file_filter.as_deref().unwrap_or("*");
        for release in releases.iter().filter(|r| !r.draft) {
            for asset in &release.assets {
                if crate::catalog::pin_matches(filter, &asset.name) {
                    return Ok(Release {
                        url: asset.browser_download_url.clone(),
                        upstream_id: release.tag_name.clone(),
                    });
                }
            }
        }
        Err(SourceError::NoMatch {
            from: "github",
            filter: filter.into(),
        })
    }

    async fn modrinth(&self, artifact: &Artifact) -> Result<Release, SourceError> {
        #[derive(Deserialize)]
        struct MrFile {
            url: String,
            filename: String,
            #[serde(default)]
            primary: bool,
        }
        #[derive(Deserialize)]
        struct MrVersion {
            id: String,
            #[serde(default)]
            files: Vec<MrFile>,
        }

        let slug = modrinth_slug(&artifact.url).ok_or_else(|| {
            SourceError::Unsupported(format!("{:?} is not a modrinth project url", artifact.url))
        })?;
        let api = format!("https://api.modrinth.com/v2/project/{slug}/version");
        let versions: Vec<MrVersion> = self.get_json("modrinth", &api).await?;

        let filter = artifact.file_filter.as_deref().unwrap_or("*");
        for version in &versions {
            // Modrinth marks one file per version as the one you want; the
            // rest are sources jars and the like.
            let mut files: Vec<&MrFile> = version.files.iter().collect();
            files.sort_by_key(|f| !f.primary);
            for file in files {
                if crate::catalog::pin_matches(filter, &file.filename) {
                    return Ok(Release {
                        url: file.url.clone(),
                        upstream_id: Some(version.id.clone()),
                    });
                }
            }
        }
        Err(SourceError::NoMatch {
            from: "modrinth",
            filter: filter.into(),
        })
    }

    async fn jenkins(&self, artifact: &Artifact) -> Result<Release, SourceError> {
        #[derive(Deserialize)]
        struct JkArtifact {
            #[serde(rename = "fileName")]
            file_name: String,
            #[serde(rename = "relativePath")]
            relative_path: String,
        }
        #[derive(Deserialize)]
        struct JkBuild {
            number: Option<u64>,
            #[serde(default)]
            artifacts: Vec<JkArtifact>,
        }

        let base = artifact.url.trim_end_matches('/');
        let api = format!("{base}/lastSuccessfulBuild/api/json");
        let build: JkBuild = self.get_json("jenkins", &api).await?;

        let filter = artifact.file_filter.as_deref().unwrap_or("*");
        for entry in &build.artifacts {
            if crate::catalog::pin_matches(filter, &entry.file_name) {
                return Ok(Release {
                    url: format!(
                        "{base}/lastSuccessfulBuild/artifact/{}",
                        entry.relative_path
                    ),
                    upstream_id: build.number.map(|n| n.to_string()),
                });
            }
        }
        Err(SourceError::NoMatch {
            from: "jenkins",
            filter: filter.into(),
        })
    }

    async fn papermc(&self, artifact: &Artifact) -> Result<Release, SourceError> {
        #[derive(Deserialize)]
        struct PmDownload {
            name: String,
            url: String,
        }
        #[derive(Deserialize)]
        struct PmBuild {
            id: Option<u64>,
            #[serde(default)]
            downloads: std::collections::BTreeMap<String, PmDownload>,
        }

        // The url in the catalog names the project and the version, for
        // example https://fill.papermc.io/v3/projects/velocity/versions/3.4.0
        let base = artifact.url.trim_end_matches('/');
        let api = format!("{base}/builds/latest");
        let build: PmBuild = self.get_json("papermc", &api).await?;

        // The build number on its own is not something anybody can pin
        // against, so the version segment of the url goes in front of it.
        let version = base.rsplit('/').next().unwrap_or("");
        let identity = match build.id {
            Some(id) if !version.is_empty() => Some(format!("{version}-{id}")),
            Some(id) => Some(id.to_string()),
            None => (!version.is_empty()).then(|| version.to_string()),
        };

        let filter = artifact.file_filter.as_deref().unwrap_or("*");
        for download in build.downloads.values() {
            if crate::catalog::pin_matches(filter, &download.name) {
                return Ok(Release {
                    url: download.url.clone(),
                    upstream_id: identity,
                });
            }
        }
        Err(SourceError::NoMatch {
            from: "papermc",
            filter: filter.into(),
        })
    }

    /// Spiget is SpigotMC's read only API. It serves free resources
    /// without the browser challenge that makes scraping SpigotMC such a
    /// pleasure. Premium resources are not downloadable here, and you will
    /// have to upload those yourself.
    async fn spiget(&self, artifact: &Artifact) -> Result<Release, SourceError> {
        #[derive(Deserialize)]
        struct SpigetVersion {
            id: Option<u64>,
            name: Option<String>,
        }

        let id = spiget_id(&artifact.url).ok_or_else(|| {
            SourceError::Unsupported(format!("{:?} has no spigotmc resource id", artifact.url))
        })?;
        let api = format!("https://api.spiget.org/v2/resources/{id}/versions/latest");
        let version: SpigetVersion = self.get_json("spiget", &api).await?;
        Ok(Release {
            url: format!("https://api.spiget.org/v2/resources/{id}/download"),
            upstream_id: version.name.or_else(|| version.id.map(|i| i.to_string())),
        })
    }

    /// Hangar, PaperMC's plugin site.
    ///
    /// The catalog url is the project page, so
    /// https://hangar.papermc.io/DenizenScript/Denizen.
    async fn hangar(&self, artifact: &Artifact) -> Result<Release, SourceError> {
        #[derive(Deserialize)]
        struct HgFile {
            name: String,
        }
        #[derive(Deserialize)]
        struct HgDownload {
            #[serde(rename = "fileInfo")]
            file_info: Option<HgFile>,
            #[serde(rename = "downloadUrl")]
            download_url: Option<String>,
            #[serde(rename = "externalUrl")]
            external_url: Option<String>,
        }
        #[derive(Deserialize)]
        struct HgVersion {
            name: String,
            #[serde(default)]
            downloads: std::collections::BTreeMap<String, HgDownload>,
        }
        #[derive(Deserialize)]
        struct HgVersions {
            result: Vec<HgVersion>,
        }

        let slug = hangar_slug(&artifact.url).ok_or_else(|| {
            SourceError::Unsupported(format!("{:?} is not a hangar project url", artifact.url))
        })?;
        let api = format!("https://hangar.papermc.io/api/v1/projects/{slug}/versions?limit=10");
        let versions: HgVersions = self.get_json("hangar", &api).await?;

        let filter = artifact.file_filter.as_deref().unwrap_or("*");
        for version in &versions.result {
            for (platform, download) in &version.downloads {
                let name = download
                    .file_info
                    .as_ref()
                    .map(|info| info.name.as_str())
                    .unwrap_or(platform.as_str());
                if !crate::catalog::pin_matches(filter, name) {
                    continue;
                }
                // Hangar either hosts the file or points somewhere else.
                let url = download
                    .download_url
                    .clone()
                    .or_else(|| download.external_url.clone())
                    .unwrap_or_else(|| {
                        format!(
                            "https://hangar.papermc.io/api/v1/projects/{slug}/versions/{}/{platform}/download",
                            version.name
                        )
                    });
                return Ok(Release {
                    url,
                    upstream_id: Some(version.name.clone()),
                });
            }
        }
        Err(SourceError::NoMatch {
            from: "hangar",
            filter: filter.into(),
        })
    }

    /// The Fabric server jar, which nobody serves as one URL.
    ///
    /// The meta API answers with a list of loader versions for a Minecraft
    /// version and a separate list of installer versions, and the download
    /// path is built out of both. The catalog url points at the loader
    /// listing for the Minecraft version you want.
    async fn fabricmc(&self, artifact: &Artifact) -> Result<Release, SourceError> {
        #[derive(Deserialize)]
        struct Loader {
            version: String,
        }
        #[derive(Deserialize)]
        struct LoaderEntry {
            loader: Loader,
        }
        #[derive(Deserialize)]
        struct Installer {
            version: String,
        }

        let loaders: Vec<LoaderEntry> = self.get_json("fabricmc", &artifact.url).await?;
        let loader = loaders.first().ok_or_else(|| SourceError::NoMatch {
            from: "fabricmc",
            filter: "any loader version".into(),
        })?;
        let installers: Vec<Installer> = self.get_json("fabricmc", FABRIC_INSTALLERS).await?;
        let installer = installers.first().ok_or_else(|| SourceError::NoMatch {
            from: "fabricmc",
            filter: "any installer version".into(),
        })?;

        let base = artifact.url.trim_end_matches('/');
        Ok(Release {
            url: format!(
                "{base}/{}/{}/server/jar",
                loader.loader.version, installer.version
            ),
            upstream_id: Some(loader.loader.version.clone()),
        })
    }
}

/// `https://github.com/owner/repo` or any deeper url gives `owner/repo`.
fn github_repo(url: &str) -> Option<String> {
    let rest = url.split("github.com/").nth(1)?;
    let mut parts = rest.trim_start_matches('/').split('/');
    let owner = parts.next()?;
    let repo = parts.next()?.trim_end_matches(".git");
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

/// `https://modrinth.com/plugin/luckperms` gives `luckperms`.
fn modrinth_slug(url: &str) -> Option<String> {
    let rest = url.split("modrinth.com/").nth(1)?;
    let mut parts = rest.trim_start_matches('/').split('/');
    let _kind = parts.next()?;
    let slug = parts.next()?;
    if slug.is_empty() {
        return None;
    }
    Some(slug.split('?').next().unwrap_or(slug).to_string())
}

/// SpigotMC resource urls end in `.<id>/`, for example
/// `https://www.spigotmc.org/resources/plotsquared.77506/`.
/// The Fabric meta API's installer listing.
const FABRIC_INSTALLERS: &str = "https://meta.fabricmc.net/v2/versions/installer";

/// `https://hangar.papermc.io/Owner/Project` gives `Owner/Project`.
fn hangar_slug(url: &str) -> Option<String> {
    let rest = url.split("hangar.papermc.io/").nth(1)?;
    let mut parts = rest.trim_start_matches('/').split('/');
    let owner = parts.next()?;
    let project = parts.next()?;
    if owner.is_empty() || project.is_empty() {
        return None;
    }
    Some(format!("{owner}/{project}"))
}

fn spiget_id(url: &str) -> Option<String> {
    let trimmed = url.trim_end_matches('/');
    let last = trimmed.rsplit('/').next()?;
    let id = last.rsplit('.').next()?;
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_urls_reduce_to_owner_and_repo() {
        assert_eq!(
            github_repo("https://github.com/LuckPerms/LuckPerms").as_deref(),
            Some("LuckPerms/LuckPerms")
        );
        assert_eq!(
            github_repo("https://github.com/redcraft-org/RedCraftChat/releases").as_deref(),
            Some("redcraft-org/RedCraftChat")
        );
        assert_eq!(github_repo("https://example.invalid/thing"), None);
    }

    #[test]
    fn modrinth_urls_reduce_to_a_slug() {
        assert_eq!(
            modrinth_slug("https://modrinth.com/plugin/luckperms").as_deref(),
            Some("luckperms")
        );
        assert_eq!(
            modrinth_slug("https://modrinth.com/mod/fabric-api?version=1.21").as_deref(),
            Some("fabric-api")
        );
        assert_eq!(modrinth_slug("https://modrinth.com/"), None);
    }

    #[test]
    fn spigotmc_urls_give_up_their_resource_id() {
        assert_eq!(
            spiget_id("https://www.spigotmc.org/resources/plotsquared.77506/").as_deref(),
            Some("77506")
        );
        assert_eq!(
            spiget_id("https://www.spigotmc.org/resources/cmilib.87610").as_deref(),
            Some("87610")
        );
        // A resource without a numeric id is not something spiget can serve.
        assert_eq!(spiget_id("https://www.spigotmc.org/resources/thing"), None);
    }
}
