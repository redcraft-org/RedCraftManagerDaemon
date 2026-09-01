//! The rules that apply to everybody.
//!
//! Every mutation from every surface goes through `check`. That is the
//! point: the web UI, the API, MCP and the in-game panel cannot drift apart
//! on what is allowed, because there is only one place that decides.
//!
//! These rules were learned the expensive way over several years of running
//! the fleet. Each one exists because something went wrong once.

use serde::Serialize;

use crate::auth::{Actor, Role};

/// What somebody is trying to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Send a line to a server console.
    SendCommand {
        server: String,
    },
    Start {
        server: String,
    },
    Stop {
        server: String,
    },
    /// A restart that will drop whoever is connected.
    Restart {
        server: String,
    },
    /// Overwrite a server's files from a template.
    ApplyTemplate {
        server: String,
    },
    Backup {
        server: String,
    },
    /// Fetch artifacts and rebuild templates.
    RunPipeline,
    BuildTemplate {
        name: String,
    },
    /// Anything that pushes to a public repository.
    PushToGit {
        repository: String,
    },
    ManageTokens,
}

impl Action {
    /// The audit log name for this action.
    pub fn name(&self) -> &'static str {
        match self {
            Action::SendCommand { .. } => "server.command",
            Action::Start { .. } => "server.start",
            Action::Stop { .. } => "server.stop",
            Action::Restart { .. } => "server.restart",
            Action::ApplyTemplate { .. } => "server.apply_template",
            Action::Backup { .. } => "server.backup",
            Action::RunPipeline => "pipeline.run",
            Action::BuildTemplate { .. } => "template.build",
            Action::PushToGit { .. } => "git.push",
            Action::ManageTokens => "tokens.manage",
        }
    }

    /// What the action is being done to, for the audit log.
    pub fn target(&self) -> Option<String> {
        match self {
            Action::RunPipeline | Action::ManageTokens => None,
            Action::SendCommand { server }
            | Action::Start { server }
            | Action::Stop { server }
            | Action::Restart { server }
            | Action::ApplyTemplate { server }
            | Action::Backup { server } => Some(server.clone()),
            Action::BuildTemplate { name } => Some(name.clone()),
            Action::PushToGit { repository } => Some(repository.clone()),
        }
    }

    fn needs(&self) -> Role {
        match self {
            Action::ManageTokens => Role::Admin,
            _ => Role::Operator,
        }
    }

    fn disturbs_players(&self) -> bool {
        matches!(
            self,
            Action::Stop { .. } | Action::Restart { .. } | Action::ApplyTemplate { .. }
        )
    }
}

/// What the policy layer knows about the world when it decides.
#[derive(Debug, Clone, Default)]
pub struct Situation {
    /// Servers nobody may touch. RedCraft has one that is deliberately kept
    /// exactly as it is, and every tool that ever "helpfully" updated it
    /// cost somebody an evening.
    pub frozen_servers: Vec<String>,
    /// Servers that are not Minecraft. A template restart on the map
    /// renderer is not a thing that means anything.
    pub non_minecraft_servers: Vec<String>,
    /// How many players are connected.
    pub players_online: PlayerCount,
    /// Whether pushing to public repositories is allowed right now.
    pub push_window_open: bool,
}

/// What is known about who is connected.
///
/// The three states are not the same thing and collapsing them is how you
/// end up dropping a full server. `Unconfigured` means nobody asked us to
/// protect anybody, `Unavailable` means we were supposed to know and could
/// not find out, and only the second one is a reason to refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlayerCount {
    /// No source of player counts is set up.
    #[default]
    Unconfigured,
    Known(u32),
    /// A source is set up and it did not answer.
    Unavailable,
}

/// The answer, with the reason attached so every surface can show the same
/// sentence rather than inventing its own.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Verdict {
    pub allowed: bool,
    pub reason: String,
    /// True when the caller could proceed by saying so explicitly.
    pub overridable: bool,
}

impl Verdict {
    fn allow(reason: impl Into<String>) -> Self {
        Self {
            allowed: true,
            reason: reason.into(),
            overridable: false,
        }
    }

    fn deny(reason: impl Into<String>) -> Self {
        Self {
            allowed: false,
            reason: reason.into(),
            overridable: false,
        }
    }

    fn deny_overridable(reason: impl Into<String>) -> Self {
        Self {
            allowed: false,
            reason: reason.into(),
            overridable: true,
        }
    }
}

/// Decides. `force` is the caller saying they know and want it anyway, and
/// it only lifts the rules marked overridable.
pub fn check(actor: &Actor, action: &Action, situation: &Situation, force: bool) -> Verdict {
    if actor.role < action.needs() {
        return Verdict::deny(format!(
            "{} needs the {} role, and you have {}",
            action.name(),
            action.needs(),
            actor.role
        ));
    }

    if let Some(server) = action.target() {
        if action.disturbs_players() && situation.frozen_servers.contains(&server) {
            // Deliberately not overridable. A frozen server is frozen.
            return Verdict::deny(format!("{server} is frozen and nothing may restart it"));
        }
        if matches!(action, Action::ApplyTemplate { .. })
            && situation.non_minecraft_servers.contains(&server)
        {
            return Verdict::deny(format!(
                "{server} is not a Minecraft server, so it has no template"
            ));
        }
    }

    if action.disturbs_players() && !force {
        match situation.players_online {
            PlayerCount::Unconfigured | PlayerCount::Known(0) => {}
            PlayerCount::Known(count) => {
                return Verdict::deny_overridable(format!(
                    "{count} player(s) are connected and would be disconnected"
                ));
            }
            // Being unable to answer is exactly when a restart is most
            // likely to be a bad idea, so an unanswered question is a no.
            // Failing open here once dropped a full server mid event.
            PlayerCount::Unavailable => {
                return Verdict::deny_overridable(
                    "nobody could say how many players are connected, so this is being treated as busy"
                        .to_string(),
                );
            }
        }
    }

    if let Action::PushToGit { repository } = action {
        if !situation.push_window_open && actor.role < Role::Admin {
            return Verdict::deny(format!(
                "pushes to {repository} are outside the push window and you are not an admin"
            ));
        }
        if !situation.push_window_open {
            return Verdict::allow("outside the push window, allowed because you are an admin");
        }
    }

    Verdict::allow("allowed")
}

/// Whether the caller's own console line is safe to hand to a server.
///
/// A newline lets one request become two commands, which turns "say hello"
/// into "say hello\nop someone". The console takes one line at a time and
/// this is where that is enforced.
pub fn check_command(line: &str) -> Result<(), String> {
    if line.trim().is_empty() {
        return Err("an empty command does nothing".into());
    }
    if line.contains('\n') || line.contains('\r') {
        return Err("a command may not contain a line break".into());
    }
    if line.len() > 4096 {
        return Err("that command is too long for a console".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Surface;

    fn actor(role: Role) -> Actor {
        Actor {
            name: "tester".into(),
            role,
            surface: Surface::Api,
            player_uuid: None,
        }
    }

    fn empty_server() -> Situation {
        Situation {
            players_online: PlayerCount::Known(0),
            push_window_open: true,
            ..Default::default()
        }
    }

    #[test]
    fn a_viewer_may_look_but_not_touch() {
        let situation = empty_server();
        let restart = Action::Restart {
            server: "hub".into(),
        };
        assert!(!check(&actor(Role::Viewer), &restart, &situation, false).allowed);
        assert!(check(&actor(Role::Operator), &restart, &situation, false).allowed);
    }

    #[test]
    fn only_an_admin_hands_out_tokens() {
        let situation = empty_server();
        assert!(
            !check(
                &actor(Role::Operator),
                &Action::ManageTokens,
                &situation,
                false
            )
            .allowed
        );
        assert!(
            check(
                &actor(Role::Admin),
                &Action::ManageTokens,
                &situation,
                false
            )
            .allowed
        );
    }

    #[test]
    fn connected_players_stop_a_restart_until_you_insist() {
        let situation = Situation {
            players_online: PlayerCount::Known(3),
            ..empty_server()
        };
        let restart = Action::Restart {
            server: "survival".into(),
        };
        let verdict = check(&actor(Role::Operator), &restart, &situation, false);
        assert!(!verdict.allowed);
        assert!(verdict.overridable);
        assert!(verdict.reason.contains('3'));
        assert!(check(&actor(Role::Operator), &restart, &situation, true).allowed);
    }

    #[test]
    fn an_unknown_player_count_is_treated_as_busy() {
        // This is the whole point of the rule. An API that is down is not
        // an empty server, and guessing wrong drops everybody.
        let situation = Situation {
            players_online: PlayerCount::Unavailable,
            ..empty_server()
        };
        let restart = Action::Restart {
            server: "survival".into(),
        };
        let verdict = check(&actor(Role::Admin), &restart, &situation, false);
        assert!(!verdict.allowed);
        assert!(verdict.overridable);
    }

    #[test]
    fn with_no_player_source_at_all_nothing_gets_in_the_way() {
        // Somebody running one server on a box they own should not have to
        // pass force to every restart because they never wired up a count.
        let situation = Situation {
            players_online: PlayerCount::Unconfigured,
            push_window_open: true,
            ..Default::default()
        };
        let restart = Action::Restart {
            server: "hub".into(),
        };
        assert!(check(&actor(Role::Operator), &restart, &situation, false).allowed);
    }

    #[test]
    fn a_frozen_server_stays_frozen_even_when_you_insist() {
        let situation = Situation {
            frozen_servers: vec!["crea_engineer_test".into()],
            ..empty_server()
        };
        let restart = Action::Restart {
            server: "crea_engineer_test".into(),
        };
        let verdict = check(&actor(Role::Admin), &restart, &situation, true);
        assert!(!verdict.allowed);
        assert!(!verdict.overridable);
        // A backup is still fine, because it disturbs nobody.
        let backup = Action::Backup {
            server: "crea_engineer_test".into(),
        };
        assert!(check(&actor(Role::Admin), &backup, &situation, false).allowed);
    }

    #[test]
    fn a_process_that_is_not_minecraft_has_no_template() {
        let situation = Situation {
            non_minecraft_servers: vec!["bluemap".into()],
            ..empty_server()
        };
        let apply = Action::ApplyTemplate {
            server: "bluemap".into(),
        };
        assert!(!check(&actor(Role::Admin), &apply, &situation, true).allowed);
        // Starting and stopping it is perfectly normal.
        let start = Action::Start {
            server: "bluemap".into(),
        };
        assert!(check(&actor(Role::Operator), &start, &situation, false).allowed);
    }

    #[test]
    fn the_push_window_holds_for_everybody_but_an_admin() {
        let situation = Situation {
            push_window_open: false,
            ..empty_server()
        };
        let push = Action::PushToGit {
            repository: "redcraft-org/RedCraftManagerDaemon".into(),
        };
        assert!(!check(&actor(Role::Operator), &push, &situation, false).allowed);
        assert!(check(&actor(Role::Admin), &push, &situation, false).allowed);
        // Inside the window everybody with the role may push.
        assert!(check(&actor(Role::Operator), &push, &empty_server(), false).allowed);
    }

    #[test]
    fn a_console_line_is_one_line() {
        assert!(check_command("say hello").is_ok());
        assert!(check_command("say hello\nop someone").is_err());
        assert!(check_command("say hello\r\nop someone").is_err());
        assert!(check_command("   ").is_err());
        assert!(check_command(&"x".repeat(5000)).is_err());
    }
}
