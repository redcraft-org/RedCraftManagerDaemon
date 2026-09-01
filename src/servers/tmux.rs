//! The tmux layer, ported from rcsm/tmux.go with its semantics kept.
//!
//! Detached tmux sessions are the reason rcmd can restart without touching a
//! single running server, and the reason the cutover from rcsm is
//! zero-downtime: the daemon adopts whatever sessions already exist. The
//! session prefix therefore stays configurable and defaults to rcsm's.
//!
//! Two deliberate ports and one fix:
//! - Commands are typed with `send-keys -l` (literal) followed by a separate
//!   Enter, which is what makes arbitrary UTF-8 safe against key-name
//!   interpretation (tmux.go did the same).
//! - Session existence is polled, because tmux offers no wait primitive.
//! - Targets are exact, never prefix-matched. Verified against tmux 3.6: with
//!   only `rcmdprefix_alpha_long` alive, `-t rcmdprefix_alpha` resolves to it,
//!   so a plain name means `send-keys -t rcsm_crea` would type into
//!   `rcsm_crea_build_plot`. rcsm lived with that; an API that takes server
//!   names from callers must not. The `=` prefix disables the fuzzy match.
//!   Session verbs (has-session, kill-session) take `=name`; pane verbs
//!   (send-keys, capture-pane) need the trailing colon, `=name:`, because
//!   without it tmux parses the whole string as a pane name and fails.

use std::path::Path;
use std::time::Duration;

use tokio::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum TmuxError {
    #[error("tmux could not be invoked: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("server crashed on start (session never appeared within {0:?})")]
    NeverAppeared(Duration),
    #[error("tmux {verb} failed: {stderr}")]
    Failed { verb: &'static str, stderr: String },
}

/// How long SessionCreate waited in rcsm for the session to appear.
const CREATE_WAIT: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct Tmux {
    prefix: String,
}

impl Tmux {
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
        }
    }

    pub fn session_name(&self, server: &str) -> String {
        format!("{}{}", self.prefix, server)
    }

    /// Exact session target, for verbs that address a session.
    fn session_target(&self, server: &str) -> String {
        format!("={}", self.session_name(server))
    }

    /// Exact pane target, for verbs that address a pane. The trailing colon
    /// selects the session's active window/pane; without it tmux reads the
    /// whole string as a pane name and reports "can't find pane".
    fn pane_target(&self, server: &str) -> String {
        format!("={}:", self.session_name(server))
    }

    pub async fn session_exists(&self, server: &str) -> Result<bool, TmuxError> {
        let status = Command::new("tmux")
            .args(["has-session", "-t", &self.session_target(server)])
            .output()
            .await?;
        Ok(status.status.success())
    }

    /// `tmux new -d -s <name> <start command>`, cwd = the server directory,
    /// then wait for the session to exist. The start command is split on
    /// spaces exactly as rcsm split it: the commands in rcsm_config.json were
    /// written for that splitting, and quoting rules would change their
    /// meaning.
    pub async fn session_create(
        &self,
        server: &str,
        server_dir: &Path,
        start_command: &str,
    ) -> Result<(), TmuxError> {
        let mut args = vec!["new", "-d", "-s"];
        let name = self.session_name(server);
        args.push(&name);
        let split: Vec<&str> = start_command.split(' ').filter(|s| !s.is_empty()).collect();
        args.extend(split);

        let out = Command::new("tmux")
            .args(&args)
            .current_dir(server_dir)
            .output()
            .await?;
        if !out.status.success() {
            return Err(TmuxError::Failed {
                verb: "new-session",
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }

        if self.wait_for_state(server, true, CREATE_WAIT).await? {
            Ok(())
        } else {
            Err(TmuxError::NeverAppeared(CREATE_WAIT))
        }
    }

    /// Types a command into the session: literal keys, then Enter.
    pub async fn session_run_command(&self, server: &str, command: &str) -> Result<(), TmuxError> {
        let target = self.pane_target(server);
        let out = Command::new("tmux")
            .args(["send-keys", "-t", &target, "-l", command])
            .output()
            .await?;
        if !out.status.success() {
            return Err(TmuxError::Failed {
                verb: "send-keys",
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        let out = Command::new("tmux")
            .args(["send-keys", "-t", &target, "Enter"])
            .output()
            .await?;
        if !out.status.success() {
            return Err(TmuxError::Failed {
                verb: "send-keys Enter",
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(())
    }

    /// Clean stop: type the stop command, wait for the session to disappear,
    /// kill it if the timeout passes. Returns whether the kill was needed,
    /// because the caller wants to log rsm's "Timeout shutdown reached,
    /// killing the server" distinctly from a clean exit.
    pub async fn session_terminate(
        &self,
        server: &str,
        stop_command: &str,
        timeout: Duration,
    ) -> Result<bool, TmuxError> {
        self.session_run_command(server, stop_command).await?;
        if self.wait_for_state(server, false, timeout).await? {
            return Ok(false);
        }
        let out = Command::new("tmux")
            .args(["kill-session", "-t", &self.session_target(server)])
            .output()
            .await?;
        if !out.status.success() {
            // The session may have died between the poll and the kill; a
            // missing session is the outcome we wanted.
            if self.session_exists(server).await? {
                return Err(TmuxError::Failed {
                    verb: "kill-session",
                    stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
                });
            }
        }
        Ok(true)
    }

    /// Snapshot of the pane's scrollback, the same call the old MCP made
    /// over SSH: `capture-pane -p -J -S -<lines>`.
    pub async fn capture_pane(&self, server: &str, lines: u32) -> Result<String, TmuxError> {
        let start = format!("-{lines}");
        let out = Command::new("tmux")
            .args([
                "capture-pane",
                "-t",
                &self.pane_target(server),
                "-p",
                "-J",
                "-S",
                &start,
            ])
            .output()
            .await?;
        if !out.status.success() {
            return Err(TmuxError::Failed {
                verb: "capture-pane",
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Every session with our prefix, mapped back to server names. This is
    /// what session adoption at boot reads.
    #[allow(dead_code)] // used by the cutover session-adoption path (M9)
    pub async fn list_sessions(&self) -> Result<Vec<String>, TmuxError> {
        let out = Command::new("tmux")
            .args(["ls", "-F", "#{session_name}"])
            .output()
            .await?;
        // `tmux ls` exits non-zero when no server is running at all, which
        // simply means no sessions.
        if !out.status.success() {
            return Ok(vec![]);
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|s| s.strip_prefix(&self.prefix))
            .map(str::to_string)
            .collect())
    }

    /// Polls until the session's existence matches `wanted` or the timeout
    /// passes. Returns whether the wanted state was reached.
    pub async fn wait_for_state(
        &self,
        server: &str,
        wanted: bool,
        timeout: Duration,
    ) -> Result<bool, TmuxError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.session_exists(server).await? == wanted {
                return Ok(true);
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}
