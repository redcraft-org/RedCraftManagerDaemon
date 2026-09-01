//! Domain types shared across modules.

use serde::Serialize;

/// What rcmd believes about a server's process right now.
///
/// rcsm kept two booleans (`running`, `crashed`) and an unexported try
/// counter; the states they actually encoded are these five. `Crashed` keeps
/// whether the healthcheck gave up, because "down, will revive" and "down,
/// needs a human" are different answers to every question a caller asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ServerState {
    Stopped,
    Starting,
    Running,
    Stopping,
    Crashed { bootloop: bool },
}

impl ServerState {
    #[allow(dead_code)]
    pub fn is_running(self) -> bool {
        matches!(self, ServerState::Running | ServerState::Starting)
    }
}

/// A server as reported by the API.
#[derive(Debug, Clone, Serialize)]
pub struct ServerStatus {
    pub name: String,
    #[serde(flatten)]
    pub state: ServerState,
    /// Whether a tmux session actually exists, straight from tmux rather
    /// than from belief. The healthcheck exists because these two disagree.
    pub session_alive: bool,
    pub restart_tries: u32,
}
