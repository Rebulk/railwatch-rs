//! Compact results for agents. A security read is mostly the same request
//! or log line many times over: here those fold into one entry each, and
//! the rows that are still listed go on a diet.
//!
//! Lossless: an entry keeps its exact count, first and last time, the
//! values of every field that varies within it with their counts (the ten
//! commonest listed, the rest counted), a sample execution_id, and `expand`:
//! the arguments that list its rows again (`group`, its id, and the query
//! narrowed to its first and last time). Entries past the limit are counted
//! in `more`, by key. Nothing here summarizes with a model or samples
//! without counting.
//!
//! - Requests fold by status, route, outcome, halted callback and exception
//!   class (`request_key`); method, path, user, tenant, address, user agent,
//!   country, redirect, rate limit, dropped parameters and the exception's
//!   message are counted per entry.
//! - Log lines fold by level and message template (`template`: numbers,
//!   uuids, hex, emails, addresses and quoted strings masked); exceptions
//!   by class and template. Two messages are kept verbatim per entry.
//! - Per entry: `bursts` (requests within 2 s of each other), `cadence` (the
//!   median gap between bursts, and whether it is regular), `hourly`.
//! - Rows (`shape: "compact"`): no null fields, one `url_template` in place
//!   of a url per row, fields equal on every row in `common`, long deploy
//!   versions shortened through a `deploys` table.

use std::collections::HashMap;
use std::sync::LazyLock;

use indexmap::IndexMap;
use serde_json::{Map, Value};

use super::{Args, Fail};
use crate::dashboard::db::{Rec, jf};
use crate::dashboard::rb;
use crate::dashboard::request_scan::{FIELD_CHARS, Row, percentile, unpermitted_keys, untrusted};

/// What a search returns.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shape {
    /// Entries folded by shape.
    Grouped,
    /// Rows without nulls, urls or repeated values.
    Compact,
    /// Rows as they were before compact output.
    Rows,
}

/// The `shape` argument, or `default`.
pub(super) fn shape(a: &Args, default: Shape) -> Result<Shape, Value> {
    match a.presence("shape").as_deref() {
        None => Ok(default),
        Some("grouped") => Ok(Shape::Grouped),
        Some("compact") => Ok(Shape::Compact),
        Some("rows") => Ok(Shape::Rows),
        Some(s) => Err(serde_json::json!({"error": format!("shape must be grouped, compact or rows, not {s:?}")})),
    }
}

/// The `max_tokens` argument as a byte budget for the listed entries (4
/// bytes a token, 2 KB at least); without it, what fits under the result
/// cap with room for the rest of the result, so entries past it are counted
/// in `more` rather than cut by the cap.
pub(super) fn budget(a: &Args) -> Result<Option<usize>, Fail> {
    if !a.present("max_tokens") {
        return Ok(Some(DEFAULT_BUDGET));
    }
    let n = super::ruby_to_i(a.get("max_tokens")).filter(|n| *n > 0).ok_or_else(|| Fail::Invalid("max_tokens must be a positive number".into()))?;
    Ok(Some((n as usize).saturating_mul(4).clamp(2_000, DEFAULT_BUDGET)))
}

/// The entries' share of a result under the cap (MAX_RESULT_BYTES).
const DEFAULT_BUDGET: usize = super::mcp_summary::MAX_RESULT_BYTES - 8_000;

/// Entry fields that stay on each entry when every entry shares them.
pub const ENTRY_KEEP: [&str; 12] =
    ["count", "status", "route", "level", "class_name", "first", "last", "at", "execution_id", "sample_execution_id", "slowest_execution_id", "duration_ms"];

/// Distinct values an entry lists per varying field; the rest are counted.
pub const VARYING_VALUES: usize = 10;
/// Distinct messages an entry lists verbatim.
pub const MESSAGE_EXAMPLES: usize = 2;
/// Distinct values a field counts one by one; past this the rest are
/// counted together (a scanner's paths are what would grow without end).
const TRACKED_VALUES: usize = 10_000;
/// Requests this close to the one before are one burst.
const BURST_GAP: i64 = 2_000_000;
const HOUR: i64 = 3_600_000_000;

/// A short stable id for an entry's key: the same key gives the same id in
/// every call, so a group can be listed again by it.
pub fn group_id(parts: &[&str]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(parts.join("\u{1f}").as_bytes());
    digest.iter().take(5).map(|b| format!("{b:02x}")).collect()
}

// -- Entry keys ---------------------------------------------------------------

/// A request's entry key: status, route, outcome, halted callback,
/// exception class (as filter_query's grammar has them).
fn request_key(r: &Row<'_>) -> [String; 5] {
    let cut = |s: Option<&str>| s.map(|s| untrusted(s, FIELD_CHARS)).unwrap_or_default();
    [
        r.status.map(|s| s.to_string()).unwrap_or_default(),
        untrusted(r.name, FIELD_CHARS),
        r.outcome().unwrap_or("").to_string(),
        cut(r.halted),
        cut(r.exception),
    ]
}

/// Whether `key` (a request_key) is `r`'s, without building `r`'s.
fn same_request_key(key: &[String], r: &Row<'_>) -> bool {
    // Values past FIELD_CHARS or with control characters are cut or
    // changed in the key: compare them the slow way.
    let eq = |k: &str, v: Option<&str>| match v {
        None => k.is_empty(),
        Some(v) if v.len() <= FIELD_CHARS && !v.chars().any(|c| c.is_control() || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')) => k == v,
        Some(v) => k == untrusted(v, FIELD_CHARS),
    };
    let status_eq = match r.status {
        Some(s) => key[0].parse::<i64>().ok() == Some(s),
        None => key[0].is_empty(),
    };
    status_eq && eq(&key[1], Some(r.name)) && key[2] == r.outcome().unwrap_or("") && eq(&key[3], r.halted) && eq(&key[4], r.exception)
}

pub fn request_group(r: &Row<'_>) -> String {
    let k = request_key(r);
    group_id(&k.each_ref().map(String::as_str))
}

/// The columns `add_record` reads, for a grouped read of logs or exceptions.
pub const LOG_COLUMNS: &str = "\"logs\".\"id\", \"logs\".\"occurred_at\", \"logs\".\"level\", \"logs\".\"message\", \"logs\".\"execution_id\", \
     \"logs\".\"source\", \"logs\".\"execution_source\", \"logs\".\"execution_preview\", \"logs\".\"app_tenant\", \"logs\".\"user_ref\", \"logs\".\"deploy\", \
     \"logs\".\"context\", \"logs\".\"tags\"";
pub const EXCEPTION_COLUMNS: &str = "\"exceptions\".\"id\", \"exceptions\".\"occurred_at\", \"exceptions\".\"class_name\", \"exceptions\".\"message\", \
     \"exceptions\".\"execution_id\", \"exceptions\".\"handled\", \"exceptions\".\"severity\", \"exceptions\".\"group_hash\", \"exceptions\".\"file\", \
     \"exceptions\".\"line\", \"exceptions\".\"source\", \"exceptions\".\"execution_source\", \"exceptions\".\"execution_preview\", \
     \"exceptions\".\"app_tenant\", \"exceptions\".\"user_ref\", \"exceptions\".\"deploy\"";

/// A log line's or exception's entry key: (level or class, message
/// template), from the columns log_row and exception_row read.
fn record_key(exceptions: bool, r: &Rec, templates: &mut Templates) -> [String; 2] {
    let message = r.s("message").unwrap_or("");
    if exceptions {
        [r.s("class_name").unwrap_or("").to_string(), templates.of(rb::first_chars(message, 500))]
    } else {
        [r.s("level").unwrap_or("").to_string(), templates.of(rb::first_chars(message, 2000))]
    }
}

/// Each message's template, worked out once: a log is mostly the same few
/// lines over and over.
#[derive(Default)]
pub struct Templates(HashMap<String, String>);

impl Templates {
    fn of(&mut self, message: &str) -> String {
        if let Some(t) = self.0.get(message) {
            return t.clone();
        }
        let t = template(message);
        // Past this many distinct messages, worked out each time.
        if self.0.len() < TRACKED_VALUES {
            self.0.insert(message.to_string(), t.clone());
        }
        t
    }
}

pub fn record_group(exceptions: bool, r: &Rec, templates: &mut Templates) -> String {
    let k = record_key(exceptions, r, templates);
    group_id(&k.each_ref().map(String::as_str))
}

// -- Folding ------------------------------------------------------------------

/// Rows folded into entries by key, in first-seen order.
#[derive(Default)]
pub struct Groups {
    /// key fields -> (the entry's key JSON, its fold); the id is hashed from
    /// the key fields once, in `into_entries`.
    groups: IndexMap<Vec<String>, (Map<String, Value>, Fold)>,
    templates: Templates,
    /// The index of the group the last request went to.
    last: Option<usize>,
    /// Rows added.
    pub rows: usize,
}

impl Groups {
    /// The index of `key`'s group, made with its `json` when new.
    fn entry_index(&mut self, key: Vec<String>, json: impl FnOnce(&[String]) -> Map<String, Value>) -> usize {
        self.rows += 1;
        match self.groups.entry(key) {
            indexmap::map::Entry::Occupied(e) => e.index(),
            indexmap::map::Entry::Vacant(e) => {
                let j = json(e.key());
                let i = e.index();
                e.insert((j, Fold::default()));
                i
            }
        }
    }

    /// One request (rows come newest first). `access`: count addresses and
    /// user agents; `skip`: fields the caller already pins (ip_activity's
    /// address).
    pub fn add_request(&mut self, r: &Row<'_>, access: bool, skip: &[&str]) {
        // Most requests repeat the last one's key: find it without
        // building the key's strings.
        let hit = match self.last.and_then(|i| self.groups.get_index_mut(i)) {
            Some((k, (_, fold))) if same_request_key(k, r) => Some(fold),
            _ => None,
        };
        let fold = match hit {
            Some(fold) => {
                self.rows += 1;
                fold
            }
            None => {
                let i = self.entry_index(request_key(r).into(), request_key_json);
                self.last = Some(i);
                &mut self.groups[i].1
            }
        };
        fold_request(fold, r, access, skip);
    }

    /// One log line (`exceptions` false) or exception, rows newest first.
    /// Deploys are shortened through `deploys`.
    pub fn add_record(&mut self, r: &Rec, exceptions: bool, deploys: &mut Deploys) {
        let key = record_key(exceptions, r, &mut self.templates);
        let i = self.entry_index(key.into(), |k| {
            let mut m = Map::new();
            m.insert(if exceptions { "class_name" } else { "level" }.into(), Value::from(untrusted(&k[0], FIELD_CHARS)));
            // Kept only when the entry has more than one message.
            m.insert("template".into(), Value::from(k[1].clone()));
            m
        });
        let fold = &mut self.groups[i].1;
        fold.add(r.t("occurred_at").unwrap_or(0), None, r.s("execution_id"));
        // The message as log_row / exception_row cut it, control characters
        // and all: it is the app's text, shown as a JSON string.
        fold.field("message", r.s("message"), if exceptions { 500 } else { 2000 }, false);
        if exceptions {
            fold.field("handled", r.b("handled").map(|h| if h { "true" } else { "false" }), 8, true);
            fold.field("severity", r.s("severity"), FIELD_CHARS, true);
            fold.field("group_hash", r.s("group_hash"), FIELD_CHARS, true);
            let file = r.s("file").map(|f| r.i("line").map_or_else(|| f.to_string(), |l| format!("{f}:{l}")));
            fold.field("file", file.as_deref(), FIELD_CHARS, true);
        }
        for (name, col) in
            [("source", "source"), ("execution_source", "execution_source"), ("execution_preview", "execution_preview"), ("tenant", "app_tenant")]
        {
            fold.field(name, r.s(col), FIELD_CHARS, true);
        }
        fold.field("user_ref", r.s("user_ref"), FIELD_CHARS, true);
        let deploy = r.s("deploy").map(|d| deploys.short(d));
        fold.field("deploy", deploy.as_deref(), FIELD_CHARS, true);
        if !exceptions {
            fold.field("context", r.s("context").filter(|c| !matches!(*c, "{}" | "null")), FIELD_CHARS, false);
            fold.field("tags", r.s("tags").filter(|t| !matches!(*t, "[]" | "null")), FIELD_CHARS, false);
        }
    }

    /// The entries, in first-seen order.
    pub fn into_entries(self) -> Vec<Entry> {
        self.groups.into_iter().map(|(k, (key, fold))| finish(group_id(&k.iter().map(String::as_str).collect::<Vec<_>>()), key, fold)).collect()
    }
}

/// Requests folded into runs: consecutive requests with the same entry key
/// are one entry, so a timeline keeps its order. At most `limit` runs, the
/// newest (rows come newest first); the requests before them are counted
/// in `left_out`.
pub struct Runs {
    runs: Vec<([String; 5], Map<String, Value>, Fold)>,
    limit: usize,
    pub left_out: i64,
}

impl Runs {
    pub fn new(limit: usize) -> Runs {
        Runs { runs: vec![], limit, left_out: 0 }
    }

    pub fn add_request(&mut self, r: &Row<'_>, access: bool, skip: &[&str]) {
        if self.left_out > 0 {
            self.left_out += 1;
            return;
        }
        let n = self.runs.len();
        match self.runs.last_mut() {
            Some((last, _, fold)) if same_request_key(last, r) => fold_request(fold, r, access, skip),
            _ if n < self.limit => {
                let key = request_key(r);
                let mut fold = Fold::default();
                fold_request(&mut fold, r, access, skip);
                let json = request_key_json(&key);
                self.runs.push((key, json, fold));
            }
            _ => self.left_out = 1,
        }
    }

    /// The runs, newest first.
    pub fn into_entries(self) -> Vec<Entry> {
        self.runs.into_iter().map(|(k, key, fold)| finish(group_id(&k.each_ref().map(String::as_str)), key, fold)).collect()
    }
}

/// One run of requests that share a request key (a session journey's,
/// oldest first): its entry, folded as `Runs` folds one, with the group id
/// search_requests' `group` takes.
pub fn request_entry(rows: &[Row<'_>], access: bool) -> Entry {
    let key = request_key(&rows[0]);
    let mut fold = Fold::default();
    for r in rows {
        fold_request(&mut fold, r, access, &[]);
    }
    finish(group_id(&key.each_ref().map(String::as_str)), request_key_json(&key), fold)
}

/// An entry of other records from its key fields and its fold.
pub fn entry(id: String, key: Map<String, Value>, fold: Fold) -> Entry {
    finish(id, key, fold)
}

/// An entry from its fold: the largest response size, and the template
/// only when the messages differ.
fn finish(id: String, mut key: Map<String, Value>, mut fold: Fold) -> Entry {
    if fold.fields.iter().any(|(n, m)| *n == "message" && m.counts.len() == 1 && m.untracked == 0) {
        key.shift_remove("template");
    }
    let mut extra = Map::new();
    if fold.max_response > 0 {
        extra.insert(if fold.count > 1 { "max_response_size" } else { "response_size" }.into(), Value::from(fold.max_response));
    }
    Entry::new(id, key, &mut fold, extra)
}

/// A request entry's key fields: status and route always, outcome, halted
/// and exception when set.
fn request_key_json(key: &[String]) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("status".into(), key[0].parse::<i64>().map_or(Value::Null, Value::from));
    m.insert("route".into(), Value::from(key[1].clone()));
    for (k, v) in ["outcome", "halted", "exception"].into_iter().zip(&key[2..]) {
        if !v.is_empty() {
            m.insert(k.into(), Value::from(v.clone()));
        }
    }
    m
}

/// One request into its entry's fold.
fn fold_request(fold: &mut Fold, r: &Row<'_>, access: bool, skip: &[&str]) {
    fold.add(r.at, Some(r.duration), Some(r.execution_id));
    let keys = r.unpermitted.then(|| unpermitted_keys(r.unpermitted_keys).join(","));
    let rate_limit = r.rate_limit.map(|n| if n.is_empty() { "(unnamed)" } else { n });
    // The route name starts with the method (mapper: "GET /orders"):
    // counted only when it does not.
    let method = r.method.filter(|m| !r.name.strip_prefix(m).is_some_and(|rest| rest.starts_with(' ')));
    let fields: [(&'static str, Option<&str>); 11] = [
        ("method", method),
        ("path", r.path),
        ("user", r.user),
        ("tenant", r.tenant),
        ("ip", r.ip),
        ("user_agent", r.user_agent),
        ("country", r.country),
        ("redirect_to", r.redirect_to),
        ("rate_limit", rate_limit),
        ("unpermitted", keys.as_deref()),
        ("exception_preview", r.exception_preview),
    ];
    for (name, v) in fields {
        if skip.contains(&name) || (!access && matches!(name, "ip" | "user_agent")) {
            continue;
        }
        fold.field(name, v, FIELD_CHARS, true);
    }
    if let Some(n) = r.response_size.filter(|n| *n > 0) {
        fold.max_response = fold.max_response.max(n);
    }
}

/// The arguments that list an entry's rows again: its group and the query
/// narrowed to its first and last time, over the longest window (so it
/// still covers them later). Every row of the group in that range is one
/// of the entry's (a run's range holds no other run of the same group).
/// The rows come a page (`limit`) at a time, each page with its `next`.
pub fn expand(parsed: &crate::dashboard::filter_query::Parsed, e: &Entry) -> Value {
    let mut p = parsed.clone();
    p.fields.insert("after".into(), time(e.first));
    p.fields.insert("before".into(), time(e.last));
    serde_json::json!({"q": crate::dashboard::filter_query::unparse(&p), "window": "30d", "group": e.id})
}

/// Each entry of more than one row with its `expand` (before `rank`, so a
/// budget counts it).
pub fn with_expand(mut entries: Vec<Entry>, parsed: &crate::dashboard::filter_query::Parsed) -> Vec<Entry> {
    for e in &mut entries {
        if e.count > 1 {
            let x = expand(parsed, e);
            e.json.insert("expand".into(), x);
        }
    }
    entries
}

/// One field's values within an entry.
#[derive(Default)]
struct Values {
    /// value -> count, in first-seen order.
    counts: IndexMap<String, i64>,
    /// Rows whose value came after TRACKED_VALUES distinct ones.
    untracked: i64,
    /// Rows without the field.
    without: i64,
}

/// One entry's rows: count, times, durations, field values.
#[derive(Default)]
pub struct Fold {
    pub count: i64,
    times: Vec<i64>,
    durations: Vec<i64>,
    sample: Option<String>,
    /// The slowest row's duration and execution_id.
    slowest: Option<(i64, String)>,
    /// In the order first added (a few fields: a list, not a map).
    fields: Vec<(&'static str, Values)>,
    max_response: i64,
}

impl Fold {
    /// One row: its time, duration (microseconds) and execution_id. The
    /// first row added is the sample (rows come newest first).
    pub fn add(&mut self, at: i64, duration: Option<i64>, execution_id: Option<&str>) {
        self.count += 1;
        self.times.push(at);
        let id = execution_id.filter(|e| !e.is_empty());
        if self.sample.is_none() {
            self.sample = id.map(str::to_string);
        }
        if let Some(d) = duration {
            self.durations.push(d);
            if let Some(id) = id
                && self.slowest.as_ref().is_none_or(|(s, _)| d > *s)
            {
                self.slowest = Some((d, id.to_string()));
            }
        }
    }

    /// One row's value of `name` (call it for every row, None included):
    /// cut to `chars`, and with control characters replaced when the
    /// sender wrote it (`client`).
    pub fn field(&mut self, name: &'static str, v: Option<&str>, chars: usize, client: bool) {
        let values = match self.fields.iter().position(|(n, _)| *n == name) {
            Some(i) => &mut self.fields[i].1,
            None => {
                self.fields.push((name, Values::default()));
                &mut self.fields.last_mut().expect("pushed").1
            }
        };
        match v.filter(|v| !v.is_empty()) {
            None => values.without += 1,
            Some(v) => {
                // Most values are already as shown (short, no control
                // characters): count them without an allocation.
                if let Some(n) = values.counts.get_mut(v) {
                    *n += 1;
                    return;
                }
                let v = if client { untrusted(v, chars) } else { rb::first_chars(v, chars).to_string() };
                if let Some(n) = values.counts.get_mut(&v) {
                    *n += 1;
                } else if values.counts.len() < TRACKED_VALUES {
                    values.counts.insert(v, 1);
                } else {
                    values.untracked += 1;
                }
            }
        }
    }
}

/// A folded entry: its count and times (to rank and fold it) and its JSON.
pub struct Entry {
    pub id: String,
    pub count: i64,
    pub first: i64,
    pub last: i64,
    pub json: Map<String, Value>,
}

impl Entry {
    fn new(id: String, key: Map<String, Value>, fold: &mut Fold, extra: Map<String, Value>) -> Entry {
        fold.times.sort_unstable();
        fold.durations.sort_unstable();
        let (first, last) = (fold.times.first().copied().unwrap_or(0), fold.times.last().copied().unwrap_or(0));
        let mut m = Map::new();
        // One row: the row itself, without the entry's apparatus.
        if fold.count == 1 {
            m.insert("count".into(), Value::from(1));
            m.extend(key);
            m.insert("at".into(), Value::from(time(first)));
            for (name, values) in &fold.fields {
                if let Some((v, _)) = values.counts.first() {
                    m.insert((*name).into(), Value::from(v.clone()));
                }
            }
            if let Some(d) = fold.durations.first() {
                m.insert("duration_ms".into(), ms(*d));
            }
            m.extend(extra);
            if let Some(s) = &fold.sample {
                m.insert("execution_id".into(), Value::from(s.clone()));
            }
            return Entry { id, count: 1, first, last, json: m };
        }
        m.insert("count".into(), Value::from(fold.count));
        m.extend(key);
        m.insert("first".into(), Value::from(time(first)));
        m.insert("last".into(), Value::from(time(last)));
        for (k, v) in signals(&fold.times) {
            m.insert(k.into(), Value::from(v));
        }
        if !fold.durations.is_empty() {
            m.insert("duration_ms".into(), serde_json::json!({"p50": ms(percentile(&fold.durations, 50)), "max": ms(*fold.durations.last().unwrap())}));
        }
        let mut varying = Map::new();
        let mut rest = Map::new();
        for (name, values) in &fold.fields {
            if values.counts.is_empty() && values.untracked == 0 {
                continue;
            }
            // The same on every row: a plain field.
            if values.counts.len() == 1 && values.untracked == 0 && values.without == 0 {
                m.insert((*name).into(), Value::from(values.counts.first().unwrap().0.clone()));
                continue;
            }
            let n = if *name == "message" { MESSAGE_EXAMPLES } else { VARYING_VALUES };
            let mut top: Vec<(&String, &i64)> = values.counts.iter().collect();
            // Stable: ties keep first-seen (newest) order.
            top.sort_by(|a, b| b.1.cmp(a.1));
            varying.insert((*name).into(), Value::Object(top.iter().take(n).map(|(k, c)| ((*k).clone(), Value::from(**c))).collect()));
            let mut r = Map::new();
            let more_values = top.len().saturating_sub(n);
            if more_values > 0 || values.untracked > 0 {
                let distinct = if values.untracked > 0 { Value::from(format!(">={}", more_values + 1)) } else { Value::from(more_values as i64) };
                r.insert("values".into(), distinct);
                r.insert("count".into(), Value::from(top.iter().skip(n).map(|(_, c)| **c).sum::<i64>() + values.untracked));
            }
            if values.without > 0 {
                r.insert("none".into(), Value::from(values.without));
            }
            if !r.is_empty() {
                rest.insert((*name).into(), Value::Object(r));
            }
        }
        if !varying.is_empty() {
            m.insert("varying".into(), Value::Object(varying));
        }
        if !rest.is_empty() {
            m.insert("not_listed".into(), Value::Object(rest));
        }
        m.extend(extra);
        if let Some(s) = &fold.sample {
            m.insert("sample_execution_id".into(), Value::from(s.clone()));
        }
        // The one to open for duration_ms.max.
        if let Some((_, s)) = &fold.slowest
            && Some(s) != fold.sample.as_ref()
        {
            m.insert("slowest_execution_id".into(), Value::from(s.clone()));
        }
        Entry { id, count: fold.count, first, last, json: m }
    }
}

/// A time as ISO 8601, without fractional seconds when it has none (the
/// filter grammar's after:/before: read both).
pub fn time(us: i64) -> String {
    if us.rem_euclid(1_000_000) == 0 { crate::time::iso8601(us.div_euclid(1_000_000)) } else { crate::time::iso8601_6(us) }
}

fn ms(us: i64) -> Value {
    jf(rb::round(us as f64 / 1000.0, 2))
}

/// Which entries to list: the biggest first (ties: the newest), at most
/// `limit`, and within `budget` bytes when given (the first always). The
/// rest are counted in `more` by the values of `by`.
pub fn rank(mut entries: Vec<Entry>, limit: usize, budget: Option<usize>, by: &[&str]) -> (Vec<Entry>, Option<Value>) {
    entries.sort_by(|a, b| b.count.cmp(&a.count).then(b.last.cmp(&a.last)));
    let mut keep = limit.min(entries.len());
    if let Some(budget) = budget {
        let mut used = 0;
        for (i, e) in entries.iter().take(keep).enumerate() {
            used += Value::Object(e.json.clone()).to_string().len() + 1;
            if used > budget && i > 0 {
                keep = i;
                break;
            }
        }
    }
    let folded = entries.split_off(keep);
    (entries, fold_rest(&folded, by))
}

/// What `rank` leaves out: {entries, count, by: {key: {value: count}}}.
pub fn fold_rest(folded: &[Entry], by: &[&str]) -> Option<Value> {
    if folded.is_empty() {
        return None;
    }
    let mut m = Map::new();
    m.insert("entries".into(), Value::from(folded.len() as i64));
    m.insert("count".into(), Value::from(folded.iter().map(|e| e.count).sum::<i64>()));
    let mut by_key = Map::new();
    for k in by {
        let mut counts: IndexMap<String, i64> = IndexMap::new();
        for e in folded {
            if let Some(v) = e.json.get(*k).filter(|v| !v.is_null()) {
                *counts.entry(v.as_str().map_or_else(|| v.to_string(), str::to_string)).or_default() += e.count;
            }
        }
        if counts.is_empty() {
            continue;
        }
        let mut top: Vec<(String, i64)> = counts.into_iter().collect();
        top.sort_by_key(|a| std::cmp::Reverse(a.1));
        let n = top.len();
        let mut v = Map::new();
        v.insert("values".into(), Value::from(n as i64));
        v.insert("top".into(), Value::Object(top.into_iter().take(VARYING_VALUES).map(|(k, c)| (k, Value::from(c))).collect()));
        by_key.insert((*k).into(), Value::Object(v));
    }
    if !by_key.is_empty() {
        m.insert("by".into(), Value::Object(by_key));
    }
    Some(Value::Object(m))
}

// -- Signals ------------------------------------------------------------------

/// `bursts`, `cadence` and `hourly` of sorted times (microseconds), each
/// only when it says something:
/// - bursts: a request within 2 s of the one before is in its burst. "in
///   pairs", "in threes", "in bursts of ~N (up to M)" (the median burst),
///   or "N bursts of 2-M" when most came alone;
/// - cadence: the median gap between the starts of bursts, "regular" when
///   two thirds of the gaps are within 20% of it (4 bursts at least);
/// - hourly: requests per hour, over the hours from first to last ("3-31
///   an hour, in 24 of 25 hours"; 10 requests over 2 hours at least).
pub fn signals(times: &[i64]) -> Vec<(&'static str, String)> {
    let mut out = vec![];
    if times.len() < 2 {
        return out;
    }
    // (start, size, last)
    let mut bursts: Vec<(i64, usize, i64)> = vec![];
    for &t in times {
        match bursts.last_mut() {
            Some((_, n, last)) if t - *last <= BURST_GAP => {
                *n += 1;
                *last = t;
            }
            _ => bursts.push((t, 1, t)),
        }
    }
    let mut sizes: Vec<usize> = bursts.iter().map(|b| b.1).collect();
    sizes.sort_unstable();
    let biggest = *sizes.last().unwrap();
    if biggest > 1 {
        let several = sizes.iter().filter(|s| **s > 1).count();
        let text = match sizes[(sizes.len() - 1) / 2] {
            1 if biggest == 2 => format!("{several} {}, the rest alone", if several == 1 { "pair" } else { "pairs" }),
            1 => format!("{several} bursts of 2-{biggest}, the rest alone"),
            2 if biggest == 2 => "in pairs".to_string(),
            3 if biggest == 3 => "in threes".to_string(),
            m => format!("in bursts of ~{m} (up to {biggest})"),
        };
        out.push(("bursts", text));
    }
    if bursts.len() >= 4 {
        let mut gaps: Vec<i64> = bursts.windows(2).map(|w| w[1].0 - w[0].0).collect();
        gaps.sort_unstable();
        let median = gaps[(gaps.len() - 1) / 2];
        let near = gaps.iter().filter(|g| **g * 5 >= median * 4 && **g * 4 <= median * 5).count();
        let regular = near * 3 >= gaps.len() * 2;
        out.push(("cadence", format!("every ~{}{}", span(median), if regular { ", regular" } else { " (median), irregular" })));
    }
    let (first, last) = (times[0], *times.last().unwrap());
    if times.len() >= 10 && last - first >= 2 * HOUR {
        let mut per: HashMap<i64, i64> = HashMap::new();
        for t in times {
            *per.entry(t.div_euclid(HOUR)).or_default() += 1;
        }
        let hours = last.div_euclid(HOUR) - first.div_euclid(HOUR) + 1;
        let (lo, hi) = (per.values().min().unwrap(), per.values().max().unwrap());
        out.push(("hourly", format!("{lo}-{hi} an hour, in {} of {hours} hours", per.len())));
    }
    out
}

/// "45s", "5m", "3h", "2d".
fn span(us: i64) -> String {
    let s = us / 1_000_000;
    match s {
        ..90 => format!("{s}s"),
        90..5_400 => format!("{}m", (s + 30) / 60),
        5_400..172_800 => format!("{}h", (s + 1_800) / 3_600),
        _ => format!("{}d", (s + 43_200) / 86_400),
    }
}

// -- Message templates ----------------------------------------------------------

static MASK: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?x)
          (?P<uuid>[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})
        | (?P<email>[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+)
        | (?P<ip>\d{1,3}(?:\.\d{1,3}){3}(?::\d+)?)
        | (?P<ip6>(?:[0-9a-fA-F]{1,4}:){7}[0-9a-fA-F]{1,4}|(?:[0-9a-fA-F]{1,4}:){1,6}:[0-9a-fA-F]{1,4})
        | (?P<hex>0x[0-9a-fA-F]+|[0-9a-fA-F]{8,})
        | (?P<num>\d+(?:\.\d+)?)
        ",
    )
    .expect("mask")
});

/// A message with its variable parts masked, so lines that differ only in
/// ids, counts, addresses or quoted values share one template: quoted
/// strings "<str>", uuids <uuid>, emails <email>, IPv4/IPv6 addresses <ip>,
/// hex (0x..., or 8+ hex characters with a digit) <hex>, numbers <num>.
/// Words are never masked, and digits inside a word stay (utf8, h264, v2,
/// user5): "Login failed for user 5" and "... for user admin" are two
/// templates.
pub fn template(message: &str) -> String {
    let quoted = mask_quotes(message);
    let mut out = String::with_capacity(quoted.len());
    let mut at = 0;
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    for c in MASK.captures_iter(&quoted) {
        let m = c.get(0).expect("match");
        let (before, after) = (quoted[..m.start()].chars().next_back(), quoted[m.end()..].chars().next());
        let alone = !word(before) && !word(after);
        let text = m.as_str();
        let token = if c.name("uuid").is_some() {
            alone.then_some("<uuid>")
        } else if c.name("email").is_some() {
            Some("<email>")
        } else if c.name("ip").is_some() || c.name("ip6").is_some() {
            (alone && text.bytes().any(|b| b.is_ascii_digit())).then_some("<ip>")
        } else if c.name("hex").is_some() {
            (alone && text.bytes().any(|b| b.is_ascii_digit())).then_some("<hex>")
        } else {
            // A number after a letter is part of a word (utf8, v2); before
            // one it is a quantity with its unit (30s, 67.86pt).
            (!before.is_some_and(|ch| ch.is_alphanumeric() || ch == '_')).then_some("<num>")
        };
        out.push_str(&quoted[at..m.start()]);
        out.push_str(token.unwrap_or(text));
        at = m.end();
    }
    out.push_str(&quoted[at..]);
    out
}

/// "..." always, and '...' or `...' / `...` when the quote opens and
/// closes at a word's edge (so "can't ... don't" is not a quoted string),
/// become "<str>".
fn mask_quotes(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let word = |c: Option<&char>| c.is_some_and(|c| c.is_alphanumeric() || *c == '_');
    let mut i = 0;
    while i < chars.len() {
        let q = chars[i];
        if matches!(q, '"' | '\'' | '`') && (q == '"' || !word(i.checked_sub(1).and_then(|p| chars.get(p)))) {
            let closes = |c: char| if q == '`' { c == '`' || c == '\'' } else { c == q };
            if let Some(j) = (i + 1..chars.len()).find(|j| closes(chars[*j]))
                && (q == '"' || !word(chars.get(j + 1)))
            {
                out.push(q);
                out.push_str("<str>");
                out.push(chars[j]);
                i = j + 1;
                continue;
            }
        }
        out.push(q);
        i += 1;
    }
    out
}

// -- Rows ---------------------------------------------------------------------

/// Long deploy versions shortened to 12 characters, with the table that
/// maps them back.
#[derive(Default)]
pub struct Deploys {
    by_short: IndexMap<String, String>,
}

impl Deploys {
    /// `deploy` as shown: its first 12 characters when it is longer than 16
    /// and no other deploy here starts with them, else whole.
    pub fn short(&mut self, deploy: &str) -> String {
        if deploy.chars().count() <= 16 {
            return deploy.to_string();
        }
        let short = rb::first_chars(deploy, 12).to_string();
        match self.by_short.get(&short) {
            Some(full) if full != deploy => deploy.to_string(),
            Some(_) => short,
            None => {
                self.by_short.insert(short.clone(), deploy.to_string());
                short
            }
        }
    }

    pub fn into_json(self) -> Option<Value> {
        (!self.by_short.is_empty()).then(|| Value::Object(self.by_short.into_iter().map(|(k, v)| (k, Value::from(v))).collect()))
    }
}

/// Moves the scalar fields every map has with the same value (two maps at
/// least) into the returned map; `keep` are never moved.
pub fn hoist(maps: &mut [Map<String, Value>], keep: &[&str]) -> Map<String, Value> {
    let mut common = Map::new();
    if maps.len() < 2 {
        return common;
    }
    let candidates: Vec<(String, Value)> = maps[0]
        .iter()
        .filter(|(k, v)| !keep.contains(&k.as_str()) && !v.is_object() && !v.is_array() && !v.is_null())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (k, v) in candidates {
        if maps.iter().all(|e| e.get(&k) == Some(&v)) {
            for e in maps.iter_mut() {
                e.shift_remove(&k);
            }
            common.insert(k, v);
        }
    }
    common
}

/// The url template, of `templates` (`{key}` where the row's `key` goes),
/// that rebuilds the most rows' `url`; each of those rows loses its `url`,
/// every other row keeps its own. A null url stays (as null) on a row that
/// has a `key`, so the template is not read as its link, and goes on a row
/// without one. None (every url kept) when no template rebuilds any.
pub fn strip_urls(rows: &mut [Value], templates: &[String], key: &str) -> Option<String> {
    let placeholder = format!("{{{key}}}");
    let fill = |t: &str, r: &Value| r.get(key).and_then(Value::as_str).map(|id| t.replace(&placeholder, id));
    let rebuilds = |t: &str, r: &Value| r.get("url").and_then(Value::as_str).is_some_and(|u| fill(t, r).as_deref() == Some(u));
    let (best, n) = templates.iter().map(|t| (t, rows.iter().filter(|r| rebuilds(t, r)).count())).max_by_key(|(_, n)| *n)?;
    if n == 0 {
        return None;
    }
    for r in rows.iter_mut() {
        let drop = rebuilds(best, r) || (r.get("url").is_some_and(Value::is_null) && fill(best, r).is_none());
        if drop && let Value::Object(m) = r {
            m.shift_remove("url");
        }
    }
    Some(best.clone())
}

/// Rows on a diet: null fields removed (but `url`: see `strip_urls`), long
/// deploys shortened through `deploys`, fields equal on every row moved to
/// the returned `common` (ids, times and durations stay per row).
pub fn diet(rows: Vec<Value>, deploys: &mut Deploys) -> (Vec<Value>, Map<String, Value>) {
    let mut maps: Vec<Map<String, Value>> = rows
        .into_iter()
        .map(|r| match r {
            Value::Object(m) => m
                .into_iter()
                .filter(|(k, v)| !v.is_null() || k == "url")
                .map(|(k, v)| match v {
                    Value::String(d) if k == "deploy" => (k, Value::from(deploys.short(&d))),
                    v => (k, v),
                })
                .collect(),
            _ => Map::new(),
        })
        .collect();
    let common = hoist(&mut maps, &["execution_id", "id", "occurred_at", "duration", "message", "url"]);
    (maps.into_iter().map(Value::Object).collect(), common)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const S: i64 = 1_000_000;

    #[test]
    fn templates_mask_values_and_keep_words() {
        let t = template("Generating image variants for blob 123/456 (0x1f) at 10.0.0.1:443 by ada@example.com, request 3f2a9c1e7b6d");
        assert_eq!(t, "Generating image variants for blob <num>/<num> (<hex>) at <ip> by <email>, request <hex>");
        assert_eq!(template("job 6f1c2d3e-0a1b-4c5d-8e9f-001122334455 failed after 2.5s"), "job <uuid> failed after <num>s");
        assert_eq!(template("Please add `gem \"image_processing\"` to your Gemfile"), "Please add `<str>` to your Gemfile");
        assert_eq!(template("Please add 'image_processing' to your Gemfile"), "Please add '<str>' to your Gemfile");
        assert_eq!(template("undefined method `foo' for nil"), "undefined method `<str>' for nil");
        assert_eq!(template("from fe80::1 and 2001:db8::7, Net::OpenTimeout"), "from <ip> and <ip>, Net::OpenTimeout");
        assert_eq!(template("full 2001:0db8:0000:0000:0000:ff00:0042:8329 address"), "full <ip> address");
        // A clock time is numbers, not an IPv6 address.
        assert_eq!(template("Job ran at 12:30:45"), "Job ran at <num>:<num>:<num>");
        assert_eq!(template("utf8 h264 v2 user5 overflow=67.86pt columns=4"), "utf8 h264 v2 user5 overflow=<num>pt columns=<num>");
        assert_eq!(template("deadbeef and facade42 and decade"), "deadbeef and <hex> and decade");
        // Stable: the values differ, the template does not.
        assert_eq!(template("Retrying in 30s (attempt 2 of 5)"), template("Retrying in 5s (attempt 4 of 5)"));
        assert_eq!(template("Retrying in 30s (attempt 2 of 5)"), "Retrying in <num>s (attempt <num> of <num>)");
        assert_eq!(template(&template("blob 12 at 10.0.0.1")), template("blob 12 at 10.0.0.1"), "a template is its own template");
    }

    #[test]
    fn templates_keep_apart_messages_that_only_look_alike() {
        let pairs = [
            ("Login failed for user 5", "Login failed for user admin"),
            ("User 5 signed in", "User 5 signed out"),
            ("Payment failed for order 12", "Payment refused for order 12"),
            ("Can't find user 5, don't retry", "Can't find order 5, don't retry"),
            ("GET /widgets/12 took 30ms", "GET /gadgets/12 took 30ms"),
            ("cache miss: views/a", "cache miss: views/b"),
            ("token abc123 rejected", "token abd123 rejected"),
            ("Timed out after 30s", "Timed out after 30ms"),
        ];
        for (a, b) in pairs {
            assert_ne!(template(a), template(b), "{a} / {b}");
        }
        assert_eq!(template("Can't find user 5, don't retry"), "Can't find user <num>, don't retry");
    }

    fn sig(times: &[i64]) -> HashMap<&'static str, String> {
        signals(times).into_iter().collect()
    }

    #[test]
    fn bursts_and_cadence_read_a_client_retrying_on_a_timer() {
        // Pairs every 5 minutes, half a second apart.
        let times: Vec<i64> = (0..24).flat_map(|i| [i * 300 * S, i * 300 * S + S / 2]).collect();
        let s = sig(&times);
        assert_eq!(s["bursts"], "in pairs");
        assert_eq!(s["cadence"], "every ~5m, regular");
        assert!(!s.contains_key("hourly"), "48 requests over 1h55m");
        // The same pairs at irregular gaps.
        let mut t = 0;
        let times: Vec<i64> = [40, 900, 75, 300, 610, 20, 1_500, 95]
            .iter()
            .flat_map(|g| {
                t += g * S;
                [t, t + S]
            })
            .collect();
        let s = sig(&times);
        assert_eq!(s["bursts"], "in pairs");
        assert_eq!(s["cadence"], "every ~5m (median), irregular");
        // Alone, 10 minutes apart: no bursts.
        let times: Vec<i64> = (0..30).map(|i| i * 600 * S).collect();
        let s = sig(&times);
        assert!(!s.contains_key("bursts"));
        assert_eq!(s["cadence"], "every ~10m, regular");
        assert_eq!(s["hourly"], "6-6 an hour, in 5 of 5 hours");
        // Mostly alone, with two bursts.
        let mut times: Vec<i64> = (0..10).map(|i| i * 60 * S).collect();
        times.extend([540 * S + 1, 540 * S + 2, 300 * S + 5]);
        times.sort();
        assert_eq!(sig(&times)["bursts"], "2 bursts of 2-3, the rest alone");
        times.pop();
        assert_eq!(sig(&times)["bursts"], "2 pairs, the rest alone");
        // A burst is a chain: each within 2 s of the one before.
        let times: Vec<i64> = (0..6).map(|i| i * S * 3 / 2).collect();
        assert_eq!(signals(&times), vec![("bursts", "in bursts of ~6 (up to 6)".to_string())]);
        let times: Vec<i64> = (0..6).map(|i| i * S * 5 / 2).collect();
        assert!(!sig(&times).contains_key("bursts"), "2.5 s apart is not a burst");
        assert!(signals(&[S]).is_empty());
    }

    #[test]
    fn a_fold_counts_every_value_exactly_and_names_what_it_does_not_list() {
        let mut f = Fold::default();
        for i in 0..30 {
            f.add(i * S, Some(1000 * (i + 1)), Some(&format!("e{i}")));
            f.field("path", Some(&format!("/p{}", i % 13)), FIELD_CHARS, true);
            f.field("user", if i % 3 == 0 { Some("ada") } else { None }, FIELD_CHARS, true);
            f.field("method", Some("GET"), FIELD_CHARS, true);
        }
        let j = Value::Object(Entry::new("g".into(), Map::new(), &mut f, Map::new()).json);
        assert_eq!(j["count"], 30);
        assert_eq!(j["method"], "GET", "a field the same on every row is a plain field");
        let paths = j["varying"]["path"].as_object().unwrap();
        assert_eq!(paths.len(), VARYING_VALUES);
        assert_eq!(paths["/p0"], 3);
        let listed: i64 = paths.values().map(|v| v.as_i64().unwrap()).sum();
        assert_eq!(listed + j["not_listed"]["path"]["count"].as_i64().unwrap(), 30);
        assert_eq!(j["not_listed"]["path"]["values"], 3);
        assert_eq!(j["varying"]["user"], json!({"ada": 10}));
        assert_eq!(j["not_listed"]["user"], json!({"none": 20}));
        assert_eq!(j["sample_execution_id"], "e0");
        assert_eq!(j["slowest_execution_id"], "e29");
        assert_eq!(j["duration_ms"], json!({"p50": 15.0, "max": 30.0}));
        // Past TRACKED_VALUES distinct values the rest are counted, not listed.
        let mut f = Fold::default();
        for i in 0..TRACKED_VALUES + 5 {
            f.add(0, None, None);
            f.field("path", Some(&format!("/{i}")), FIELD_CHARS, true);
        }
        let j = Value::Object(Entry::new("g".into(), Map::new(), &mut f, Map::new()).json);
        assert_eq!(
            j["not_listed"]["path"],
            json!({"values": format!(">={}", TRACKED_VALUES - VARYING_VALUES + 1), "count": TRACKED_VALUES - VARYING_VALUES + 5})
        );
    }

    fn row<'a>(name: &'a str, status: Option<i64>, halted: Option<&'a str>, exception: Option<&'a str>, redirect_to: Option<&'a str>) -> Row<'a> {
        Row {
            at: 0,
            execution_id: "e",
            name,
            group_hash: "",
            method: None,
            status,
            duration: 0,
            user: None,
            tenant: None,
            exception,
            exception_preview: exception,
            ip: None,
            user_agent: None,
            path: None,
            country: None,
            rate_limited: false,
            rate_limit: None,
            halted,
            unpermitted: false,
            unpermitted_keys: None,
            redirect_to,
            response_size: None,
        }
    }

    /// The fast comparison groups exactly as the key does, on the values
    /// the key cuts or cleans too.
    #[test]
    fn the_same_key_test_agrees_with_the_key_itself() {
        let long = "x".repeat(FIELD_CHARS + 3);
        let long2 = format!("{}y", "x".repeat(FIELD_CHARS + 2));
        let rows = [
            row("GET /a", Some(401), None, None, None),
            row("GET /a", Some(401), Some(""), None, None),
            row("GET /a", Some(401), Some("authenticate"), None, None),
            row("GET /a", Some(403), Some("authenticate"), None, None),
            row("GET /a", None, None, None, None),
            row("GET /a", Some(302), Some("authenticate"), None, Some("https://x/sign_in")),
            row("GET /a", Some(302), Some("authenticate"), None, Some("https://x/home")),
            row("GET /a\u{1b}", Some(401), None, None, None),
            row("GET /a\u{7}", Some(401), None, None, None),
            row(&long, Some(401), None, None, None),
            row(&long2, Some(401), None, None, None),
            row("GET /a", Some(500), None, Some("ArgumentError"), None),
            row("GET /a", Some(500), None, Some("ArgumentError\u{202E}"), None),
        ];
        for a in &rows {
            let key = request_key(a);
            for b in &rows {
                assert_eq!(same_request_key(&key, b), key == request_key(b), "{:?} / {:?}", a.name, b.name);
            }
        }
    }

    #[test]
    fn rank_lists_the_biggest_and_counts_the_rest_by_key() {
        let entry = |n: i64, status: i64| {
            let mut f = Fold::default();
            for i in 0..n {
                f.add(i, None, Some("e"));
            }
            let mut key = Map::new();
            key.insert("status".into(), Value::from(status));
            Entry::new(format!("{n}-{status}"), key, &mut f, Map::new())
        };
        let all = || vec![entry(1, 404), entry(9, 401), entry(3, 404), entry(2, 200)];
        let (shown, more) = rank(all(), 2, None, &["status"]);
        assert_eq!(shown.iter().map(|e| e.count).collect::<Vec<_>>(), [9, 3]);
        assert_eq!(more.unwrap(), json!({"entries": 2, "count": 3, "by": {"status": {"values": 2, "top": {"200": 2, "404": 1}}}}));
        // A budget lists fewer, the first always.
        let (shown, more) = rank(all(), 10, Some(1), &["status"]);
        assert_eq!(shown.len(), 1);
        assert_eq!(more.unwrap()["count"], 6);
        assert!(rank(all(), 10, None, &[]).1.is_none());
    }

    #[test]
    fn a_diet_drops_nulls_and_urls_and_hoists_what_every_row_shares() {
        let mut d = Deploys::default();
        let rows = vec![
            json!({"execution_id": "a", "status": 401, "deploy": "65878919f7586318fd83a3472fa299d872e4dab9", "user_ref": null, "url": "https://x/a"}),
            json!({"execution_id": "b", "status": 401, "deploy": "b1fb283f96e64b5d897e6ae5a00ef7991f4ca16b", "user_ref": null, "url": "https://x/b"}),
        ];
        let mut rows = rows;
        assert_eq!(strip_urls(&mut rows, &["https://x/{execution_id}".to_string()], "execution_id").as_deref(), Some("https://x/{execution_id}"));
        let (rows, common) = diet(rows, &mut d);
        assert_eq!(Value::Object(common), json!({"status": 401}));
        assert_eq!(rows[0], json!({"execution_id": "a", "deploy": "65878919f758"}));
        assert_eq!(d.into_json().unwrap()["b1fb283f96e6"], "b1fb283f96e64b5d897e6ae5a00ef7991f4ca16b");
        // Two deploys that share 12 characters: the second stays whole.
        let mut d = Deploys::default();
        assert_eq!(d.short("aaaaaaaaaaaa1111111"), "aaaaaaaaaaaa");
        assert_eq!(d.short("aaaaaaaaaaaa2222222"), "aaaaaaaaaaaa2222222");
        assert_eq!(d.short("aaaaaaaaaaaa1111111"), "aaaaaaaaaaaa");
        assert_eq!(d.short("short"), "short");
        // One row: nothing is common.
        let (rows, common) = diet(vec![json!({"execution_id": "a", "status": 1})], &mut Deploys::default());
        assert!(common.is_empty() && rows[0]["status"] == 1);
        // The template most rows fit; the others keep their own url, a row
        // with an id but no link keeps its null, one without an id loses it.
        let t = |s: &str| s.to_string();
        let mut rows = vec![
            json!({"execution_id": "a", "url": "https://x/requests/a"}),
            json!({"execution_id": "b", "url": "https://x/requests/b"}),
            json!({"execution_id": "c", "url": "https://x/jobs/c"}),
            json!({"execution_id": "d", "url": null}),
            json!({"url": null}),
        ];
        let best = strip_urls(&mut rows, &[t("https://x/jobs/{execution_id}"), t("https://x/requests/{execution_id}")], "execution_id");
        assert_eq!(best.as_deref(), Some("https://x/requests/{execution_id}"));
        assert_eq!(
            rows,
            vec![
                json!({"execution_id": "a"}),
                json!({"execution_id": "b"}),
                json!({"execution_id": "c", "url": "https://x/jobs/c"}),
                json!({"execution_id": "d", "url": null}),
                json!({})
            ]
        );
        let (rows, _) = diet(rows, &mut Deploys::default());
        assert_eq!(rows[3], json!({"execution_id": "d", "url": null}), "a null url survives the diet");
        let mut none = vec![json!({"execution_id": "a", "url": "https://elsewhere/a"})];
        assert_eq!(strip_urls(&mut none, &[t("https://x/{execution_id}")], "execution_id"), None);
        assert_eq!(none[0]["url"], "https://elsewhere/a");
    }
}
