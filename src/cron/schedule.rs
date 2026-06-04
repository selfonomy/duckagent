use super::types::{CronJob, CronJobState, CronMissedRunPolicy, CronSchedule, CronWeekday};
use anyhow::{Context, Result, bail};
use chrono::{
    DateTime, Datelike, Duration as ChronoDuration, FixedOffset, Local, LocalResult, NaiveDate,
    NaiveDateTime, NaiveTime, TimeZone, Utc, Weekday,
};

pub fn compute_next_run_at(
    job: &CronJob,
    state: &CronJobState,
    now: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>> {
    if !job.enabled {
        return Ok(None);
    }
    let created_at = parse_rfc3339_utc(&job.created_at)?;
    match &job.schedule {
        CronSchedule::Once { at } => {
            if state.last_scheduled_for.is_some() {
                return Ok(None);
            }
            let at = parse_rfc3339_utc(at)?;
            if at < created_at {
                return Ok(None);
            }
            if at <= now && job.policy.missed_run == CronMissedRunPolicy::Skip {
                return Ok(None);
            }
            Ok(Some(at))
        }
        CronSchedule::Interval {
            every_seconds,
            anchor,
        } => compute_interval_next(
            *every_seconds,
            anchor.as_deref(),
            state,
            now,
            created_at,
            job.policy.missed_run,
        ),
        CronSchedule::Daily { time, timezone } => compute_daily_next(
            time,
            timezone.as_deref(),
            state.last_scheduled_for.as_deref(),
            now,
            created_at,
            job.policy.missed_run,
        ),
        CronSchedule::Weekly {
            weekdays,
            time,
            timezone,
        } => compute_weekly_next(
            weekdays,
            time,
            timezone.as_deref(),
            state.last_scheduled_for.as_deref(),
            now,
            created_at,
            job.policy.missed_run,
        ),
    }
}

pub fn parse_rfc3339_utc(raw: &str) -> Result<DateTime<Utc>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("timestamp must be non-empty");
    }
    Ok(DateTime::parse_from_rfc3339(trimmed)
        .with_context(|| format!("invalid RFC3339 timestamp: {trimmed}"))?
        .with_timezone(&Utc))
}

fn compute_interval_next(
    every_seconds: u64,
    anchor: Option<&str>,
    state: &CronJobState,
    now: DateTime<Utc>,
    created_at: DateTime<Utc>,
    missed_run: CronMissedRunPolicy,
) -> Result<Option<DateTime<Utc>>> {
    if every_seconds == 0 {
        bail!("interval schedule every_seconds must be greater than zero");
    }
    let every = ChronoDuration::seconds(every_seconds as i64);
    if let Some(last) = state.last_scheduled_for.as_deref() {
        let last = parse_rfc3339_utc(last)?;
        return Ok(Some(last + every));
    }
    let mut anchor = match anchor {
        Some(anchor) => parse_rfc3339_utc(anchor)?,
        None => now + every,
    };
    if anchor < created_at {
        let elapsed = created_at
            .signed_duration_since(anchor)
            .num_seconds()
            .max(0);
        let mut steps = elapsed / every_seconds as i64;
        if elapsed % every_seconds as i64 != 0 {
            steps += 1;
        }
        anchor += ChronoDuration::seconds(steps * every_seconds as i64);
    }
    if anchor > now {
        return Ok(Some(anchor));
    }
    let elapsed = now.signed_duration_since(anchor).num_seconds().max(0);
    let steps = elapsed / every_seconds as i64;
    let latest_due = anchor + ChronoDuration::seconds(steps * every_seconds as i64);
    if latest_due == now || missed_run == CronMissedRunPolicy::RunOnce {
        return Ok(Some(latest_due));
    }
    Ok(Some(latest_due + every))
}

fn compute_daily_next(
    time: &str,
    timezone: Option<&str>,
    last_scheduled_for: Option<&str>,
    now: DateTime<Utc>,
    created_at: DateTime<Utc>,
    missed_run: CronMissedRunPolicy,
) -> Result<Option<DateTime<Utc>>> {
    let local_time = parse_time(time)?;
    let tz = TimezoneSpec::parse(timezone)?;
    let today = tz.local_date(now);
    let mut best = None;
    for offset_days in -1..=370 {
        let date = today + ChronoDuration::days(offset_days);
        let candidate = tz.local_to_utc(date, local_time)?;
        if !is_after_last(candidate, last_scheduled_for)? {
            continue;
        }
        if last_scheduled_for.is_none() && candidate < created_at {
            continue;
        }
        if candidate < now {
            if missed_run == CronMissedRunPolicy::RunOnce {
                best = Some(candidate);
            }
            continue;
        }
        return Ok(best.or(Some(candidate)));
    }
    Ok(best)
}

fn compute_weekly_next(
    weekdays: &[CronWeekday],
    time: &str,
    timezone: Option<&str>,
    last_scheduled_for: Option<&str>,
    now: DateTime<Utc>,
    created_at: DateTime<Utc>,
    missed_run: CronMissedRunPolicy,
) -> Result<Option<DateTime<Utc>>> {
    if weekdays.is_empty() {
        bail!("weekly schedule requires at least one weekday");
    }
    let local_time = parse_time(time)?;
    let tz = TimezoneSpec::parse(timezone)?;
    let today = tz.local_date(now);
    let mut best = None;
    for offset_days in -7..=370 {
        let date = today + ChronoDuration::days(offset_days);
        if !weekdays
            .iter()
            .any(|weekday| weekday.to_chrono() == date.weekday())
        {
            continue;
        }
        let candidate = tz.local_to_utc(date, local_time)?;
        if !is_after_last(candidate, last_scheduled_for)? {
            continue;
        }
        if last_scheduled_for.is_none() && candidate < created_at {
            continue;
        }
        if candidate < now {
            if missed_run == CronMissedRunPolicy::RunOnce {
                best = Some(candidate);
            }
            continue;
        }
        return Ok(best.or(Some(candidate)));
    }
    Ok(best)
}

fn is_after_last(candidate: DateTime<Utc>, last_scheduled_for: Option<&str>) -> Result<bool> {
    let Some(last) = last_scheduled_for else {
        return Ok(true);
    };
    Ok(candidate > parse_rfc3339_utc(last)?)
}

fn parse_time(raw: &str) -> Result<NaiveTime> {
    let trimmed = raw.trim();
    NaiveTime::parse_from_str(trimmed, "%H:%M:%S")
        .or_else(|_| NaiveTime::parse_from_str(trimmed, "%H:%M"))
        .with_context(|| format!("invalid schedule time `{trimmed}`; expected HH:MM or HH:MM:SS"))
}

enum TimezoneSpec {
    Local,
    Utc,
    Fixed(FixedOffset),
}

impl TimezoneSpec {
    fn parse(raw: Option<&str>) -> Result<Self> {
        let value = raw.unwrap_or("local").trim();
        if value.is_empty() || value.eq_ignore_ascii_case("local") {
            return Ok(Self::Local);
        }
        if value.eq_ignore_ascii_case("utc") || value == "Z" {
            return Ok(Self::Utc);
        }
        if let Some(offset) = parse_fixed_offset(value) {
            return Ok(Self::Fixed(offset));
        }
        // IANA names need a timezone database crate. Keep named values accepted
        // as labels, but evaluate them with the process local timezone.
        Ok(Self::Local)
    }

    fn local_date(&self, now: DateTime<Utc>) -> NaiveDate {
        match self {
            TimezoneSpec::Local => now.with_timezone(&Local).date_naive(),
            TimezoneSpec::Utc => now.date_naive(),
            TimezoneSpec::Fixed(offset) => now.with_timezone(offset).date_naive(),
        }
    }

    fn local_to_utc(&self, date: NaiveDate, time: NaiveTime) -> Result<DateTime<Utc>> {
        let naive = NaiveDateTime::new(date, time);
        match self {
            TimezoneSpec::Local => local_result_to_utc(Local.from_local_datetime(&naive)),
            TimezoneSpec::Utc => Ok(Utc.from_utc_datetime(&naive)),
            TimezoneSpec::Fixed(offset) => local_result_to_utc(offset.from_local_datetime(&naive)),
        }
    }
}

fn local_result_to_utc<Tz: TimeZone>(value: LocalResult<DateTime<Tz>>) -> Result<DateTime<Utc>> {
    match value {
        LocalResult::Single(dt) => Ok(dt.with_timezone(&Utc)),
        LocalResult::Ambiguous(first, _) => Ok(first.with_timezone(&Utc)),
        LocalResult::None => bail!("local schedule time does not exist in the selected timezone"),
    }
}

fn parse_fixed_offset(raw: &str) -> Option<FixedOffset> {
    let trimmed = raw.trim();
    let sign = match trimmed.as_bytes().first().copied()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let body = &trimmed[1..];
    let (hours, minutes) = if let Some((h, m)) = body.split_once(':') {
        (h.parse::<i32>().ok()?, m.parse::<i32>().ok()?)
    } else if body.len() == 4 {
        (
            body[..2].parse::<i32>().ok()?,
            body[2..].parse::<i32>().ok()?,
        )
    } else {
        return None;
    };
    if hours > 23 || minutes > 59 {
        return None;
    }
    FixedOffset::east_opt(sign * (hours * 3600 + minutes * 60))
}

impl CronWeekday {
    fn to_chrono(self) -> Weekday {
        match self {
            CronWeekday::Mon => Weekday::Mon,
            CronWeekday::Tue => Weekday::Tue,
            CronWeekday::Wed => Weekday::Wed,
            CronWeekday::Thu => Weekday::Thu,
            CronWeekday::Fri => Weekday::Fri,
            CronWeekday::Sat => Weekday::Sat,
            CronWeekday::Sun => Weekday::Sun,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron::types::{CronJobPolicy, CronTarget, CronTask};

    fn job(schedule: CronSchedule) -> CronJob {
        CronJob {
            id: "job".to_string(),
            revision: 1,
            name: "job".to_string(),
            description: None,
            enabled: true,
            created_at: "2026-06-01T00:00:00Z".to_string(),
            updated_at: "2026-06-01T00:00:00Z".to_string(),
            schedule,
            task: CronTask::AgentPrompt {
                prompt: "test".to_string(),
            },
            target: CronTarget::Session {
                session_id: "session".to_string(),
            },
            policy: CronJobPolicy::default(),
        }
    }

    fn job_with_policy(schedule: CronSchedule, policy: CronJobPolicy) -> CronJob {
        let mut job = job(schedule);
        job.policy = policy;
        job
    }

    #[test]
    fn once_due_until_scheduled() -> Result<()> {
        let now = parse_rfc3339_utc("2026-06-01T00:05:00Z")?;
        let next = compute_next_run_at(
            &job(CronSchedule::Once {
                at: "2026-06-01T00:00:00Z".to_string(),
            }),
            &CronJobState::default(),
            now,
        )?;
        assert_eq!(next.unwrap(), parse_rfc3339_utc("2026-06-01T00:00:00Z")?);
        Ok(())
    }

    #[test]
    fn once_skip_does_not_fire_past_unscheduled_job() -> Result<()> {
        let mut policy = CronJobPolicy::default();
        policy.missed_run = CronMissedRunPolicy::Skip;
        let next = compute_next_run_at(
            &job_with_policy(
                CronSchedule::Once {
                    at: "2026-06-01T00:00:00Z".to_string(),
                },
                policy,
            ),
            &CronJobState::default(),
            parse_rfc3339_utc("2026-06-01T00:05:00Z")?,
        )?;
        assert_eq!(next, None);
        Ok(())
    }

    #[test]
    fn once_never_runs_after_it_was_scheduled() -> Result<()> {
        let mut state = CronJobState::default();
        state.last_scheduled_for = Some("2026-06-01T00:00:00Z".to_string());
        let next = compute_next_run_at(
            &job(CronSchedule::Once {
                at: "2026-06-01T00:00:00Z".to_string(),
            }),
            &state,
            parse_rfc3339_utc("2026-06-01T00:05:00Z")?,
        )?;
        assert_eq!(next, None);
        Ok(())
    }

    #[test]
    fn once_before_job_creation_is_not_a_missed_run() -> Result<()> {
        let mut new_job = job(CronSchedule::Once {
            at: "2026-06-01T00:00:00Z".to_string(),
        });
        new_job.created_at = "2026-06-01T00:05:00Z".to_string();
        let next = compute_next_run_at(
            &new_job,
            &CronJobState::default(),
            parse_rfc3339_utc("2026-06-01T00:06:00Z")?,
        )?;
        assert_eq!(next, None);
        Ok(())
    }

    #[test]
    fn interval_uses_last_scheduled_time() -> Result<()> {
        let mut state = CronJobState::default();
        state.last_scheduled_for = Some("2026-06-01T00:00:00Z".to_string());
        let next = compute_next_run_at(
            &job(CronSchedule::Interval {
                every_seconds: 300,
                anchor: None,
            }),
            &state,
            parse_rfc3339_utc("2026-06-01T00:01:00Z")?,
        )?;
        assert_eq!(next.unwrap(), parse_rfc3339_utc("2026-06-01T00:05:00Z")?);
        Ok(())
    }

    #[test]
    fn interval_run_once_fires_latest_missed_anchor() -> Result<()> {
        let next = compute_next_run_at(
            &job(CronSchedule::Interval {
                every_seconds: 300,
                anchor: Some("2026-06-01T00:00:00Z".to_string()),
            }),
            &CronJobState::default(),
            parse_rfc3339_utc("2026-06-01T00:11:00Z")?,
        )?;
        assert_eq!(next.unwrap(), parse_rfc3339_utc("2026-06-01T00:10:00Z")?);
        Ok(())
    }

    #[test]
    fn interval_skip_advances_to_future_after_missed_anchor() -> Result<()> {
        let mut policy = CronJobPolicy::default();
        policy.missed_run = CronMissedRunPolicy::Skip;
        let next = compute_next_run_at(
            &job_with_policy(
                CronSchedule::Interval {
                    every_seconds: 300,
                    anchor: Some("2026-06-01T00:00:00Z".to_string()),
                },
                policy,
            ),
            &CronJobState::default(),
            parse_rfc3339_utc("2026-06-01T00:11:00Z")?,
        )?;
        assert_eq!(next.unwrap(), parse_rfc3339_utc("2026-06-01T00:15:00Z")?);
        Ok(())
    }

    #[test]
    fn interval_anchor_exactly_now_is_due_even_with_skip_policy() -> Result<()> {
        let mut policy = CronJobPolicy::default();
        policy.missed_run = CronMissedRunPolicy::Skip;
        let now = parse_rfc3339_utc("2026-06-01T00:00:00Z")?;
        let next = compute_next_run_at(
            &job_with_policy(
                CronSchedule::Interval {
                    every_seconds: 300,
                    anchor: Some("2026-06-01T00:00:00Z".to_string()),
                },
                policy,
            ),
            &CronJobState::default(),
            now,
        )?;
        assert_eq!(next.unwrap(), now);
        Ok(())
    }

    #[test]
    fn interval_anchor_before_creation_keeps_phase_but_not_past_occurrences() -> Result<()> {
        let mut new_job = job(CronSchedule::Interval {
            every_seconds: 300,
            anchor: Some("2026-06-01T00:00:00Z".to_string()),
        });
        new_job.created_at = "2026-06-01T00:12:00Z".to_string();
        let next = compute_next_run_at(
            &new_job,
            &CronJobState::default(),
            parse_rfc3339_utc("2026-06-01T00:13:00Z")?,
        )?;
        assert_eq!(next.unwrap(), parse_rfc3339_utc("2026-06-01T00:15:00Z")?);
        Ok(())
    }

    #[test]
    fn interval_rejects_zero_seconds() {
        let err = compute_next_run_at(
            &job(CronSchedule::Interval {
                every_seconds: 0,
                anchor: None,
            }),
            &CronJobState::default(),
            Utc::now(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("every_seconds"));
    }

    #[test]
    fn daily_fixed_offset_understands_morning() -> Result<()> {
        let next = compute_next_run_at(
            &job(CronSchedule::Daily {
                time: "08:00".to_string(),
                timezone: Some("+08:00".to_string()),
            }),
            &CronJobState::default(),
            parse_rfc3339_utc("2026-06-01T00:10:00Z")?,
        )?;
        assert_eq!(next.unwrap(), parse_rfc3339_utc("2026-06-01T00:00:00Z")?);
        Ok(())
    }

    #[test]
    fn daily_skip_policy_returns_future_time_after_missed_run() -> Result<()> {
        let mut policy = CronJobPolicy::default();
        policy.missed_run = CronMissedRunPolicy::Skip;
        let next = compute_next_run_at(
            &job_with_policy(
                CronSchedule::Daily {
                    time: "08:00".to_string(),
                    timezone: Some("+08:00".to_string()),
                },
                policy,
            ),
            &CronJobState::default(),
            parse_rfc3339_utc("2026-06-01T00:10:00Z")?,
        )?;
        assert_eq!(next.unwrap(), parse_rfc3339_utc("2026-06-02T00:00:00Z")?);
        Ok(())
    }

    #[test]
    fn daily_time_before_creation_is_not_treated_as_missed() -> Result<()> {
        let mut new_job = job(CronSchedule::Daily {
            time: "08:00".to_string(),
            timezone: Some("+08:00".to_string()),
        });
        new_job.created_at = "2026-06-01T00:10:00Z".to_string();
        let next = compute_next_run_at(
            &new_job,
            &CronJobState::default(),
            parse_rfc3339_utc("2026-06-01T00:11:00Z")?,
        )?;
        assert_eq!(next.unwrap(), parse_rfc3339_utc("2026-06-02T00:00:00Z")?);
        Ok(())
    }

    #[test]
    fn weekly_fixed_offset_picks_next_allowed_weekday() -> Result<()> {
        let next = compute_next_run_at(
            &job(CronSchedule::Weekly {
                weekdays: vec![CronWeekday::Wed],
                time: "08:00".to_string(),
                timezone: Some("+08:00".to_string()),
            }),
            &CronJobState::default(),
            parse_rfc3339_utc("2026-06-01T00:10:00Z")?,
        )?;
        assert_eq!(next.unwrap(), parse_rfc3339_utc("2026-06-03T00:00:00Z")?);
        Ok(())
    }

    #[test]
    fn disabled_job_has_no_next_run() -> Result<()> {
        let mut disabled = job(CronSchedule::Daily {
            time: "08:00".to_string(),
            timezone: Some("UTC".to_string()),
        });
        disabled.enabled = false;
        assert_eq!(
            compute_next_run_at(&disabled, &CronJobState::default(), Utc::now())?,
            None
        );
        Ok(())
    }
}
