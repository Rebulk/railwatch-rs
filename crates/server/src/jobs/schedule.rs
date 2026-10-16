// SPDX-License-Identifier: AGPL-3.0-only AND MIT
//
// Follows fugit 1.11's schedule grammar ("every ..." natural forms and cron),
// Copyright (c) 2017-2024 John Mettraux, MIT license (full text in
// THIRD-PARTY-LICENSES, "fugit").

//! The schedule strings Solid Queue recurring tasks carry, parsed the way
//! CheckScheduledTasksJob does with fugit: cron (5 or 6 fields, optionally
//! followed by a time zone) and fugit's natural forms ("every 5 minutes",
//! "every hour", "every hour at minute 5", "every day at 3am", "every monday
//! at 8am"). Times are UTC;
//! a trailing zone other than UTC/Etc/UTC is not supported and yields None
//! (the task is skipped, as fugit's parse failure skips it in Rails).

use chrono::{TimeZone, Utc};

/// `Fugit.parse(schedule).next_time(last_run)` in epoch seconds.
pub fn next_after(schedule: &str, after: i64) -> Option<i64> {
    let cron = to_cron(schedule.trim())?;
    let fields = cron.split_whitespace().count();
    let parser = croner::parser::CronParser::builder()
        .seconds(if fields == 6 { croner::parser::Seconds::Required } else { croner::parser::Seconds::Disallowed })
        .build();
    let c = parser.parse(&cron).ok()?;
    let start = Utc.timestamp_opt(after, 0).single()?;
    c.find_next_occurrence(&start, false).ok().map(|t| t.timestamp())
}

fn to_cron(s: &str) -> Option<String> {
    let lower = s.to_ascii_lowercase();
    if !lower.starts_with("every ") {
        let mut parts: Vec<&str> = s.split_whitespace().collect();
        if parts.len() == 6 || parts.len() == 7 {
            let last = *parts.last().unwrap();
            if last.contains('/') || last.eq_ignore_ascii_case("utc") {
                if !matches!(last.to_ascii_lowercase().as_str(), "utc" | "etc/utc") {
                    return None;
                }
                parts.pop();
            }
        }
        return (parts.len() == 5 || parts.len() == 6).then(|| parts.join(" "));
    }
    let rest = lower["every ".len()..].trim();
    // fugit takes a trailing zone on the natural forms too ("every day at 3am UTC").
    let rest = rest.strip_suffix(" etc/utc").or_else(|| rest.strip_suffix(" utc")).unwrap_or(rest).trim_end();
    let (what, at) = match rest.split_once(" at ") {
        Some((w, a)) => (w.trim(), Some(a.trim())),
        None => (rest, None),
    };
    let (h, m) = match at {
        // "every hour at minute 5" / "every 2 hours at minute 5" (fugit: "5 * * * *", "5 */2 * * *").
        Some(a) if a.starts_with("minute ") => (0, a["minute ".len()..].trim().parse::<u32>().ok().filter(|m| *m < 60)?),
        Some(a) => parse_at(a)?,
        None => (0, 0),
    };
    let words: Vec<&str> = what.split_whitespace().collect();
    let unit_n = |n: &str| n.parse::<u32>().ok().filter(|n| *n > 0);
    Some(match words.as_slice() {
        ["second"] => "* * * * * *".into(),
        [n, "seconds"] => format!("*/{} * * * * *", unit_n(n)?),
        ["minute"] => "* * * * *".into(),
        [n, "minutes"] => format!("*/{} * * * *", unit_n(n)?),
        ["hour"] => format!("{m} * * * *"),
        [n, "hours"] => format!("{m} */{} * * *", unit_n(n)?),
        ["day"] => format!("{m} {h} * * *"),
        [day] if weekday(day).is_some() => format!("{m} {h} * * {}", weekday(day)?),
        ["weekday"] => format!("{m} {h} * * 1-5"),
        ["month"] => format!("{m} {h} 1 * *"),
        _ => return None,
    })
}

fn weekday(d: &str) -> Option<u32> {
    ["sunday", "monday", "tuesday", "wednesday", "thursday", "friday", "saturday"].iter().position(|w| *w == d).map(|i| i as u32)
}

/// "3am", "8:30pm", "noon", "midnight", "14:05".
fn parse_at(a: &str) -> Option<(u32, u32)> {
    match a {
        "noon" => return Some((12, 0)),
        "midnight" => return Some((0, 0)),
        _ => {}
    }
    let (body, pm, am) = if let Some(b) = a.strip_suffix("pm") { (b, true, false) } else if let Some(b) = a.strip_suffix("am") { (b, false, true) } else { (a, false, false) };
    let (h, m) = match body.trim().split_once(':') {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?),
        None => (body.trim().parse::<u32>().ok()?, 0),
    };
    let h = match (am, pm) {
        (true, _) if h == 12 => 0,
        (_, true) if h < 12 => h + 12,
        _ => h,
    };
    (h < 24 && m < 60).then_some((h, m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solid_queue_schedules() {
        let t0 = 1_791_907_200; // 2026-10-13 16:00:00 UTC, a Tuesday
        assert_eq!(next_after("every 5 minutes", t0 + 61), Some(t0 + 300));
        assert_eq!(next_after("every minute", t0 + 5), Some(t0 + 60));
        assert_eq!(next_after("every hour at minute 17", t0), Some(t0 + 17 * 60));
        assert_eq!(next_after("every 2 hours at minute 5", t0 + 600), Some(t0 + 2 * 3600 + 300));
        assert_eq!(next_after("every day at 3am UTC", t0), Some(t0 + 11 * 3600));
        assert_eq!(next_after("every day at 4am America/New_York", t0), None);
        assert_eq!(next_after("every hour at minute 60", t0), None);
        assert_eq!(next_after("every hour", t0 + 1), Some(t0 + 3600));
        assert_eq!(next_after("every day at 3am", t0), Some(t0 + 11 * 3600));
        assert_eq!(next_after("every monday at 8am", t0), Some(t0 + 6 * 86_400 - 8 * 3600));
        assert_eq!(next_after("*/10 * * * *", t0 + 1), Some(t0 + 600));
        assert_eq!(next_after("0 4 * * * UTC", t0), Some(t0 + 12 * 3600));
        assert_eq!(next_after("0 4 * * * America/New_York", t0), None);
    }
}
