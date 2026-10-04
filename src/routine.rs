//! Scheduled prompts ("routines") — Grok Bot's per-agent routines, fired by
//! aviary's own tick while it runs. Pure time math here, unit-tested; the
//! shell owns the firing. No chrono: local time = epoch + a cached UTC offset
//! (refreshed hourly via `date +%z`, the same trade the room timestamps make).

use anyhow::{bail, Result};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Schedule {
    /// Every day at local HH:MM.
    Daily { h: u32, m: u32 },
    /// Monday–Friday at local HH:MM.
    Weekdays { h: u32, m: u32 },
    /// Every N seconds, anchored to the previous firing.
    Every { secs: u64 },
}

/// `daily@HH:MM` · `weekdays@HH:MM` · `every:<N>m|h`.
pub fn parse(s: &str) -> Result<Schedule> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("daily@") {
        let (h, m) = parse_hm(rest)?;
        return Ok(Schedule::Daily { h, m });
    }
    if let Some(rest) = s.strip_prefix("weekdays@") {
        let (h, m) = parse_hm(rest)?;
        return Ok(Schedule::Weekdays { h, m });
    }
    if let Some(rest) = s.strip_prefix("every:") {
        let (num, unit) = rest.split_at(rest.len().saturating_sub(1));
        let n: u64 = num.parse().map_err(|_| anyhow::anyhow!("bad interval {rest:?}"))?;
        if n == 0 {
            bail!("interval must be positive");
        }
        return match unit {
            "m" => Ok(Schedule::Every { secs: n * 60 }),
            "h" => Ok(Schedule::Every { secs: n * 3600 }),
            _ => bail!("interval unit must be m or h (got {rest:?})"),
        };
    }
    bail!("unknown schedule {s:?} — use daily@HH:MM, weekdays@HH:MM, or every:<N>m|h")
}

fn parse_hm(s: &str) -> Result<(u32, u32)> {
    let Some((h, m)) = s.split_once(':') else {
        bail!("expected HH:MM, got {s:?}");
    };
    let (h, m): (u32, u32) = (h.parse()?, m.parse()?);
    if h > 23 || m > 59 {
        bail!("{h:02}:{m:02} is not a time of day");
    }
    Ok((h, m))
}

/// Local-time view of an epoch, computed from a UTC offset in seconds.
#[derive(Clone, Copy)]
pub struct LocalTime {
    pub offset_secs: i64,
}

impl LocalTime {
    /// Parse a `date +%z` style offset ("-0700", "+0530").
    pub fn from_tz_offset(s: &str) -> Option<LocalTime> {
        let s = s.trim();
        let (sign, digits) = s.split_at(1);
        if digits.len() != 4 {
            return None;
        }
        let h: i64 = digits[..2].parse().ok()?;
        let m: i64 = digits[2..].parse().ok()?;
        let magnitude = h * 3600 + m * 60;
        Some(LocalTime {
            offset_secs: if sign == "-" { -magnitude } else { magnitude },
        })
    }

    /// Ask the system once (cache it); unix-epoch fallback = UTC.
    pub fn detect() -> LocalTime {
        std::process::Command::new("date")
            .arg("+%z")
            .output()
            .ok()
            .and_then(|o| LocalTime::from_tz_offset(&String::from_utf8_lossy(&o.stdout)))
            .unwrap_or(LocalTime { offset_secs: 0 })
    }

    fn local_secs(&self, epoch: u64) -> i64 {
        epoch as i64 + self.offset_secs
    }

    /// Days since epoch in local time (the "which day is it" key).
    pub fn day(&self, epoch: u64) -> i64 {
        self.local_secs(epoch).div_euclid(86_400)
    }

    /// Seconds past local midnight.
    pub fn time_of_day(&self, epoch: u64) -> i64 {
        self.local_secs(epoch).rem_euclid(86_400)
    }

    /// ISO weekday 1=Mon … 7=Sun (1970-01-01 was a Thursday).
    pub fn weekday(&self, epoch: u64) -> u32 {
        ((self.day(epoch) + 3).rem_euclid(7) + 1) as u32
    }
}

/// Is a routine due right now? `last_run` is the epoch of its previous firing
/// (None = never). Daily/weekday routines fire at most once per local day,
/// including one catch-up firing if aviary was closed at the scheduled minute;
/// interval routines anchor to the previous run and never fire on first sight
/// (the shell records `now` instead, avoiding a boot-storm of every routine).
pub fn due(schedule: &Schedule, last_run: Option<u64>, now: u64, clock: &LocalTime) -> bool {
    match schedule {
        Schedule::Every { secs } => match last_run {
            Some(prev) => now.saturating_sub(prev) >= *secs,
            None => false,
        },
        Schedule::Daily { h, m } | Schedule::Weekdays { h, m } => {
            if matches!(schedule, Schedule::Weekdays { .. }) && clock.weekday(now) > 5 {
                return false;
            }
            let target = (*h as i64) * 3600 + (*m as i64) * 60;
            if clock.time_of_day(now) < target {
                return false;
            }
            match last_run {
                Some(prev) => clock.day(prev) < clock.day(now),
                None => true,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UTC: LocalTime = LocalTime { offset_secs: 0 };

    #[test]
    fn parses_the_grammar_and_rejects_junk() {
        assert_eq!(parse("daily@09:00").unwrap(), Schedule::Daily { h: 9, m: 0 });
        assert_eq!(
            parse("weekdays@17:30").unwrap(),
            Schedule::Weekdays { h: 17, m: 30 }
        );
        assert_eq!(parse("every:30m").unwrap(), Schedule::Every { secs: 1800 });
        assert_eq!(parse("every:2h").unwrap(), Schedule::Every { secs: 7200 });
        for bad in ["daily@25:00", "daily@0900", "every:0m", "every:5x", "monthly@1"] {
            assert!(parse(bad).is_err(), "{bad} should not parse");
        }
    }

    #[test]
    fn tz_offsets_parse_both_signs() {
        assert_eq!(LocalTime::from_tz_offset("-0700").unwrap().offset_secs, -25200);
        assert_eq!(LocalTime::from_tz_offset("+0530").unwrap().offset_secs, 19800);
        assert!(LocalTime::from_tz_offset("junk").is_none());
    }

    /// 2026-10-04 00:00 UTC — a Sunday (20730 days past the epoch).
    const SUNDAY: u64 = 1_791_072_000;

    #[test]
    fn weekday_math_is_right() {
        // 1970-01-01 (epoch 0) was a Thursday.
        assert_eq!(UTC.weekday(0), 4);
        assert_eq!(UTC.weekday(SUNDAY), 7);
        assert_eq!(UTC.weekday(SUNDAY + 86_400), 1);
    }

    #[test]
    fn interval_routines_anchor_and_never_boot_storm() {
        let s = Schedule::Every { secs: 60 };
        assert!(!due(&s, None, 1_000, &UTC), "no firing on first sight");
        assert!(!due(&s, Some(960), 1_000, &UTC));
        assert!(due(&s, Some(940), 1_000, &UTC));
    }

    #[test]
    fn daily_fires_once_with_catch_up() {
        let s = Schedule::Daily { h: 9, m: 0 };
        let day = 20_000i64; // arbitrary local day
        let at = |tod: i64| (day * 86_400 + tod) as u64;
        // Before 09:00 — not due.
        assert!(!due(&s, None, at(8 * 3600), &UTC));
        // 09:30 and never run — due (catch-up covers "aviary opened late").
        assert!(due(&s, None, at(9 * 3600 + 1800), &UTC));
        // Already ran today — not due again.
        assert!(!due(&s, Some(at(9 * 3600 + 60)), at(18 * 3600), &UTC));
        // Ran yesterday — due today after 09:00.
        let yesterday = ((day - 1) * 86_400 + 9 * 3600) as u64;
        assert!(due(&s, Some(yesterday), at(9 * 3600), &UTC));
    }

    #[test]
    fn weekdays_skip_the_weekend() {
        let s = Schedule::Weekdays { h: 9, m: 0 };
        let sunday_10am = SUNDAY + 10 * 3600;
        assert!(!due(&s, None, sunday_10am, &UTC));
        let monday_10am = sunday_10am + 86_400;
        assert!(due(&s, None, monday_10am, &UTC));
    }
}
