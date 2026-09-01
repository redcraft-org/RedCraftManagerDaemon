//! Telling somebody what happened.
//!
//! One webhook, fire and forget. A notification that fails must never fail
//! the thing it was reporting on: a Discord outage is not a reason for a
//! plugin update to be rolled back.

use serde_json::json;

use crate::pipeline::PipelineReport;

/// How bad a thing is. Same four the old daemon used, so the Discord
/// channel this posts into keeps looking exactly the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Severe,
    Fatal,
}

impl Level {
    fn name(self) -> &'static str {
        match self {
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Severe => "SEVERE",
            Level::Fatal => "FATAL",
        }
    }

    /// The embed colours the old daemon used, kept to the number so the
    /// channel does not suddenly change palette mid cutover.
    fn colour(self) -> u32 {
        match self {
            Level::Info => 1_499_250,
            Level::Warn => 14_992_650,
            Level::Severe => 16_144_655,
            Level::Fatal => 16_141_655,
        }
    }
}

pub struct Notifier {
    webhook_url: Option<String>,
    instance: String,
    client: reqwest::Client,
}

impl Notifier {
    pub fn named(webhook_url: Option<String>, instance: String) -> Self {
        Self {
            webhook_url,
            instance,
            client: reqwest::Client::new(),
        }
    }

    /// Reports one thing that happened to one server.
    ///
    /// This is the alert that matters: a server crashing at 4am is worth
    /// waking up for, and the daemon this replaces sent exactly these. It
    /// would have been easy to drop them in the rewrite and not notice
    /// until something crashed quietly.
    pub async fn event(&self, level: Level, service: &str, message: &str) {
        let Some(url) = self.webhook_url.as_deref() else {
            return;
        };
        let payload = json!({
            "content": "New event",
            "embeds": [{
                "color": level.colour(),
                "fields": [
                    { "name": "Level", "value": level.name(), "inline": true },
                    { "name": "Instance", "value": self.instance, "inline": true },
                    { "name": "Server/Service", "value": service, "inline": true },
                    { "name": "Message", "value": message, "inline": false },
                ],
            }],
        });
        let result = self
            .client
            .post(url)
            .timeout(std::time::Duration::from_secs(10))
            .json(&payload)
            .send()
            .await;
        match result {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => tracing::warn!("the webhook answered {}", response.status()),
            Err(e) => tracing::warn!("could not reach the webhook: {e}"),
        }
    }

    pub fn is_configured(&self) -> bool {
        self.webhook_url.is_some()
    }

    /// Posts a message. Silent when no webhook is set, which is the default.
    pub async fn send(&self, content: &str) {
        let Some(url) = self.webhook_url.as_deref() else {
            return;
        };
        // Discord takes 2000 characters and drops the whole message if you
        // send more, so a long report gets trimmed rather than lost.
        let content = if content.chars().count() > 1900 {
            let kept: String = content.chars().take(1900).collect();
            format!("{kept}\n... (trimmed)")
        } else {
            content.to_string()
        };
        let result = self
            .client
            .post(url)
            .timeout(std::time::Duration::from_secs(10))
            .json(&json!({ "content": content }))
            .send()
            .await;
        match result {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => tracing::warn!("the webhook answered {}", response.status()),
            Err(e) => tracing::warn!("could not reach the webhook: {e}"),
        }
    }

    pub async fn pipeline_finished(&self, report: &PipelineReport) {
        if !self.is_configured() {
            return;
        }
        self.send(&summarize(report)).await;
    }
}

/// A pipeline run in a few lines somebody will actually read.
pub fn summarize(report: &PipelineReport) -> String {
    let updated: Vec<&str> = report
        .fetched
        .iter()
        .filter(|outcome| outcome.action == "stored")
        .map(|outcome| outcome.name.as_str())
        .collect();
    let failed: Vec<&str> = report
        .fetched
        .iter()
        .filter(|outcome| !outcome.succeeded)
        .map(|outcome| outcome.name.as_str())
        .collect();
    let built: Vec<&str> = report
        .builds
        .iter()
        .filter(|build| build.succeeded)
        .map(|build| build.template.as_str())
        .collect();
    let broken: Vec<&str> = report
        .builds
        .iter()
        .filter(|build| !build.succeeded)
        .map(|build| build.template.as_str())
        .collect();

    let headline = if report.clean {
        "Pipeline finished"
    } else {
        "Pipeline finished with problems"
    };
    let mut lines = vec![format!("**{headline}**")];
    if updated.is_empty() {
        lines.push("Nothing was out of date.".to_string());
    } else {
        lines.push(format!("Updated {}: {}", updated.len(), updated.join(", ")));
    }
    if !built.is_empty() {
        lines.push(format!("Rebuilt {}: {}", built.len(), built.join(", ")));
    }
    if !failed.is_empty() {
        lines.push(format!("Could not fetch: {}", failed.join(", ")));
    }
    if !broken.is_empty() {
        lines.push(format!("Could not build: {}", broken.join(", ")));
    }
    let proposed: Vec<&str> = report
        .proposals
        .iter()
        .filter(|proposal| proposal.pushed)
        .map(|proposal| proposal.title.as_str())
        .collect();
    if !proposed.is_empty() {
        lines.push(format!("Waiting for review: {}", proposed.join(", ")));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::FetchOutcome;
    use crate::pipeline::BuildOutcome;

    fn fetched(name: &str, action: &'static str, succeeded: bool) -> FetchOutcome {
        FetchOutcome {
            name: name.into(),
            succeeded,
            action,
            version: Some("1.0".into()),
            sha256: None,
            error: (!succeeded).then(|| "404".to_string()),
        }
    }

    fn built(template: &str, succeeded: bool) -> BuildOutcome {
        BuildOutcome {
            template: template.into(),
            succeeded,
            tar: None,
            tar_sha256: None,
            missing_secrets: vec![],
            error: None,
            artifacts: vec![],
        }
    }

    fn report(fetched: Vec<FetchOutcome>, builds: Vec<BuildOutcome>) -> PipelineReport {
        let clean = fetched.iter().all(|f| f.succeeded) && builds.iter().all(|b| b.succeeded);
        PipelineReport {
            started_at: chrono::Utc::now(),
            finished_at: chrono::Utc::now(),
            fetched,
            builds,
            proposals: vec![],
            clean,
        }
    }

    #[test]
    fn a_quiet_night_says_so_in_one_line() {
        let summary = summarize(&report(vec![fetched("CMI", "unchanged", true)], vec![]));
        assert!(summary.contains("Nothing was out of date"));
        assert!(!summary.contains("problems"));
    }

    #[test]
    fn a_run_with_one_broken_source_still_reports_what_worked() {
        // The whole point of not stopping on the first failure is that the
        // other fifty nine plugins still update, so the summary has to show
        // both halves.
        let summary = summarize(&report(
            vec![
                fetched("CMI", "stored", true),
                fetched("LuckPerms", "failed", false),
            ],
            vec![built("hub", true)],
        ));
        assert!(summary.contains("problems"));
        assert!(summary.contains("Updated 1: CMI"));
        assert!(summary.contains("Could not fetch: LuckPerms"));
        assert!(summary.contains("Rebuilt 1: hub"));
    }

    #[tokio::test]
    async fn no_webhook_means_no_request_and_no_error() {
        let notifier = Notifier::named(None, "test".into());
        assert!(!notifier.is_configured());
        // Would panic on an unwrap if this tried to send anywhere.
        notifier.send("anything").await;
        notifier.event(Level::Severe, "hub", "crashed").await;
    }

    #[test]
    fn the_embed_colours_match_the_daemon_this_replaces() {
        // Same numbers, so the Discord channel does not visibly change the
        // day of the swap.
        assert_eq!(Level::Info.colour(), 1_499_250);
        assert_eq!(Level::Warn.colour(), 14_992_650);
        assert_eq!(Level::Severe.colour(), 16_144_655);
        assert_eq!(Level::Fatal.colour(), 16_141_655);
    }
}
