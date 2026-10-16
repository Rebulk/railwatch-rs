//! Port of Railwatch::Window and the window/step half of EnvironmentScoped.
//! Times are epoch microseconds; `now` is always an input so a page can be
//! computed at a frozen instant (the props-diff harness does).

pub const HOUR: i64 = 3_600_000_000;
pub const DAY: i64 = 24 * HOUR;
pub const MINUTE: i64 = 60_000_000;

pub const PRESETS: [(&str, i64); 5] = [("1h", HOUR), ("6h", 6 * HOUR), ("24h", DAY), ("7d", 7 * DAY), ("30d", 30 * DAY)];
pub const DEFAULT: &str = "24h";
pub const MAX_CUSTOM_RANGE: i64 = 90 * DAY;

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub key: String,
    pub from: i64,
    pub to: i64,
}

impl Window {
    /// `Window.preset(key)`: a preset ending now, or the default.
    pub fn preset(key: Option<&str>, now: i64) -> Window {
        let (k, span) = PRESETS
            .iter()
            .find(|(k, _)| Some(*k) == key)
            .or_else(|| PRESETS.iter().find(|(k, _)| *k == DEFAULT))
            .copied()
            .unwrap();
        Window { key: k.to_string(), from: now - span, to: now }
    }

    /// `Window.parse(window:, from:, to:)`.
    pub fn parse(window: Option<&str>, from: Option<&str>, to: Option<&str>, now: i64) -> Window {
        Self::custom(from, to, now).unwrap_or_else(|| Self::preset(window, now))
    }

    fn custom(from: Option<&str>, to: Option<&str>, now: i64) -> Option<Window> {
        let from = parse_time(from?)?;
        let to = match to {
            Some(t) if !t.trim().is_empty() => parse_time(t)?,
            _ => now,
        };
        (to > from && to - from <= MAX_CUSTOM_RANGE).then(|| Window { key: "custom".into(), from, to })
    }

    pub fn range(&self) -> (i64, i64) {
        (self.from, self.to)
    }

    pub fn span(&self) -> i64 {
        self.to - self.from
    }

    pub fn previous(&self) -> Window {
        Window { key: self.key.clone(), from: self.from - self.span(), to: self.from }
    }

    /// `{from: iso8601(6), to: iso8601(6)}`
    pub fn to_h(&self) -> serde_json::Value {
        crate::obj! { "from" => crate::time::iso8601_6(self.from), "to" => crate::time::iso8601_6(self.to) }
    }

    pub fn is_custom(&self) -> bool {
        self.key == "custom"
    }
}

/// `Time.zone.parse` (UTC zone) for the formats a URL carries: ISO 8601 with
/// or without fraction/offset, or "YYYY-MM-DD[ HH:MM[:SS]]". Anything else is
/// nil. A narrower parser than Ruby's (see docs/DIFFERENCES.md).
pub fn parse_time(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Split off a trailing offset (+HH:MM / -HH:MM / Z).
    let (body, offset_secs) = split_offset(s);
    let body = if body.len() == 10 { format!("{body} 00:00:00") } else { body.to_string() };
    let body = if body.matches(':').count() == 1 { format!("{body}:00") } else { body };
    let micros = crate::time::parse_db_time(&body)?;
    Some(micros - offset_secs * 1_000_000)
}

fn split_offset(s: &str) -> (&str, i64) {
    if let Some(b) = s.strip_suffix('Z').or_else(|| s.strip_suffix('z')) {
        return (b, 0);
    }
    if s.len() > 16 {
        let tail = &s[s.len() - 6..];
        let b = tail.as_bytes();
        if (b[0] == b'+' || b[0] == b'-') && b[3] == b':'
            && let (Ok(h), Ok(m)) = (tail[1..3].parse::<i64>(), tail[4..6].parse::<i64>()) {
                let sign = if b[0] == b'-' { -1 } else { 1 };
                return (&s[..s.len() - 6], sign * (h * 3600 + m * 60));
            }
    }
    (s, 0)
}

// -- Aggregations::STEPS ---------------------------------------------------

pub const STEPS: [(&str, i64); 6] =
    [("1m", MINUTE), ("5m", 5 * MINUTE), ("15m", 15 * MINUTE), ("1h", HOUR), ("6h", 6 * HOUR), ("1d", DAY)];
pub const MAX_POINTS: f64 = 400.0;

pub fn step_seconds(key: &str) -> Option<i64> {
    STEPS.iter().find(|(k, _)| *k == key).map(|(_, v)| v / 1_000_000)
}

pub fn default_step(from: i64, to: i64) -> &'static str {
    let span = to - from;
    if span <= 2 * HOUR {
        "1m"
    } else if span <= 12 * HOUR {
        "5m"
    } else if span <= 7 * DAY {
        "1h"
    } else {
        "6h"
    }
}

/// `(span / step).between?(2, MAX_POINTS)`: span is a Float of seconds in
/// Ruby (Time - Time), so the division is a float one.
pub fn steps_for(from: i64, to: i64) -> Vec<&'static str> {
    let span = super::rb::time_sub(to, from);
    STEPS
        .iter()
        .filter(|(_, step)| {
            let n = span / (*step as f64 / 1e6);
            (2.0..=MAX_POINTS).contains(&n)
        })
        .map(|(k, _)| *k)
        .collect()
}

/// EnvironmentScoped#step_key.
pub fn step_key(from: i64, to: i64, requested: Option<&str>) -> &'static str {
    let offered = steps_for(from, to);
    match requested.and_then(|r| offered.iter().find(|k| **k == r)) {
        Some(k) => k,
        None => default_step(from, to),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_and_custom() {
        let now = 1_760_000_000_000_000;
        assert_eq!(Window::parse(Some("7d"), None, None, now).from, now - 7 * DAY);
        assert_eq!(Window::parse(Some("bogus"), None, None, now).key, "24h");
        let w = Window::parse(None, Some("2025-10-01T00:00:00Z"), Some("2025-10-02T00:00:00Z"), now);
        assert_eq!((w.key.as_str(), w.span()), ("custom", DAY));
        // reversed, or over 90 days, falls back to the preset
        assert_eq!(Window::parse(None, Some("2025-10-02"), Some("2025-10-01"), now).key, "24h");
        assert_eq!(Window::parse(Some("1h"), Some("2025-01-01"), Some("2025-10-01"), now).key, "1h");
    }

    #[test]
    fn steps() {
        assert_eq!(steps_for(0, HOUR), vec!["1m", "5m", "15m"]);
        assert_eq!(steps_for(0, DAY), vec!["5m", "15m", "1h", "6h"]);
        assert_eq!(step_key(0, DAY, Some("1m")), "1h");
        assert_eq!(step_key(0, DAY, Some("15m")), "15m");
    }
}
