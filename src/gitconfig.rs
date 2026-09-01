//! The config repository.
//!
//! Everything rcmd is configured with lives in git: the catalog, the
//! templates, the policy data. The daemon is a reader. It hard resets its
//! checkout to whatever the remote says and never commits on the main
//! branch, so a change somebody made through the web UI can never quietly
//! diverge from what the repository claims is deployed.
//!
//! The one thing rcmd writes is a branch: when an artifact moves past its
//! pin, it pushes `update/<artifact>-<version>` and leaves a pull request
//! for a person to look at. Same review as anybody else's change, which is
//! the point of putting the config in git in the first place.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;

use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git is not installed, or not on the PATH")]
    NotInstalled,
    #[error("git {command} failed: {message}")]
    Failed { command: String, message: String },
    #[error("{0} is not a git checkout")]
    NotACheckout(String),
    #[error("io error: {0}")]
    Io(String),
}

/// The environment variable the credential helper reads the token from.
const TOKEN_VARIABLE: &str = "RCMD_GIT_TOKEN";

/// A git credential helper that answers from the environment.
///
/// `x-access-token` is the username GitHub expects when the password is a
/// token, and it works for both classic and fine grained ones.
const CREDENTIAL_HELPER: &str = concat!(
    "credential.helper=!f() { echo username=x-access-token; ",
    "echo \"password=$RCMD_GIT_TOKEN\"; }; f"
);

/// A checkout of the config repository.
pub struct ConfigRepo {
    root: PathBuf,
    /// The branch the daemon follows. Never committed to locally.
    branch: String,
    /// Used to open the pull request once the branch is pushed, and to
    /// push over https. Without one the branch does not land at all.
    ///
    /// Deliberately its own token rather than the one the artifact
    /// resolver uses. That one is sent to every repository a catalog entry
    /// points at, and a token scoped to just the config repository would be
    /// refused by all of them, so sharing one would mean turning on pull
    /// requests silently broke plugin downloads.
    github_token: Option<String>,
    /// Who the pin bump commits are from. A daemon cannot rely on whatever
    /// happens to be in somebody's global git config, and on a fresh box
    /// there is nothing in it at all.
    author_name: String,
    author_email: String,
    client: reqwest::Client,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncReport {
    pub before: String,
    pub after: String,
    pub changed: bool,
}

impl ConfigRepo {
    pub fn new(
        root: impl Into<PathBuf>,
        branch: impl Into<String>,
        github_token: Option<String>,
    ) -> Self {
        Self {
            root: root.into(),
            branch: branch.into(),
            github_token,
            author_name: std::env::var("GIT_AUTHOR_NAME").unwrap_or_else(|_| "rcmd".into()),
            author_email: std::env::var("GIT_AUTHOR_EMAIL")
                .unwrap_or_else(|_| "rcmd@localhost".into()),
            client: reqwest::Client::new(),
        }
    }

    pub fn is_checkout(&self) -> bool {
        self.root.join(".git").exists()
    }

    /// Fetches and hard resets to the remote branch.
    ///
    /// :warning: This throws away local changes in the checkout on purpose.
    /// The checkout is a cache of what the repository says, not somewhere
    /// to edit files. Edit them in the repository and let them come back.
    pub async fn sync(&self) -> Result<SyncReport, GitError> {
        if !self.is_checkout() {
            return Err(GitError::NotACheckout(self.root.display().to_string()));
        }
        let before = self.head().await?;
        self.git(&["fetch", "--prune", "origin"]).await?;
        self.git(&["reset", "--hard", &format!("origin/{}", self.branch)])
            .await?;
        let after = self.head().await?;
        Ok(SyncReport {
            changed: before != after,
            before,
            after,
        })
    }

    pub async fn head(&self) -> Result<String, GitError> {
        Ok(self.git(&["rev-parse", "HEAD"]).await?.trim().to_string())
    }

    /// Opens or updates a branch carrying one pin bump.
    ///
    /// The branch is named after the artifact and the version, so a second
    /// run for the same version force pushes over its own branch instead of
    /// stacking commits onto somebody else's. Sharing one long lived branch
    /// across every update is how the previous tool ended up with pull
    /// requests whose title and contents disagreed.
    pub async fn propose_pin_bump(
        &self,
        artifact: &str,
        version: &str,
        edits: &BTreeMap<PathBuf, String>,
        body: &str,
    ) -> Result<Proposal, GitError> {
        if !self.is_checkout() {
            return Err(GitError::NotACheckout(self.root.display().to_string()));
        }
        let branch = branch_name(artifact, version);

        // Always branch from the remote tip, never from whatever the
        // checkout happens to be sitting on.
        self.git(&["fetch", "--prune", "origin"]).await?;
        self.git(&[
            "checkout",
            "-B",
            &branch,
            &format!("origin/{}", self.branch),
        ])
        .await?;

        for (relative, contents) in edits {
            let path = self.root.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| GitError::Io(e.to_string()))?;
            }
            std::fs::write(&path, contents).map_err(|e| GitError::Io(e.to_string()))?;
        }

        // `add -u` only: a stray file that wandered into the checkout is
        // not part of anybody's pin bump.
        self.git(&["add", "-u"]).await?;
        let staged = self.git(&["diff", "--cached", "--name-only"]).await?;
        let unchanged = Proposal {
            branch: branch.clone(),
            title: title_for(artifact, version),
            body: body.to_string(),
            pushed: false,
            changed_files: vec![],
            url: None,
        };
        if staged.trim().is_empty() {
            // The pin is already what we wanted to set it to.
            self.git(&["checkout", &self.branch]).await.ok();
            return Ok(unchanged);
        }
        // A branch already proposing exactly this must not be force pushed
        // again. Doing that every night turns one pull request into a wall
        // of "force-pushed" events and buries whatever a reviewer said.
        if self.remote_branch_exists(&branch).await
            && self
                .git(&["diff", "--cached", "--quiet", &format!("origin/{branch}")])
                .await
                .is_ok()
        {
            self.git(&["checkout", &self.branch]).await.ok();
            return Ok(unchanged);
        }

        let title = title_for(artifact, version);
        self.git(&["commit", "-m", &title]).await?;
        self.git(&["push", "--force-with-lease", "origin", &branch])
            .await?;
        let changed_files = staged.lines().map(|line| line.to_string()).collect();
        self.git(&["checkout", &self.branch]).await.ok();

        let url = self.open_pull_request(&branch, &title, body).await;
        Ok(Proposal {
            branch,
            title,
            body: body.to_string(),
            pushed: true,
            changed_files,
            url,
        })
    }

    /// Opens the pull request, or says nothing if it cannot.
    ///
    /// A branch nobody can see is not much of a proposal, but failing the
    /// whole run because GitHub was slow would be worse. The branch is
    /// pushed either way and the log says what happened.
    async fn open_pull_request(&self, branch: &str, title: &str, body: &str) -> Option<String> {
        let token = self.github_token.as_deref()?;
        let remote = self.git(&["remote", "get-url", "origin"]).await.ok()?;
        let slug = github_slug(remote.trim())?;

        let response = self
            .client
            .post(format!("https://api.github.com/repos/{slug}/pulls"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "rcmd")
            .timeout(std::time::Duration::from_secs(30))
            .json(&serde_json::json!({
                "title": title,
                "body": body,
                "head": branch,
                "base": self.branch,
            }))
            .send()
            .await
            .ok()?;

        let status = response.status();
        let payload: serde_json::Value = response.json().await.ok()?;
        if status.is_success() {
            return payload
                .get("html_url")
                .and_then(|url| url.as_str())
                .map(str::to_string);
        }

        // A pull request already open for this branch is the normal case on
        // a rerun, not a failure.
        let message = payload
            .get("errors")
            .and_then(|errors| errors.get(0))
            .and_then(|first| first.get("message"))
            .and_then(|message| message.as_str())
            .unwrap_or("");
        if message.contains("already exists") {
            tracing::info!("{branch} already has a pull request open");
        } else {
            tracing::warn!("could not open a pull request for {branch}: {status} {payload}");
        }
        None
    }

    /// Opens a branch and a pull request for an arbitrary set of edits.
    ///
    /// Same machinery as a pin bump, but the caller decides the branch and
    /// the title, because "somebody asked for this plugin to be added" is
    /// not an update and should not look like one in the branch list.
    pub async fn propose(
        &self,
        branch_suffix: &str,
        title: &str,
        body: &str,
        edits: &BTreeMap<PathBuf, String>,
    ) -> Result<Proposal, GitError> {
        if !self.is_checkout() {
            return Err(GitError::NotACheckout(self.root.display().to_string()));
        }
        let branch = format!("change/{}", slug(branch_suffix));

        self.git(&["fetch", "--prune", "origin"]).await?;
        self.git(&[
            "checkout",
            "-B",
            &branch,
            &format!("origin/{}", self.branch),
        ])
        .await?;
        for (relative, contents) in edits {
            let path = self.root.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| GitError::Io(e.to_string()))?;
            }
            std::fs::write(&path, contents).map_err(|e| GitError::Io(e.to_string()))?;
        }
        // Add new files too: a catalog entry can bring a template with it.
        self.git(&["add", "-A"]).await?;
        let staged = self.git(&["diff", "--cached", "--name-only"]).await?;
        if staged.trim().is_empty() {
            self.git(&["checkout", &self.branch]).await.ok();
            return Ok(Proposal {
                branch,
                title: title.to_string(),
                body: body.to_string(),
                pushed: false,
                changed_files: vec![],
                url: None,
            });
        }
        self.git(&["commit", "-m", title]).await?;
        self.git(&["push", "--force-with-lease", "origin", &branch])
            .await?;
        let changed_files = staged.lines().map(|line| line.to_string()).collect();
        self.git(&["checkout", &self.branch]).await.ok();
        let url = self.open_pull_request(&branch, title, body).await;
        Ok(Proposal {
            branch,
            title: title.to_string(),
            body: body.to_string(),
            pushed: true,
            changed_files,
            url,
        })
    }

    async fn remote_branch_exists(&self, branch: &str) -> bool {
        self.git(&["rev-parse", "--verify", &format!("origin/{branch}")])
            .await
            .is_ok()
    }

    async fn git(&self, args: &[&str]) -> Result<String, GitError> {
        let mut command = tokio::process::Command::new("git");
        if self.github_token.is_some() {
            // Authenticate over https without ever putting the token in the
            // process arguments, where anybody running ps could read it. The
            // helper reads it from the environment instead, which is only
            // readable by the user running the process. The empty helper
            // first clears whatever is configured globally, so ours is the
            // only one asked.
            command
                .arg("-c")
                .arg("credential.helper=")
                .arg("-c")
                .arg(CREDENTIAL_HELPER);
        }
        let output = command
            .args(args)
            .current_dir(&self.root)
            // A prompt for a password in a daemon is a hang, not a prompt.
            .env("GIT_TERMINAL_PROMPT", "0")
            .env(
                TOKEN_VARIABLE,
                self.github_token.clone().unwrap_or_default(),
            )
            .env("GIT_AUTHOR_NAME", &self.author_name)
            .env("GIT_AUTHOR_EMAIL", &self.author_email)
            .env("GIT_COMMITTER_NAME", &self.author_name)
            .env("GIT_COMMITTER_EMAIL", &self.author_email)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    GitError::NotInstalled
                } else {
                    GitError::Io(e.to_string())
                }
            })?;
        if !output.status.success() {
            return Err(GitError::Failed {
                command: args.join(" "),
                message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

/// A branch pushed and waiting for somebody to look at it.
#[derive(Debug, Clone, Serialize)]
pub struct Proposal {
    pub branch: String,
    pub title: String,
    pub body: String,
    /// False when the bump turned out to change nothing.
    pub pushed: bool,
    pub changed_files: Vec<String>,
    /// The pull request, when rcmd had a token to open one with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// `git@github.com:owner/repo.git` or the https form gives `owner/repo`.
fn github_slug(remote: &str) -> Option<String> {
    let rest = remote
        .split_once("github.com")
        .map(|(_, rest)| rest.trim_start_matches([':', '/']))?;
    let rest = rest.trim_end_matches(".git").trim_end_matches('/');
    let mut parts = rest.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    (!owner.is_empty() && !repo.is_empty()).then(|| format!("{owner}/{repo}"))
}

/// One branch per artifact and version, so a rerun replaces itself.
fn branch_name(artifact: &str, version: &str) -> String {
    format!("update/{}-{}", slug(artifact), slug(version))
}

fn title_for(artifact: &str, version: &str) -> String {
    format!("Update {artifact} to {version}")
}

/// Makes a branch-safe name out of anything an upstream calls a version.
fn slug(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    // git refuses a few shapes outright: no leading dot, no double dot, no
    // trailing lock. Trimming to something plain avoids all of them.
    cleaned
        .trim_matches(|c: char| c == '.' || c == '-')
        .replace("..", "-")
}

/// Rewrites the pins in a template file, returning the new text.
///
/// The file is edited as text rather than round tripped through the YAML
/// parser, because a parser would helpfully reformat the whole file and
/// turn a one-line pin bump into a diff nobody wants to review.
pub fn rewrite_pins(text: &str, artifact: &str, new_pin: &str) -> Option<String> {
    let mut changed = false;
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let trimmed = line.trim_start();
        let matches_artifact = trimmed
            .strip_prefix(artifact)
            .map(|rest| rest.trim_start().starts_with(':'))
            .unwrap_or(false);
        if matches_artifact {
            let indent = &line[..line.len() - trimmed.len()];
            out.push_str(indent);
            out.push_str(artifact);
            out.push_str(": '");
            out.push_str(new_pin);
            out.push('\'');
            changed = true;
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    changed.then_some(out)
}

/// The body of a pin bump pull request.
pub fn proposal_body(artifact: &str, old_pin: &str, new_pin: &str, version: &str) -> String {
    format!(
        "`{artifact}` released `{version}`, which is outside the current pin.\n\n\
         - Pin was `{old_pin}`\n\
         - Pin becomes `{new_pin}`\n\n\
         Nothing is deployed by merging this on its own. The next build picks \
         the new pin up, and the next restart window ships it.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn a_branch_name_survives_whatever_upstream_calls_a_version() {
        assert_eq!(branch_name("CMI", "9.8.1"), "update/CMI-9.8.1");
        assert_eq!(
            branch_name("Chunky", "1.4.28 (build 42)"),
            "update/Chunky-1.4.28--build-42"
        );
        // git will not take a ref that starts with a dot or ends in one.
        assert_eq!(branch_name("Thing", ".1.0."), "update/Thing-1.0");
        assert!(!branch_name("A", "../../etc").contains(".."));
    }

    #[test]
    fn rewriting_a_pin_touches_one_line_and_keeps_the_indent() {
        let text = "buildable: true\nartifacts:\n  CMI: '9.7.*'\n  LuckPerms: '5.*'\n";
        let out = rewrite_pins(text, "CMI", "9.8.*").unwrap();
        assert_eq!(
            out,
            "buildable: true\nartifacts:\n  CMI: '9.8.*'\n  LuckPerms: '5.*'\n"
        );
    }

    #[test]
    fn an_artifact_that_is_not_pinned_here_changes_nothing() {
        let text = "artifacts:\n  CMI: '9.7.*'\n";
        assert!(rewrite_pins(text, "LuckPerms", "5.*").is_none());
        // And a name that is only a prefix of another must not match.
        let text = "artifacts:\n  CMILib: '1.4.*'\n";
        assert!(rewrite_pins(text, "CMI", "9.8.*").is_none());
    }

    #[test]
    fn the_credential_helper_never_carries_the_token_itself() {
        // The token has to reach git through the environment. Putting it in
        // the helper string would put it in the process arguments, where
        // anybody running ps can read it.
        assert!(CREDENTIAL_HELPER.contains("$RCMD_GIT_TOKEN"));
        assert!(CREDENTIAL_HELPER.contains("username=x-access-token"));
        assert_eq!(TOKEN_VARIABLE, "RCMD_GIT_TOKEN");
    }

    #[tokio::test]
    async fn without_a_token_git_runs_with_no_credential_helper_at_all() {
        // A helper configured with an empty token would answer every prompt
        // with a blank password rather than failing honestly.
        let dir = tempfile::tempdir().unwrap();
        let repo = ConfigRepo::new(dir.path(), "main", None);
        assert!(repo.github_token.is_none());
    }

    #[test]
    fn a_remote_reduces_to_a_slug_whichever_way_it_is_written() {
        assert_eq!(
            github_slug("git@github.com:redcraft-org/rcmd_config.git").as_deref(),
            Some("redcraft-org/rcmd_config")
        );
        assert_eq!(
            github_slug("https://github.com/redcraft-org/rcmd_config").as_deref(),
            Some("redcraft-org/rcmd_config")
        );
        assert_eq!(github_slug("git@gitlab.com:someone/thing.git"), None);
    }

    #[test]
    fn the_body_says_what_merging_does_and_does_not_do() {
        let body = proposal_body("CMI", "9.7.*", "9.8.*", "9.8.1");
        assert!(body.contains("9.7.*"));
        assert!(body.contains("9.8.*"));
        assert!(body.contains("Nothing is deployed by merging this"));
    }

    #[tokio::test]
    async fn a_directory_that_is_not_a_checkout_says_so_rather_than_guessing() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ConfigRepo::new(dir.path(), "main", None);
        assert!(!repo.is_checkout());
        assert!(matches!(repo.sync().await, Err(GitError::NotACheckout(_))));
    }

    #[tokio::test]
    async fn a_pin_bump_becomes_one_commit_on_its_own_branch() {
        let Some(remote) = fake_remote() else {
            // No git on this machine. Not a reason to fail the suite.
            return;
        };
        let repo = ConfigRepo::new(remote.checkout, "main", None);
        let mut edits = BTreeMap::new();
        edits.insert(
            PathBuf::from("templates/hub/template.yml"),
            "artifacts:\n  CMI: '9.8.*'\n".to_string(),
        );
        let proposal = repo
            .propose_pin_bump("CMI", "9.8.1", &edits, "body")
            .await
            .unwrap();
        assert!(proposal.pushed);
        assert_eq!(proposal.branch, "update/CMI-9.8.1");
        assert_eq!(proposal.changed_files, vec!["templates/hub/template.yml"]);

        // Running it again with the same contents must not force push over
        // a branch somebody may already be reviewing.
        let again = repo
            .propose_pin_bump("CMI", "9.8.1", &edits, "body")
            .await
            .unwrap();
        assert!(!again.pushed);

        // A newer version is a different branch, not a rewrite of this one.
        let mut newer = BTreeMap::new();
        newer.insert(
            PathBuf::from("templates/hub/template.yml"),
            "artifacts:\n  CMI: '9.9.*'\n".to_string(),
        );
        let next = repo
            .propose_pin_bump("CMI", "9.9.0", &newer, "body")
            .await
            .unwrap();
        assert!(next.pushed);
        assert_eq!(next.branch, "update/CMI-9.9.0");
    }

    struct FakeRemote {
        checkout: PathBuf,
        _dir: tempfile::TempDir,
    }

    /// A bare repository with one commit, and a checkout of it. Enough to
    /// exercise fetch, branch, commit and push without a network.
    fn fake_remote() -> Option<FakeRemote> {
        let dir = tempfile::tempdir().ok()?;
        let bare = dir.path().join("origin.git");
        let checkout = dir.path().join("checkout");
        let run = |args: &[&str], cwd: &Path| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
                .output()
                .ok()
                .filter(|out| out.status.success())
        };
        std::fs::create_dir_all(&bare).ok()?;
        run(&["init", "--bare", "--initial-branch=main", "."], &bare)?;
        std::fs::create_dir_all(&checkout).ok()?;
        run(&["init", "--initial-branch=main", "."], &checkout)?;
        run(&["remote", "add", "origin", bare.to_str()?], &checkout)?;
        std::fs::create_dir_all(checkout.join("templates/hub")).ok()?;
        std::fs::write(
            checkout.join("templates/hub/template.yml"),
            "artifacts:\n  CMI: '9.7.*'\n",
        )
        .ok()?;
        run(&["add", "-A"], &checkout)?;
        run(&["commit", "-m", "first"], &checkout)?;
        run(&["push", "-u", "origin", "main"], &checkout)?;
        Some(FakeRemote {
            checkout,
            _dir: dir,
        })
    }
}
