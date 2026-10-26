//! MCP results sized for an agent's context window rather than a page: the
//! summary that leads get_execution (where the time went, which queries
//! repeat, which N+1 to fix), collapsed framework frames for get_issue, and
//! the hard cap every tool result passes through.

use serde_json::{Map, Value, json};

use crate::dashboard::db::jf;
use crate::dashboard::rb;

/// The most bytes a tool result's text may have: about 15-20k tokens of
/// JSON, under the 25k-token limit Claude Code puts on one tool result, so a
/// client never cuts a result off mid-JSON.
pub const MAX_RESULT_BYTES: usize = 60_000;

/// Room the truncation marker itself needs.
const MARKER_BYTES: usize = 1_000;

/// One timeline row, with what the summary reads besides its JSON.
pub struct Entry {
    pub kind: &'static str,
    pub offset: f64,
    /// Milliseconds; None for record types without a duration.
    pub duration: Option<f64>,
    pub group_hash: Option<String>,
    /// Queries: the SQL, longer than the row's 120-character label.
    pub sql: Option<String>,
    pub source: Option<String>,
    pub row: Value,
}

/// One `n_plus_ones` row of the execution.
pub struct NPlusOne {
    pub group_hash: Option<String>,
    pub sql: String,
    pub source: Option<String>,
    /// The gem records an N+1 when a shape reaches its threshold, with that
    /// count; the timeline says how often the shape actually ran.
    pub count: i64,
}

pub struct Facts<'a> {
    pub duration_ms: f64,
    pub entries: &'a [Entry],
    pub n_plus_ones: &'a [NPlusOne],
    /// warn-or-worse log lines, already shaped.
    pub warnings: Vec<Value>,
    pub dropped_records: Option<i64>,
    pub has_profile: bool,
}

const TOP_REPEATED: usize = 10;
const TOP_N_PLUS_ONES: usize = 5;
const TOP_SLOWEST: usize = 10;
const TOP_EXCEPTIONS: usize = 5;

/// The time_ms buckets, in order. They overlap (SQL runs inside
/// transactions), so they do not add up to the duration; `tracked` is the
/// time covered by at least one of them.
pub const TIME_BUCKETS: [&str; 6] = ["sql", "transactions", "view", "external", "cache", "other"];

/// Where one execution's milliseconds went.
#[derive(Debug, Clone, Default)]
pub struct TimeSplit {
    pub duration: f64,
    /// TIME_BUCKETS order.
    pub buckets: [f64; 6],
    pub tracked: f64,
    pub untracked: f64,
    /// SQL milliseconds that ran while a transaction was open.
    pub sql_in_transactions: f64,
    pub transactions: usize,
}

impl TimeSplit {
    pub fn of(entries: &[Entry], duration: f64) -> TimeSplit {
        let mut buckets = [0.0; 6];
        for e in entries {
            if let Some(i) = time_bucket(e.kind).and_then(|b| TIME_BUCKETS.iter().position(|k| *k == b)) {
                buckets[i] += e.duration.unwrap_or(0.0);
            }
        }
        let tracked = covered_ms(entries.iter().filter(|e| time_bucket(e.kind).is_some()), duration);
        TimeSplit {
            duration,
            buckets,
            tracked,
            untracked: (duration - tracked).max(0.0),
            sql_in_transactions: sql_inside_transactions(entries),
            transactions: entries.iter().filter(|e| e.kind == "transaction").count(),
        }
    }

    /// Element-wise sum, for a split across many executions.
    pub fn add(&mut self, o: &TimeSplit) {
        self.duration += o.duration;
        for (a, b) in self.buckets.iter_mut().zip(o.buckets) {
            *a += b;
        }
        self.tracked += o.tracked;
        self.untracked += o.untracked;
        self.sql_in_transactions += o.sql_in_transactions;
        self.transactions += o.transactions;
    }

    pub fn sql(&self) -> f64 {
        self.buckets[0]
    }

    pub fn transaction_ms(&self) -> f64 {
        self.buckets[1]
    }

    /// `time_ms`: every number rounded to 0.1 ms, each divided by `n` (1
    /// for one execution, the sample size for an average).
    pub fn to_json(&self, n: f64) -> Value {
        let n = n.max(1.0);
        let mut time = Map::new();
        time.insert("duration".into(), jf(rb::round(self.duration / n, 1)));
        for (k, v) in TIME_BUCKETS.iter().zip(self.buckets) {
            time.insert((*k).into(), jf(rb::round(v / n, 1)));
        }
        time.insert("tracked".into(), jf(rb::round(self.tracked / n, 1)));
        time.insert("untracked".into(), jf(rb::round(self.untracked / n, 1)));
        Value::Object(time)
    }
}

/// Where an execution's time went, and what to look at first.
pub fn execution_summary(f: &Facts) -> Value {
    let total = f.duration_ms;
    let mut counts: Map<String, Value> = Map::new();
    for e in f.entries {
        let n = counts.get(e.kind).and_then(Value::as_i64).unwrap_or(0);
        counts.insert(e.kind.to_string(), Value::from(n + 1));
    }
    let split = TimeSplit::of(f.entries, total);

    let repeated = repeated_queries(f.entries);
    let n_plus_ones = n_plus_ones(f);
    let slowest = slowest(f.entries);
    let exceptions: Vec<Value> = f.entries.iter().filter(|e| e.kind == "exception").take(TOP_EXCEPTIONS).map(|e| e.row.clone()).collect();

    let notes = notes(f, &split, n_plus_ones.len());

    crate::obj! {
        "time_ms" => split.to_json(1.0),
        "counts" => Value::Object(counts),
        "distinct_query_shapes" => distinct_shapes(f.entries),
        "repeated_queries" => repeated,
        "n_plus_ones" => n_plus_ones,
        "slowest" => slowest,
        "exceptions" => exceptions,
        "warnings" => f.warnings.clone(),
        "notes" => notes,
    }
}

/// Which time_ms bucket a record type's duration counts in; None for types
/// whose duration is not time spent inside the execution (a profile spans
/// all of it).
fn time_bucket(kind: &str) -> Option<&'static str> {
    match kind {
        "query" => Some("sql"),
        "transaction" => Some("transactions"),
        "view_render" => Some("view"),
        "outgoing_request" | "llm_call" => Some("external"),
        "cache_event" => Some("cache"),
        "profile" | "exception" | "log" | "attachment" => None,
        _ => Some("other"),
    }
}

/// Milliseconds of [0, total] covered by at least one entry. The buckets
/// overlap (queries run inside transactions, partials inside templates), so
/// their sum would count the same time twice.
fn covered_ms<'a>(entries: impl Iterator<Item = &'a Entry>, total: f64) -> f64 {
    let mut spans: Vec<(f64, f64)> =
        entries.filter_map(|e| e.duration.filter(|d| *d > 0.0).map(|d| (e.offset.clamp(0.0, total), (e.offset + d).clamp(0.0, total)))).collect();
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut covered = 0.0;
    let mut current: Option<(f64, f64)> = None;
    for (start, end) in spans {
        match current {
            Some((s, e)) if start <= e => current = Some((s, e.max(end))),
            Some((s, e)) => {
                covered += e - s;
                current = Some((start, end));
            }
            None => current = Some((start, end)),
        }
    }
    if let Some((s, e)) = current {
        covered += e - s;
    }
    covered
}

/// SQL milliseconds that ran while a transaction was open.
fn sql_inside_transactions(entries: &[Entry]) -> f64 {
    let txs: Vec<(f64, f64)> = entries.iter().filter(|e| e.kind == "transaction").map(|e| (e.offset, e.offset + e.duration.unwrap_or(0.0))).collect();
    entries.iter().filter(|e| e.kind == "query" && txs.iter().any(|(s, end)| e.offset >= *s && e.offset <= *end)).map(|e| e.duration.unwrap_or(0.0)).sum()
}

fn query_key(e: &Entry) -> String {
    e.group_hash.clone().unwrap_or_else(|| e.row["label"].as_str().unwrap_or("").to_string())
}

fn distinct_shapes(entries: &[Entry]) -> usize {
    let mut seen: Vec<String> = entries.iter().filter(|e| e.kind == "query").map(query_key).collect();
    seen.sort();
    seen.dedup();
    seen.len()
}

/// Query shapes that ran more than once, most runs first: an N+1 or a
/// lookup that belongs outside a loop shows up here with its line.
fn repeated_queries(entries: &[Entry]) -> Vec<Value> {
    struct Group<'a> {
        key: String,
        first: &'a Entry,
        count: usize,
        total: f64,
        max: f64,
        sources: Vec<(&'a str, usize)>,
    }
    let mut groups: Vec<Group> = vec![];
    for e in entries.iter().filter(|e| e.kind == "query") {
        let key = query_key(e);
        let ix = match groups.iter().position(|g| g.key == key) {
            Some(i) => i,
            None => {
                groups.push(Group { key, first: e, count: 0, total: 0.0, max: 0.0, sources: vec![] });
                groups.len() - 1
            }
        };
        let g = &mut groups[ix];
        let d = e.duration.unwrap_or(0.0);
        g.count += 1;
        g.total += d;
        g.max = g.max.max(d);
        if let Some(s) = e.source.as_deref() {
            match g.sources.iter_mut().find(|(x, _)| *x == s) {
                Some(slot) => slot.1 += 1,
                None => g.sources.push((s, 1)),
            }
        }
    }
    groups.retain(|g| g.count > 1);
    // Most runs first; equal counts by time spent. A stable sort keeps
    // timeline order for exact ties.
    groups.sort_by(|a, b| b.count.cmp(&a.count).then(b.total.total_cmp(&a.total)));
    groups
        .into_iter()
        .take(TOP_REPEATED)
        .map(|g| {
            let mut sources = g.sources.clone();
            sources.sort_by_key(|s| std::cmp::Reverse(s.1));
            let mut v = crate::obj! {
                "count" => g.count, "total_ms" => jf(rb::round(g.total, 2)), "max_ms" => jf(rb::round(g.max, 3)),
                "source" => sources.first().map_or(Value::Null, |s| Value::from(s.0)),
                "sql" => rb::first_chars(g.first.sql.as_deref().unwrap_or(""), 200),
                "group_hash" => g.first.group_hash.clone(),
            };
            if sources.len() > 1 {
                crate::dashboard::db::merge_into(&mut v, "other_sources", Value::from(sources.len() - 1));
            }
            v
        })
        .collect()
}

/// The execution's N+1s with their fix, most repeated first.
fn n_plus_ones(f: &Facts) -> Vec<Value> {
    let mut rows: Vec<(usize, &NPlusOne)> = f
        .n_plus_ones
        .iter()
        .map(|n| {
            let ran = f.entries.iter().filter(|e| e.kind == "query" && e.group_hash.is_some() && e.group_hash == n.group_hash).count();
            (ran.max(n.count.max(0) as usize), n)
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    rows.into_iter()
        .take(TOP_N_PLUS_ONES)
        .map(|(repeats, n)| {
            let suggestion = crate::dashboard::pages::n_plus_one::suggestion(&n.sql, n.source.as_deref());
            crate::obj! {
                "source" => n.source.clone(), "repeats" => repeats, "sql" => rb::first_chars(&n.sql, 160),
                "fix" => suggestion.get("code").cloned().unwrap_or(Value::Null),
                "explanation" => suggestion.get("explanation").cloned().unwrap_or(Value::Null),
                "group_hash" => n.group_hash.clone(),
            }
        })
        .collect()
}

/// The longest individual records, of any type.
fn slowest(entries: &[Entry]) -> Vec<Value> {
    let mut timed: Vec<&Entry> = entries.iter().filter(|e| e.kind != "profile" && e.duration.is_some_and(|d| d > 0.0)).collect();
    timed.sort_by(|a, b| b.duration.unwrap_or(0.0).total_cmp(&a.duration.unwrap_or(0.0)));
    timed.into_iter().take(TOP_SLOWEST).map(|e| e.row.clone()).collect()
}

fn pct(part: f64, total: f64) -> i64 {
    if total <= 0.0 { 0 } else { (part * 100.0 / total).round() as i64 }
}

fn ms_s(v: f64) -> String {
    format!("{} ms", v.round() as i64)
}

/// The sentences about where the time is (transactions holding Ruby code,
/// untracked time, SQL-bound), and whether a profile would explain more.
/// `of` names what the split is of ("" for one execution, " per request"
/// for a route's average); `sql_next` says where to look when it is SQL.
pub fn time_notes(split: &TimeSplit, of: &str, sql_next: &str) -> (Vec<String>, bool) {
    let total = split.duration;
    let (sql, transactions, untracked) = (split.sql(), split.transaction_ms(), split.untracked);
    let mut out = vec![];
    let mut wants_profile = false;
    if total > 0.0 && transactions >= total * 0.5 {
        out.push(format!(
            "{}% of the time ({}{of}) is inside {} database transaction(s), but the SQL inside them took {}: application code runs while those transactions are open. Look at what the transaction blocks do besides SQL.",
            pct(transactions, total),
            ms_s(transactions),
            split.transactions,
            ms_s(split.sql_in_transactions)
        ));
        wants_profile = true;
    }
    if total > 0.0 && untracked >= total * 0.5 && untracked >= 50.0 {
        out.push(format!(
            "{}% of the time ({}{of}) is not covered by any recorded query, transaction, view render or outgoing call: it is Ruby code, GC or waiting.",
            pct(untracked, total),
            ms_s(untracked)
        ));
        wants_profile = true;
    }
    if total > 0.0 && sql >= total * 0.5 {
        out.push(format!("{}% of the time ({}{of}) is SQL: start with {sql_next}.", pct(sql, total), ms_s(sql)));
    }
    (out, wants_profile)
}

/// What the numbers mean, in sentences an agent can act on.
fn notes(f: &Facts, split: &TimeSplit, n_plus_ones: usize) -> Vec<String> {
    let (time, wants_profile) = time_notes(split, "", "repeated_queries and slowest");
    let mut out = vec![];
    let mut time = time.into_iter();
    // The SQL note goes after the profile pointer, the others before it.
    let sql_bound = split.duration > 0.0 && split.sql() >= split.duration * 0.5;
    let before = time.len() - usize::from(sql_bound);
    out.extend(time.by_ref().take(before));
    if wants_profile {
        out.push(if f.has_profile {
            "A profile exists for this execution: call get_profile with its execution_id to see which methods the time is in.".to_string()
        } else {
            format!("No profile was captured for it. {}", super::mcp::PROFILE_SETUP)
        });
    }
    out.extend(time);
    if n_plus_ones > 0 {
        out.push(format!(
            "{} N+1 pattern(s) detected; n_plus_ones lists the most repeated with the source line and an Active Record fix.",
            f.n_plus_ones.len().max(n_plus_ones)
        ));
    }
    if let Some(d) = f.dropped_records {
        out.push(format!(
            "The app dropped {d} child records after the first ones (its per-execution buffer was full): the timeline and these totals are incomplete."
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// get_issue: framework frames

/// The frames to show: the raising frame, every in-app frame, and each run
/// of two or more other frames collapsed to their count and gem names.
pub fn collapse_frames(frames: &Value) -> Value {
    let Value::Array(all) = frames else { return frames.clone() };
    let mut out: Vec<Value> = vec![];
    let mut run: Vec<&Value> = vec![];
    let flush = |run: &mut Vec<&Value>, out: &mut Vec<Value>| {
        if run.len() == 1 {
            out.push(run[0].clone());
        } else if run.len() > 1 {
            let mut gems: Vec<String> = vec![];
            for f in run.iter() {
                let g = library_of(f.get("file").and_then(Value::as_str).unwrap_or(""));
                if !gems.contains(&g) {
                    gems.push(g);
                }
            }
            out.push(json!({"omitted_frames": run.len(), "gems": gems}));
        }
        run.clear();
    };
    for (i, f) in all.iter().enumerate() {
        let in_app = f.get("in_app").and_then(Value::as_bool) == Some(true);
        if i == 0 || in_app || !f.is_object() {
            flush(&mut run, &mut out);
            out.push(f.clone());
        } else {
            run.push(f);
        }
    }
    flush(&mut run, &mut out);
    Value::Array(out)
}

/// The gem (or "ruby", or the npm package) a frame's file belongs to.
fn library_of(file: &str) -> String {
    if let Some(rest) = file.split("/gems/").nth(1).filter(|_| file.contains("/gems/")) {
        let dir = rest.split('/').next().unwrap_or(rest);
        // name-1.2.3 (or name-1.2.3-x86_64-linux): cut at the first "-<digit>".
        let cut = dir.char_indices().find(|(i, c)| *c == '-' && dir[i + 1..].starts_with(|d: char| d.is_ascii_digit())).map_or(dir.len(), |(i, _)| i);
        return dir[..cut].to_string();
    }
    if let Some(rest) = file.split("/node_modules/").last().filter(|_| file.contains("/node_modules/")) {
        let mut parts = rest.split('/');
        let first = parts.next().unwrap_or("");
        return if first.starts_with('@') { format!("{first}/{}", parts.next().unwrap_or("")) } else { first.to_string() };
    }
    if file.starts_with("<internal:") || file.contains("/lib/ruby/") {
        return "ruby".into();
    }
    "other".into()
}

// ---------------------------------------------------------------------------
// The cap

/// `text` (a tool's JSON) if it fits in `max` bytes. Otherwise the same JSON
/// cut down to fit (rows dropped from the end of the biggest arrays, then
/// very long strings shortened), wrapped as {"truncated": {...}, "result":
/// ...} with what was left out and `hint` on how to ask for less.
pub fn cap(text: String, max: usize, hint: &str) -> String {
    if text.len() <= max {
        return text;
    }
    let original = text.len();
    let Ok(mut v) = serde_json::from_str::<Value>(&text) else {
        let cut = rb::first_chars(&text, max.saturating_sub(MARKER_BYTES) / 4).to_string();
        return json!({"truncated": {"original_bytes": original, "hint": hint}, "result": cut}).to_string();
    };
    let budget = max.saturating_sub(MARKER_BYTES);
    let mut omitted: Vec<(String, usize)> = vec![];
    let mut clipped = clip_strings(&mut v, 4_000);
    while size(&v) > budget {
        let excess = size(&v) - budget;
        let Some(path) = largest_array(&v, String::new()).map(|(p, _)| p) else { break };
        let Some(arr) = at_path(&mut v, &path) else { break };
        let mut removed_bytes = 0;
        let mut removed = 0;
        while removed_bytes < excess
            && let Some(last) = arr.pop()
        {
            removed_bytes += size(&last) + 1;
            removed += 1;
        }
        let label = if path.is_empty() { "result".to_string() } else { path };
        match omitted.iter_mut().find(|(p, _)| *p == label) {
            Some(slot) => slot.1 += removed,
            None => omitted.push((label, removed)),
        }
    }
    if size(&v) > budget {
        clipped |= clip_strings(&mut v, 200);
    }
    let mut marker = Map::new();
    marker.insert("original_bytes".into(), Value::from(original));
    if !omitted.is_empty() {
        marker.insert("omitted_rows".into(), Value::Object(omitted.into_iter().map(|(p, n)| (p, Value::from(n))).collect()));
    }
    if clipped {
        marker.insert("shortened_strings".into(), Value::Bool(true));
    }
    marker.insert("hint".into(), Value::from(hint));
    json!({"truncated": marker, "result": v}).to_string()
}

fn size(v: &Value) -> usize {
    v.to_string().len()
}

/// Shortens every string longer than `max` characters; true when any was.
fn clip_strings(v: &mut Value, max: usize) -> bool {
    match v {
        Value::String(s) if s.chars().count() > max => {
            let more = s.chars().count() - max;
            *s = format!("{}... [{more} more characters]", rb::first_chars(s, max));
            true
        }
        Value::Array(a) => a.iter_mut().fold(false, |acc, x| clip_strings(x, max) | acc),
        Value::Object(o) => o.values_mut().fold(false, |acc, x| clip_strings(x, max) | acc),
        _ => false,
    }
}

/// The path ("timeline", "summary.slowest", "components[2].vitals") and
/// serialized size of the biggest non-empty array.
fn largest_array(v: &Value, path: String) -> Option<(String, usize)> {
    let mut best: Option<(String, usize)> = None;
    let mut consider = |c: Option<(String, usize)>| {
        if let Some(c) = c
            && best.as_ref().is_none_or(|b| c.1 > b.1)
        {
            best = Some(c);
        }
    };
    match v {
        Value::Array(a) => {
            if !a.is_empty() {
                consider(Some((path.clone(), size(v))));
            }
            for (i, x) in a.iter().enumerate() {
                consider(largest_array(x, format!("{path}[{i}]")));
            }
        }
        Value::Object(o) => {
            for (k, x) in o {
                let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                consider(largest_array(x, p));
            }
        }
        _ => {}
    }
    best
}

fn at_path<'a>(v: &'a mut Value, path: &str) -> Option<&'a mut Vec<Value>> {
    let mut cur = v;
    let mut rest = path;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('[') {
            let end = r.find(']')?;
            let i: usize = r[..end].parse().ok()?;
            cur = cur.get_mut(i)?;
            rest = r[end + 1..].strip_prefix('.').unwrap_or(&r[end + 1..]);
        } else {
            let end = rest.find(['.', '[']).unwrap_or(rest.len());
            cur = cur.get_mut(&rest[..end])?;
            rest = rest[end..].strip_prefix('.').unwrap_or(&rest[end..]);
        }
    }
    cur.as_array_mut()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: &'static str, offset: f64, duration: f64, group: &str, source: &str) -> Entry {
        Entry {
            kind,
            offset,
            duration: Some(duration),
            group_hash: (!group.is_empty()).then(|| group.to_string()),
            sql: Some(format!("SELECT {group}")),
            source: (!source.is_empty()).then(|| source.to_string()),
            row: json!({"type": kind, "offset": offset, "duration": duration, "label": format!("SELECT {group}"), "source": source}),
        }
    }

    /// The invoicing request's shape: most of 3.6 s inside commits, 84 ms
    /// of SQL, one lookup run 60 times from one line.
    #[test]
    fn the_summary_splits_time_and_names_the_repeated_line() {
        let mut entries = vec![entry("transaction", 100.0, 3000.0, "", "")];
        for i in 0..60 {
            entries.push(entry("query", 110.0 + i as f64 * 10.0, 1.0, "g_invoice", "app/services/documents.rb:511"));
        }
        entries.push(entry("query", 3500.0, 20.0, "g_other", "app/models/x.rb:1"));
        entries.push(entry("query", 3550.0, 4.0, "g_other", "app/models/y.rb:2"));
        let n1 = [NPlusOne {
            group_hash: Some("g_invoice".into()),
            sql: "SELECT \"car_invoices\".* FROM \"car_invoices\" WHERE \"car_invoices\".\"customer_id\" = ?".into(),
            source: Some("app/services/documents.rb:511".into()),
            count: 5,
        }];
        let s =
            execution_summary(&Facts { duration_ms: 3670.0, entries: &entries, n_plus_ones: &n1, warnings: vec![], dropped_records: None, has_profile: false });

        let t = &s["time_ms"];
        assert_eq!((t["sql"].as_f64(), t["transactions"].as_f64()), (Some(84.0), Some(3000.0)));
        // Covered: the transaction (3000) plus the two queries after it (24);
        // the 60 queries inside it are not counted twice.
        assert_eq!(t["tracked"].as_f64(), Some(3024.0));
        assert_eq!(t["untracked"].as_f64(), Some(646.0));
        assert_eq!(s["counts"], json!({"transaction": 1, "query": 62}));
        assert_eq!(s["distinct_query_shapes"], 2);

        let top = &s["repeated_queries"][0];
        assert_eq!((top["count"].as_i64(), top["source"].as_str(), top["total_ms"].as_f64()), (Some(60), Some("app/services/documents.rb:511"), Some(60.0)));
        assert_eq!((s["repeated_queries"][1]["count"].as_i64(), s["repeated_queries"][1]["other_sources"].as_i64()), (Some(2), Some(1)));

        // The N+1 reports how often the shape ran, not the gem's threshold.
        assert_eq!(s["n_plus_ones"][0]["repeats"], 60);
        assert_eq!(s["n_plus_ones"][0]["fix"], "Customer.includes(:car_invoices)");
        assert_eq!(s["slowest"][0]["type"], "transaction");

        let notes = s["notes"].as_array().unwrap().iter().map(|n| n.as_str().unwrap()).collect::<Vec<_>>().join("\n");
        assert!(notes.contains("inside 1 database transaction(s), but the SQL inside them took 60 ms"), "{notes}");
        assert!(notes.contains("RAILWATCH_PROFILE_SLOW_MS"), "no profile: names the setting\n{notes}");
        assert!(notes.contains("N+1"), "{notes}");
    }

    #[test]
    fn untracked_time_points_at_an_existing_profile() {
        let entries = vec![entry("query", 0.0, 5.0, "g", ""), entry("profile", 0.0, 1000.0, "", "")];
        let s = execution_summary(&Facts {
            duration_ms: 1000.0,
            entries: &entries,
            n_plus_ones: &[],
            warnings: vec![],
            dropped_records: Some(7),
            has_profile: true,
        });
        // A profile spans the whole execution; it is not tracked time.
        assert_eq!(s["time_ms"]["untracked"].as_f64(), Some(995.0));
        let notes = s["notes"].to_string();
        assert!(notes.contains("100% of the time (995 ms) is not covered"), "{notes}");
        assert!(notes.contains("call get_profile"), "{notes}");
        assert!(notes.contains("dropped 7 child records"), "{notes}");
        assert_eq!(s["repeated_queries"], json!([]));
    }

    #[test]
    fn framework_runs_collapse_and_in_app_frames_stay() {
        let f = |file: &str, in_app: bool| json!({"file": file, "line": 1, "function": "f", "in_app": in_app});
        let frames = json!([
            f("/usr/local/bundle/ruby/3.4.0/gems/cloudflare-email-0.4.2/lib/client.rb", false),
            f("/usr/local/bundle/ruby/3.4.0/gems/cloudflare-email-0.4.2/lib/queue.rb", false),
            f("app/services/consumer.rb", true),
            f("<internal:numeric>", false),
            f("app/jobs/feedback_job.rb", true),
            f("/usr/local/bundle/ruby/3.4.0/gems/activejob-8.1.4/lib/a.rb", false),
            f("/usr/local/bundle/ruby/3.4.0/gems/activesupport-8.1.4/lib/b.rb", false),
            f("/usr/local/bundle/ruby/3.4.0/gems/activerecord-tenanted-0.8.0/lib/c.rb", false),
            f("/usr/local/bundle/ruby/3.4.0/gems/activejob-8.1.4/lib/d.rb", false),
        ]);
        let out = collapse_frames(&frames);
        let a = out.as_array().unwrap();
        assert_eq!(a.len(), 6);
        // The raising frame is kept even though it is in a gem.
        assert_eq!(a[0]["file"], frames[0]["file"]);
        assert_eq!(a[1], frames[1], "a run of one stays a frame");
        assert_eq!((a[2]["file"].as_str(), a[4]["file"].as_str()), (Some("app/services/consumer.rb"), Some("app/jobs/feedback_job.rb")));
        assert_eq!(a[5], json!({"omitted_frames": 4, "gems": ["activejob", "activesupport", "activerecord-tenanted"]}));
        assert_eq!(library_of("https://x.test/node_modules/@inertiajs/core/dist/index.js"), "@inertiajs/core");
        assert_eq!(library_of("/usr/local/lib/ruby/3.4.0/net/http.rb"), "ruby");
    }

    #[test]
    fn cap_leaves_small_results_alone_and_cuts_big_ones_to_valid_json() {
        assert_eq!(cap("[1,2]".into(), 100, "h"), "[1,2]");
        let rows: Vec<Value> = (0..2_000).map(|i| json!({"id": i, "label": "x".repeat(60)})).collect();
        let big = json!({"name": "GET /x", "summary": {"notes": ["keep"]}, "timeline": rows}).to_string();
        let out = cap(big.clone(), 20_000, "Page with timeline_offset.");
        assert!(out.len() <= 20_000, "{}", out.len());
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["truncated"]["original_bytes"], big.len());
        assert_eq!(v["truncated"]["hint"], "Page with timeline_offset.");
        let kept = v["result"]["timeline"].as_array().unwrap().len();
        assert_eq!(v["truncated"]["omitted_rows"]["timeline"].as_u64().unwrap() as usize + kept, 2_000);
        assert_eq!(v["result"]["timeline"][0]["id"], 0, "rows are dropped from the end");
        assert_eq!(v["result"]["summary"]["notes"][0], "keep");

        // A top-level array, and one string too long for any cap.
        let arr = Value::Array((0..500).map(|i| json!({"i": i, "s": "y".repeat(100)})).collect()).to_string();
        let v: Value = serde_json::from_str(&cap(arr, 10_000, "h")).unwrap();
        assert!(v["truncated"]["omitted_rows"]["result"].as_u64().unwrap() > 400);
        let huge = json!({"body": "z".repeat(200_000)}).to_string();
        let out = cap(huge, 10_000, "h");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(out.len() <= 10_000 && v["truncated"]["shortened_strings"] == true);
    }
}
