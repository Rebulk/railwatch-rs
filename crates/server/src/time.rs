//! UTC time formatting the way the Ruby side writes it. No timezone database
//! is needed: everything the ingest path stores is UTC.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A frozen clock (microseconds), i64::MIN = the real one. The equivalent of
/// Active Support's `travel_to`, for the Rails-equivalence harnesses
/// (bench/diff/detectors); the server never sets it.
static FROZEN: AtomicI64 = AtomicI64::new(i64::MIN);

/// Freezes `now_micros` / `now_f64` at `micros` until `travel_back`.
pub fn travel_to(micros: i64) {
    FROZEN.store(micros, Ordering::Relaxed);
}

pub fn travel_back() {
    FROZEN.store(i64::MIN, Ordering::Relaxed);
}

/// Seconds since the epoch, with sub-second precision (Time.current.to_f).
pub fn now_f64() -> f64 {
    let frozen = FROZEN.load(Ordering::Relaxed);
    if frozen != i64::MIN {
        return frozen as f64 / 1e6;
    }
    let d = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock before 1970");
    d.as_secs() as f64 + d.subsec_nanos() as f64 / 1e9
}

/// Microseconds since the epoch.
pub fn now_micros() -> i64 {
    let frozen = FROZEN.load(Ordering::Relaxed);
    if frozen != i64::MIN {
        return frozen;
    }
    let d = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock before 1970");
    d.as_secs() as i64 * 1_000_000 + d.subsec_micros() as i64
}

/// (year, month, day) for days since 1970-01-01 (Howard Hinnant's algorithm).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `format("%04d-%02d-%02d %02d:%02d:%02d", ...)` of `Time.at(seconds).utc`.
pub fn utc_seconds_string(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let secs = seconds.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, m, d, secs / 3600, (secs % 3600) / 60, secs % 60)
}

/// Active Record's quoted_date: "YYYY-MM-DD HH:MM:SS" plus ".ffffff" only
/// when the microseconds are non-zero.
pub fn quoted_date(seconds: i64, usec: u32) -> String {
    let base = utc_seconds_string(seconds);
    if usec > 0 { format!("{base}.{usec:06}") } else { base }
}

pub fn quoted_date_micros(micros: i64) -> String {
    quoted_date(micros.div_euclid(1_000_000), micros.rem_euclid(1_000_000) as u32)
}

/// `Time#iso8601` for a whole-second UTC time: "2026-01-01T00:00:00Z".
pub fn iso8601(seconds: i64) -> String {
    utc_seconds_string(seconds).replacen(' ', "T", 1) + "Z"
}

/// `Time#iso8601(6)`: "2026-10-13T13:08:32.940309Z".
pub fn iso8601_6(micros: i64) -> String {
    format!("{}.{:06}Z", utc_seconds_string(micros.div_euclid(1_000_000)).replacen(' ', "T", 1), micros.rem_euclid(1_000_000))
}

/// Parses "YYYY-MM-DD HH:MM:SS[.ffffff]" (how datetimes sit in SQLite) to
/// microseconds since the epoch.
pub fn parse_db_time(s: &str) -> Option<i64> {
    let (date, rest) = s.split_once([' ', 'T'])?;
    let mut d = date.split('-');
    let (y, mo, da): (i64, i64, i64) = (d.next()?.parse().ok()?, d.next()?.parse().ok()?, d.next()?.parse().ok()?);
    let rest = rest.trim_end_matches('Z');
    let (hms, frac) = rest.split_once('.').unwrap_or((rest, ""));
    let mut t = hms.split(':');
    let (h, mi, se): (i64, i64, i64) = (t.next()?.parse().ok()?, t.next()?.parse().ok()?, t.next().unwrap_or("0").parse().ok()?);
    let mut usec = 0i64;
    if !frac.is_empty() {
        let digits: String = frac.chars().take(6).collect();
        usec = format!("{digits:0<6}").parse().ok()?;
    }
    let days = days_from_civil(y, mo as u32, da as u32);
    Some(((days * 86_400) + h * 3600 + mi * 60 + se) * 1_000_000 + usec)
}

pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(utc_seconds_string(1767225600), "2026-01-01 00:00:00");
        assert_eq!(utc_seconds_string(-1), "1969-12-31 23:59:59");
        assert_eq!(utc_seconds_string(253402300800), "10000-01-01 00:00:00");
        assert_eq!(utc_seconds_string(-62167219201), "-001-12-31 23:59:59");
        assert_eq!(quoted_date(1767225602, 0), "2026-01-01 00:00:02");
        assert_eq!(quoted_date(1760000000, 122999), "2025-10-09 08:53:20.122999");
        assert_eq!(iso8601(1767225600), "2026-01-01T00:00:00Z");
        assert_eq!(parse_db_time("2025-10-09 08:53:20.122999"), Some(1_760_000_000_122_999));
        assert_eq!(parse_db_time("1970-01-01 00:00:00"), Some(0));
    }
}
