//! The clock.
//!
//! The tooling this replaces ran its two halves as separate scheduled jobs
//! an hour apart, and "an hour" was the entire mechanism making sure the
//! second one saw the first one's output. When a download ran long the
//! templates got built against yesterday's jars, quietly. Here there is one
//! schedule that runs one pipeline, so the ordering is a function call
//! instead of a hope.
//!
//! The cron parser is deliberately small: five fields, and the syntax
//! people actually write in a crontab. Anything fancier belongs in a real
//! scheduler, and a config file that says `0 3 * * *` does not need one.

use std::sync::Arc;

use chrono::{Datelike, Timelike};

use crate::api::AppState;

#[derive(Debug, thiserror::Error)]
pub enum CronError {
    #[error("a cron expression needs 5 fields, got {0}")]
    FieldCount(usize),
    #[error("{field:?} is not a valid value for the {name} field")]
    Field { name: &'static str, field: String },
}

/// A parsed five field cron expression: minute, hour, day, month, weekday.
#[derive(Debug, Clone)]
pub struct Schedule {
    minute: Vec<u32>,
    hour: Vec<u32>,
    day: Vec<u32>,
    month: Vec<u32>,
    weekday: Vec<u32>,
}

impl Schedule {
    pub fn parse(expression: &str) -> Result<Self, CronError> {
        let fields: Vec<&str> = expression.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(CronError::FieldCount(fields.len()));
        }
        Ok(Self {
            minute: field(fields[0], 0, 59, "minute")?,
            hour: field(fields[1], 0, 23, "hour")?,
            day: field(fields[2], 1, 31, "day")?,
            month: field(fields[3], 1, 12, "month")?,
            weekday: field(fields[4], 0, 6, "weekday")?,
        })
    }

    /// Whether the expression fires at this minute.
    pub fn matches(&self, at: chrono::DateTime<chrono::Local>) -> bool {
        // Sunday is 0, the way every crontab writes it.
        let weekday = at.weekday().num_days_from_sunday();
        self.minute.contains(&at.minute())
            && self.hour.contains(&at.hour())
            && self.day.contains(&at.day())
            && self.month.contains(&at.month())
            && self.weekday.contains(&weekday)
    }
}

/// Expands one field into every value it matches.
fn field(spec: &str, min: u32, max: u32, name: &'static str) -> Result<Vec<u32>, CronError> {
    let invalid = || CronError::Field {
        name,
        field: spec.to_string(),
    };
    let mut out = Vec::new();
    for part in spec.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => (range, step.parse::<u32>().map_err(|_| invalid())?),
            None => (part, 1),
        };
        if step == 0 {
            return Err(invalid());
        }
        let (from, to) = if range == "*" {
            (min, max)
        } else if let Some((from, to)) = range.split_once('-') {
            (
                from.parse().map_err(|_| invalid())?,
                to.parse().map_err(|_| invalid())?,
            )
        } else {
            let value: u32 = range.parse().map_err(|_| invalid())?;
            (value, value)
        };
        if from < min || to > max || from > to {
            return Err(invalid());
        }
        out.extend((from..=to).step_by(step as usize));
    }
    out.sort_unstable();
    out.dedup();
    if out.is_empty() {
        return Err(invalid());
    }
    Ok(out)
}

/// What a scheduled job does when it fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    /// Fetch every artifact and rebuild what moved.
    Pipeline,
    /// Back up every server.
    Backup,
    /// Warn everybody, wait, and restart the fleet.
    Restart,
}

impl Job {
    fn audit_action(self) -> &'static str {
        match self {
            Job::Pipeline => "pipeline.run",
            Job::Backup => "server.backup",
            Job::Restart => "server.restart",
        }
    }

    fn setting(self) -> &'static str {
        match self {
            Job::Pipeline => "pipeline_schedule",
            Job::Backup => "backup_schedule",
            Job::Restart => "restart_schedule",
        }
    }
}

/// Starts every schedule the config asks for.
pub fn spawn_all(
    state: Arc<AppState>,
    config: &crate::config::Config,
) -> Vec<tokio::task::JoinHandle<()>> {
    [
        (Job::Pipeline, config.pipeline_schedule.as_str()),
        (Job::Backup, config.backup_schedule.as_str()),
        (Job::Restart, config.restart_schedule.as_str()),
    ]
    .into_iter()
    .filter_map(|(job, expression)| spawn(state.clone(), job, expression))
    .collect()
}

/// Starts one background timer. An empty expression disables it, which is
/// what you want when something else drives that job.
pub fn spawn(
    state: Arc<AppState>,
    job: Job,
    expression: &str,
) -> Option<tokio::task::JoinHandle<()>> {
    if expression.trim().is_empty() {
        tracing::info!("no {} set, nothing runs on its own", job.setting());
        return None;
    }
    let schedule = match Schedule::parse(expression) {
        Ok(schedule) => schedule,
        Err(e) => {
            // A typo in a cron line must not take the daemon down. Servers
            // keep running, that one job just does not happen, and the log
            // says so in as many words.
            tracing::error!(
                "{} {expression:?} is not valid: {e}. Nothing scheduled.",
                job.setting()
            );
            return None;
        }
    };
    // Schedules are matched in local time, and "local" on a server is
    // whatever TZ happens to be set to. Saying which one out loud at
    // startup is the cheapest way to catch the day somebody changes it and
    // the nightly restart quietly moves by four hours.
    tracing::info!(
        "{:?} scheduled at {expression} ({})",
        job,
        chrono::Local::now().format("local time is currently %H:%M %Z")
    );

    Some(tokio::spawn(async move {
        loop {
            sleep_to_next_minute().await;
            if !schedule.matches(chrono::Local::now()) {
                continue;
            }
            run(&state, job).await;
        }
    }))
}

async fn run(state: &Arc<AppState>, job: Job) {
    tracing::info!("scheduled {:?} starting", job);
    // Nobody asked for this one, so the audit log says who did.
    let actor = crate::auth::Actor::system();
    crate::auth::audit(
        &state.database,
        &actor,
        job.audit_action(),
        None,
        "allowed",
        Some("on schedule"),
    )
    .await;

    match job {
        Job::Pipeline => match state.run_pipeline().await {
            Ok(report) => tracing::info!(
                "pipeline finished: {} fetched, {} built, clean={}",
                report.fetched.len(),
                report.builds.len(),
                report.clean
            ),
            Err(e) => tracing::warn!("scheduled pipeline did not run: {e}"),
        },
        Job::Backup => {
            state.manager.backup_all().await;
        }
        Job::Restart => {
            // Frozen servers are frozen on a schedule too. This is the one
            // place a fleet wide job could quietly undo the whole point of
            // marking a server as hands off.
            state
                .manager
                .warn_and_restart_all(&state.frozen_servers)
                .await;
        }
    }
}

/// Waits until the top of the next minute, so a run happens once per
/// matching minute rather than sixty times.
async fn sleep_to_next_minute() {
    let now = chrono::Local::now();
    let seconds = 60 - now.second() as u64;
    tokio::time::sleep(std::time::Duration::from_secs(seconds.max(1))).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(month: u32, day: u32, hour: u32, minute: u32) -> chrono::DateTime<chrono::Local> {
        chrono::Local
            .with_ymd_and_hms(2026, month, day, hour, minute, 0)
            .unwrap()
    }

    #[test]
    fn the_nightly_default_fires_once_a_day() {
        let schedule = Schedule::parse("0 3 * * *").unwrap();
        assert!(schedule.matches(at(9, 1, 3, 0)));
        assert!(!schedule.matches(at(9, 1, 3, 1)));
        assert!(!schedule.matches(at(9, 1, 4, 0)));
    }

    #[test]
    fn lists_ranges_and_steps_all_work() {
        let schedule = Schedule::parse("0,30 */6 * * *").unwrap();
        assert!(schedule.matches(at(9, 1, 0, 0)));
        assert!(schedule.matches(at(9, 1, 6, 30)));
        assert!(schedule.matches(at(9, 1, 18, 0)));
        assert!(!schedule.matches(at(9, 1, 7, 0)));
        assert!(!schedule.matches(at(9, 1, 6, 15)));

        let weekdays = Schedule::parse("45 5 * * 1-5").unwrap();
        // 2026-09-01 is a Tuesday, 2026-09-05 a Saturday.
        assert!(weekdays.matches(at(9, 1, 5, 45)));
        assert!(!weekdays.matches(at(9, 5, 5, 45)));
    }

    #[test]
    fn a_broken_expression_is_an_error_and_not_a_panic() {
        assert!(Schedule::parse("0 3 * *").is_err());
        assert!(Schedule::parse("0 3 * * * *").is_err());
        assert!(Schedule::parse("60 3 * * *").is_err());
        assert!(Schedule::parse("0 25 * * *").is_err());
        assert!(Schedule::parse("0 3 * * mon").is_err());
        assert!(Schedule::parse("*/0 3 * * *").is_err());
        assert!(Schedule::parse("5-1 3 * * *").is_err());
    }

    #[test]
    fn the_schedules_redcraft_is_moving_off_cron_still_mean_the_same_thing() {
        // These two came out of k8s CronJobs that published to Redis. If
        // they drift, either the backups stop or the fleet restarts at the
        // wrong hour, and both are the kind of thing you notice late.
        let backup = Schedule::parse("15 */6 * * *").unwrap();
        for hour in [0, 6, 12, 18] {
            assert!(backup.matches(at(9, 1, hour, 15)), "missed {hour}:15");
        }
        assert!(!backup.matches(at(9, 1, 3, 15)));
        assert!(!backup.matches(at(9, 1, 6, 0)));

        let restart = Schedule::parse("45 5 * * *").unwrap();
        assert!(restart.matches(at(9, 1, 5, 45)));
        assert!(!restart.matches(at(9, 1, 6, 45)));
        assert!(!restart.matches(at(9, 1, 5, 46)));
    }

    #[test]
    fn sunday_is_zero_the_way_a_crontab_writes_it() {
        // Getting this backwards would run the weekly job on the wrong day
        // for a year before anybody noticed.
        let sunday = Schedule::parse("0 4 * * 0").unwrap();
        // 2026-09-06 is a Sunday.
        assert!(sunday.matches(at(9, 6, 4, 0)));
        assert!(!sunday.matches(at(9, 7, 4, 0)));
    }
}
