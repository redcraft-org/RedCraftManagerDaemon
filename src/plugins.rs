//! Plugins: the things rcmd learns about your setup from somewhere else.
//!
//! rcmd knows how to run servers. It does not know who is playing on them,
//! and it should not have to: that answer comes from a proxy, or a plugin
//! on the proxy, or a web endpoint somebody wrote, and every network does
//! it differently.
//!
//! So a plugin is a small named thing that answers one kind of question.
//! They are declared in the config and built in to the binary rather than
//! loaded at runtime, because a single static binary you can copy onto a
//! box is worth more than the ability to drop shared objects next to it.
//! Adding a new one is a new variant here, not a new deployment step for
//! whoever runs this.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("{plugin}: {message}")]
    Unavailable { plugin: String, message: String },
}

/// One person, as whatever is reporting them describes them.
#[derive(Debug, Clone, Serialize)]
pub struct Player {
    pub name: String,
    /// The rank in front of the display name, when there is one. Read out
    /// of a display name like `&cAdmin&f>&4lululombard`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rank: Option<String>,
}

/// Who is connected, per server. A server with nobody on it is still in the
/// map with an empty list, so "nobody is on hub" and "nothing knows about
/// hub" stay different answers.
pub type Roster = BTreeMap<String, Vec<Player>>;

/// A plugin that can say who is playing.
#[async_trait]
pub trait PlayerSource: Send + Sync {
    fn name(&self) -> &str;
    async fn roster(&self) -> Result<Roster, PluginError>;
}

/// How a plugin is asked for in the config.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginConfig {
    /// RedCraftChat, which publishes a players.json on the proxy.
    Rcc { url: String },
    /// Anything that serves the same shape: `{"players": {server: [...]}}`,
    /// or a bare `{server: [...]}`.
    JsonRoster { url: String },
}

/// Everything the config asked for, ready to be used.
#[derive(Default)]
pub struct Plugins {
    pub players: Option<Box<dyn PlayerSource>>,
}

impl Plugins {
    pub fn load(configs: &[PluginConfig]) -> Self {
        let mut plugins = Plugins::default();
        for config in configs {
            match config {
                PluginConfig::Rcc { url } => {
                    tracing::info!("plugin rcc reads players from {url}");
                    plugins.players = Some(Box::new(JsonRoster::new("rcc", url)));
                }
                PluginConfig::JsonRoster { url } => {
                    tracing::info!("plugin json_roster reads players from {url}");
                    plugins.players = Some(Box::new(JsonRoster::new("json_roster", url)));
                }
            }
        }
        plugins
    }

    /// Who is on, or None when nothing was configured to answer.
    ///
    /// The distinction matters upstream: nobody configured is not the same
    /// as configured and not answering, and only the second one is a reason
    /// to refuse a restart.
    pub async fn roster(&self) -> Option<Result<Roster, PluginError>> {
        let source = self.players.as_ref()?;
        Some(source.roster().await)
    }

    /// Which plugin answers the player question, for the API to report.
    pub fn player_source_name(&self) -> Option<&str> {
        self.players.as_ref().map(|source| source.name())
    }
}

/// Reads a roster from a URL that serves JSON.
///
/// RedCraftChat's endpoint and the proxy's own both answer with this shape,
/// so one implementation covers both and the plugin name is only there to
/// say which one is configured when something goes wrong.
pub struct JsonRoster {
    name: &'static str,
    url: String,
    client: reqwest::Client,
}

impl JsonRoster {
    pub fn new(name: &'static str, url: &str) -> Self {
        Self {
            name,
            url: url.to_string(),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl PlayerSource for JsonRoster {
    fn name(&self) -> &str {
        self.name
    }

    async fn roster(&self) -> Result<Roster, PluginError> {
        let fail = |message: String| PluginError::Unavailable {
            plugin: self.name.to_string(),
            message,
        };
        let response = self
            .client
            .get(&self.url)
            // Short on purpose. This is asked before a restart, and a
            // request that hangs for thirty seconds has already failed at
            // the job of telling you quickly whether anybody is on.
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| fail(e.to_string()))?;
        if !response.status().is_success() {
            return Err(fail(format!("answered {}", response.status())));
        }
        let body: serde_json::Value = response.json().await.map_err(|e| fail(e.to_string()))?;
        parse_roster(&body).ok_or_else(|| fail("could not make sense of that JSON".into()))
    }
}

/// Pulls a roster out of whatever shape the endpoint used.
pub fn parse_roster(body: &serde_json::Value) -> Option<Roster> {
    let servers = body.get("players").unwrap_or(body).as_object()?;
    let mut roster = Roster::new();
    for (server, players) in servers {
        let list = players.as_array()?;
        roster.insert(server.clone(), list.iter().map(parse_player).collect());
    }
    Some(roster)
}

fn parse_player(value: &serde_json::Value) -> Player {
    // A plain string is a name. An object carries more.
    if let Some(name) = value.as_str() {
        return Player {
            name: name.to_string(),
            rank: None,
        };
    }
    let name = value
        .get("name")
        .or_else(|| value.get("username"))
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let rank = value
        .get("displayName")
        .and_then(|v| v.as_str())
        .and_then(rank_from_display_name);
    Player { name, rank }
}

/// `&cAdmin&f>&4lululombard` gives `Admin`.
///
/// Colour codes come out first, then everything before the `>`. A name with
/// no prefix has no rank rather than an empty one.
fn rank_from_display_name(display: &str) -> Option<String> {
    let mut plain = String::with_capacity(display.len());
    let mut chars = display.chars();
    while let Some(c) = chars.next() {
        if c == '&' || c == '\u{a7}' {
            // Drop the code that follows the marker.
            chars.next();
        } else {
            plain.push(c);
        }
    }
    let (rank, _) = plain.split_once('>')?;
    let rank = rank.trim();
    (!rank.is_empty()).then(|| rank.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_shape_redcraft_serves_parses() {
        let body = json!({
            "players": {
                "hub": [{"name": "lululombard", "displayName": "&cAdmin&f>&4lululombard"}],
                "museum": []
            }
        });
        let roster = parse_roster(&body).unwrap();
        assert_eq!(roster["hub"][0].name, "lululombard");
        assert_eq!(roster["hub"][0].rank.as_deref(), Some("Admin"));
        // An empty server is still in the map: "nobody on museum" and
        // "never heard of museum" are different answers.
        assert!(roster["museum"].is_empty());
        assert!(roster.contains_key("museum"));
    }

    #[test]
    fn a_bare_map_without_the_players_key_also_works() {
        let body = json!({ "hub": ["someone"] });
        let roster = parse_roster(&body).unwrap();
        assert_eq!(roster["hub"][0].name, "someone");
        assert_eq!(roster["hub"][0].rank, None);
    }

    #[test]
    fn a_name_with_no_prefix_has_no_rank_rather_than_an_empty_one() {
        let body = json!({"players": {"hub": [{"name": "someone", "displayName": "&7someone"}]}});
        let roster = parse_roster(&body).unwrap();
        assert_eq!(roster["hub"][0].rank, None);
    }

    #[test]
    fn section_signs_are_stripped_as_well_as_ampersands() {
        assert_eq!(
            rank_from_display_name("\u{a7}cMod\u{a7}f>\u{a7}4somebody").as_deref(),
            Some("Mod")
        );
    }

    #[test]
    fn nonsense_is_refused_rather_than_read_as_an_empty_server() {
        // The dangerous failure: an endpoint that changed shape parsing as
        // "nobody is online", which would wave every restart through.
        assert!(parse_roster(&json!("hello")).is_none());
        assert!(parse_roster(&json!({"players": {"hub": "not a list"}})).is_none());
        assert!(parse_roster(&json!([1, 2, 3])).is_none());
    }

    #[tokio::test]
    async fn no_plugin_configured_answers_nothing_at_all() {
        let plugins = Plugins::load(&[]);
        assert!(plugins.roster().await.is_none());
    }
}
