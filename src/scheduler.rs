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

/// Starts the background timer. An empty expression disables it, which is
/// what you want when a CI job drives the pipeline instead.
pub fn spawn(state: Arc<AppState>, expression: &str) -> Option<tokio::task::JoinHandle<()>> {
    if expression.trim().is_empty() {
        tracing::info!("no pipeline schedule set, nothing runs on its own");
        return None;
    }
    let schedule = match Schedule::parse(expression) {
        Ok(schedule) => schedule,
        Err(e) => {
            // A typo in a cron line must not take the daemon down. Servers
            // keep running, the nightly refresh just does not happen, and
            // the log says so in as many words.
            tracing::error!(
                "pipeline_schedule {expression:?} is not valid: {e}. Nothing scheduled."
            );
            return None;
        }
    };
    tracing::info!("pipeline scheduled at {expression}");

    Some(tokio::spawn(async move {
        loop {
            sleep_to_next_minute().await;
            if !schedule.matches(chrono::Local::now()) {
                continue;
            }
            tracing::info!("scheduled pipeline run starting");
            // Nobody asked for this one, so the audit log says who did.
            let actor = crate::auth::Actor::system();
            crate::auth::audit(
                &state.database,
                &actor,
                "pipeline.run",
                None,
                "allowed",
                Some("on schedule"),
            )
            .await;
            match state.run_pipeline().await {
                Ok(report) => tracing::info!(
                    "pipeline finished: {} fetched, {} built, clean={}",
                    report.fetched.len(),
                    report.builds.len(),
                    report.clean
                ),
                Err(e) => tracing::warn!("scheduled pipeline did not run: {e}"),
            }
        }
    }))
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
    fn sunday_is_zero_the_way_a_crontab_writes_it() {
        // Getting this backwards would run the weekly job on the wrong day
        // for a year before anybody noticed.
        let sunday = Schedule::parse("0 4 * * 0").unwrap();
        // 2026-09-06 is a Sunday.
        assert!(sunday.matches(at(9, 6, 4, 0)));
        assert!(!sunday.matches(at(9, 7, 4, 0)));
    }
}
