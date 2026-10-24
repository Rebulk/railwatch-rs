//! The child-table pages: queries, exceptions, logs, spans, profiles,
//! view renders, transactions, deprecations, cache, mail, notifications,
//! broadcasts, outgoing requests, LLM calls, storage, visits.

use std::collections::HashMap;

use rusqlite::Connection;
use rusqlite::types::Value as Sql;
use serde_json::{Map, Value};

use super::*;
use crate::dashboard::cursor_page::{self, Order};
use crate::dashboard::db::{jf, jfo, ji, js, jt};
use crate::dashboard::execution_presenter::collapsed_capped;
use crate::dashboard::filter_query::{self, Resource, s};
use crate::dashboard::models::{self, dur};
use crate::dashboard::rb::{self, Num};
use crate::dashboard::rollup;
use crate::dashboard::window::step_seconds;

fn f(v: &Sql) -> f64 {
    match v {
        Sql::Integer(i) => *i as f64,
        Sql::Real(x) => *x,
        _ => 0.0,
    }
}

fn i(v: &Sql) -> i64 {
    match v {
        Sql::Integer(i) => *i,
        Sql::Real(x) => *x as i64,
        _ => 0,
    }
}

/// Ruby's `tally.sort_by { -count }` over strings: tally keeps first-seen
/// order and the keys are Integers, so MRI's uniform introsort orders ties.
fn tally_sorted(items: impl Iterator<Item = String>, first: Option<usize>) -> Vec<Value> {
    let mut order: Vec<String> = vec![];
    let mut counts: HashMap<String, i64> = HashMap::new();
    for it in items {
        let c = counts.entry(it.clone()).or_insert(0);
        if *c == 0 {
            order.push(it);
        }
        *c += 1;
    }
    let pairs: Vec<(String, i64)> = order
        .into_iter()
        .map(|k| {
            let c = counts[&k];
            (k, c)
        })
        .collect();
    let sorted = rb::sort_by_num(pairs, |(_, c)| Num::I(-c));
    sorted.into_iter().take(first.unwrap_or(usize::MAX)).map(|(k, c)| Value::Array(vec![Value::String(k), Value::from(c)])).collect()
}

// -- queries -----------------------------------------------------------------

pub fn queries_index(conn: &Connection, ctx: &PageContext, _control: &dyn ControlData) -> R {
    let (from, to) = ctx.range();
    let cond = filter_query::apply(conn, Resource::Queries, ctx.param("q"), from, to, &[])?;
    let page = cursor_page::call(
        conn,
        &ctx.cursor_key,
        "queries",
        "Railwatch::Telemetry::Query",
        models::QUERY_WITH_SQL,
        cond,
        ctx.param("cursor"),
        ctx.param("limit"),
        Order::Slowest,
        &ctx.cursor_context("queries"),
    )??;
    let (queries, summary) = grouped_and_summary(conn, ctx, "query", 200, true)?;
    Ok(crate::obj! {
        "queries" => queries,
        "n_plus_ones" => n_plus_ones(conn, from, to)?,
        "slowest" => page.rows.iter().map(models::query_row).collect::<Vec<_>>(),
        "pagination" => page.meta, "q" => ctx.param_s("q"),
        "summary" => summary,
        "sort" => sort_param(ctx, "count"), "dir" => dir_param(ctx),
    })
}

pub fn n_plus_ones(conn: &Connection, from: i64, to: i64) -> rusqlite::Result<Value> {
    let rows = db::query(
        conn,
        "SELECT \"n_plus_ones\".\"group_hash\", COUNT(*), MAX(sql), MAX(source), MAX(count) FROM \"n_plus_ones\" \
         WHERE \"n_plus_ones\".\"occurred_at\" BETWEEN ?1 AND ?2 GROUP BY \"n_plus_ones\".\"group_hash\"",
        rusqlite::params![qt(from), qt(to)],
    )?;
    let rows = rb::sort_by_num(rows, |r| Num::I(-i(r.at(1))));
    Ok(Value::Array(
        rows.iter()
            .take(50)
            .map(|r| {
                let sql = match r.at(2) {
                    Sql::Text(t) => Some(t.as_str()),
                    _ => None,
                };
                let src = match r.at(3) {
                    Sql::Text(t) => Some(t.as_str()),
                    _ => None,
                };
                crate::obj! {
                    "group_hash" => db::raw_json(r.at(0)), "occurrences" => db::raw_json(r.at(1)), "sql" => js(sql), "source" => js(src),
                    "max_count" => db::raw_json(r.at(4)), "suggestion" => n_plus_one::suggestion(sql.unwrap_or(""), src),
                }
            })
            .collect(),
    ))
}

pub fn queries_show(conn: &Connection, ctx: &PageContext, _control: &dyn ControlData) -> R {
    let g = ctx.param_s("id");
    let (from, to) = ctx.range();
    let rows = db::query(
        conn,
        &format!(
            "SELECT {} FROM \"queries\" WHERE \"queries\".\"group_hash\" = ?1 AND \"queries\".\"occurred_at\" BETWEEN ?2 AND ?3 \
             ORDER BY \"queries\".\"occurred_at\" DESC LIMIT 100",
            models::QUERY_WITH_SQL
        ),
        rusqlite::params![g, qt(from), qt(to)],
    )?;
    let summary = rollup::summarize_owned(&rollup::between_group(conn, "query", from, to, &g)?);
    let explain = db::query_one(
        conn,
        &format!(
            "SELECT {} FROM \"queries\" WHERE \"queries\".\"group_hash\" = ?1 AND \"queries\".\"occurred_at\" BETWEEN ?2 AND ?3 \
             AND \"queries\".\"explain\" IS NOT NULL ORDER BY \"queries\".\"occurred_at\" DESC LIMIT 1",
            models::QUERY_WITH_SQL
        ),
        rusqlite::params![g, qt(from), qt(to)],
    )?
    .map_or(Value::Null, |q| {
        crate::obj! { "plan" => js(q.s("explain")), "occurred_at" => jt(q.t("occurred_at")), "duration" => dur(&q, 3), "execution_id" => js(q.s("execution_id")) }
    });
    Ok(crate::obj! {
        "group_hash" => g.clone(),
        "sql" => rows.first().map_or(Value::Null, |r| js(r.s("resolved_sql"))),
        "summary" => summary.to_json(),
        "series" => series(conn, ctx, "query", Some(&g))?,
        "sources" => tally_sorted(rows.iter().filter_map(|r| r.s("source").map(str::to_string)), Some(10)),
        "callers" => tally_sorted(rows.iter().filter_map(|r| r.s("execution_preview").map(str::to_string)), Some(10)),
        "roles" => tally_sorted(rows.iter().filter_map(|r| r.s("role").map(str::to_string)), None),
        "explain" => explain,
        "samples" => rows.iter().take(50).map(models::query_row).collect::<Vec<_>>(),
    })
}

/// Port of Telemetry::NPlusOne#suggestion.
pub mod n_plus_one {
    use serde_json::Value;

    struct Re {
        s: Vec<char>,
        i: usize,
    }

    // A tiny matcher for the five fixed patterns in NPlusOne (case
    // insensitive, \s+ / \w+ / BIND), written out by hand to avoid a regex
    // dependency. Each returns the (table, column-prefix) captures.
    impl Re {
        fn new(s: &str) -> Re {
            Re { s: s.chars().collect(), i: 0 }
        }
        fn ws1(&mut self) -> bool {
            let st = self.i;
            while self.i < self.s.len() && self.s[self.i].is_whitespace() {
                self.i += 1;
            }
            self.i > st
        }
        fn ws0(&mut self) {
            while self.i < self.s.len() && self.s[self.i].is_whitespace() {
                self.i += 1;
            }
        }
        fn kw(&mut self, k: &str) -> bool {
            let n = k.chars().count();
            if self.i + n > self.s.len() {
                return false;
            }
            let got: String = self.s[self.i..self.i + n].iter().collect();
            if got.eq_ignore_ascii_case(k) {
                self.i += n;
                true
            } else {
                false
            }
        }
        fn word(&mut self) -> Option<String> {
            let st = self.i;
            while self.i < self.s.len() && (self.s[self.i].is_alphanumeric() || self.s[self.i] == '_') {
                self.i += 1;
            }
            (self.i > st).then(|| self.s[st..self.i].iter().collect())
        }
        /// (?:\w+\.)? then an identifier; returns the identifier.
        fn qualified(&mut self) -> Option<String> {
            let st = self.i;
            let w = self.word()?;
            if self.i < self.s.len() && self.s[self.i] == '.' {
                self.i += 1;
                if let Some(w2) = self.word() {
                    return Some(w2);
                }
                self.i = st;
                return self.word();
            }
            Some(w)
        }
        fn bind_val(&mut self) -> bool {
            if self.i < self.s.len() && self.s[self.i] == '?' {
                self.i += 1;
                return true;
            }
            let st = self.i;
            if self.i < self.s.len() && self.s[self.i] == '-' {
                self.i += 1;
            }
            let d = self.i;
            while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
                self.i += 1;
            }
            if self.i == d {
                self.i = st;
                return false;
            }
            true
        }
        /// BIND = (?:=\s*(?:\?|-?\d+)|IN\s*\(\s*(?:\?|-?\d+)\s*\))
        fn bind(&mut self) -> bool {
            let st = self.i;
            if self.i < self.s.len() && self.s[self.i] == '=' {
                self.i += 1;
                self.ws0();
                if self.bind_val() {
                    return true;
                }
            }
            self.i = st;
            if self.kw("IN") {
                self.ws0();
                if self.i < self.s.len() && self.s[self.i] == '(' {
                    self.i += 1;
                    self.ws0();
                    if self.bind_val() {
                        self.ws0();
                        if self.i < self.s.len() && self.s[self.i] == ')' {
                            self.i += 1;
                            return true;
                        }
                    }
                }
            }
            self.i = st;
            false
        }
        /// (?:\w+\.)?(\w+)_<suffix>\s*BIND : the captured prefix before "_suffix".
        fn col_bind(&mut self, suffix: &str) -> Option<String> {
            let id = self.qualified()?;
            let p = id.strip_suffix(&format!("_{suffix}"))?.to_string();
            if p.is_empty() {
                return None;
            }
            self.ws0();
            self.bind().then_some(p)
        }
        fn star(&mut self) -> bool {
            let st = self.i;
            if let Some(_w) = self.word() {
                if self.i < self.s.len() && self.s[self.i] == '.' {
                    self.i += 1;
                } else {
                    self.i = st;
                }
            }
            if self.i < self.s.len() && self.s[self.i] == '*' {
                self.i += 1;
                return true;
            }
            self.i = st;
            false
        }
    }

    fn select_star_from_where(s: &str) -> Option<(Re, String)> {
        let mut r = Re::new(s);
        (r.kw("SELECT") && r.ws1() && r.star() && r.ws1() && r.kw("FROM") && r.ws1()).then_some(())?;
        let table = r.word()?;
        (r.ws1() && r.kw("WHERE") && r.ws1()).then_some(())?;
        Some((r, table))
    }

    fn polymorphic(s: &str) -> Option<(String, String)> {
        let (mut r, table) = select_star_from_where(s)?;
        let as_ = r.col_bind("id")?;
        (r.ws1() && r.kw("AND") && r.ws1()).then_some(())?;
        let ty = r.qualified()?;
        if ty != format!("{as_}_type") {
            return None;
        }
        r.ws0();
        r.bind().then_some((table, as_))
    }

    fn child(s: &str) -> Option<(String, String)> {
        let (mut r, table) = select_star_from_where(s)?;
        let p = r.col_bind("id")?;
        Some((table, p))
    }

    fn parent(s: &str) -> Option<String> {
        let (mut r, table) = select_star_from_where(s)?;
        let id = r.qualified()?;
        if id != "id" {
            return None;
        }
        r.ws0();
        r.bind().then_some(table)
    }

    fn count(s: &str) -> Option<(String, String)> {
        let mut r = Re::new(s);
        (r.kw("SELECT") && r.ws1() && r.kw("COUNT(*)") && r.ws1()).then_some(())?;
        let st = r.i;
        if !(r.kw("AS") && r.ws1() && r.word().is_some() && r.ws1()) {
            r.i = st;
        }
        (r.kw("FROM") && r.ws1()).then_some(())?;
        let table = r.word()?;
        (r.ws1() && r.kw("WHERE") && r.ws1()).then_some(())?;
        Some((table, r.col_bind("id")?))
    }

    fn exists(s: &str) -> Option<(String, String)> {
        // /\ASELECT\s+(?:1\s+AS\s+one|EXISTS\s*\(\s*SELECT\s+1)\b.*?\sFROM\s+(\w+)\s+WHERE\s+(?:\w+\.)?(\w+)_id\s*BIND/i
        let mut r = Re::new(s);
        (r.kw("SELECT") && r.ws1()).then_some(())?;
        let st = r.i;
        let ok = (r.kw("1") && r.ws1() && r.kw("AS") && r.ws1() && r.kw("one")) || {
            r.i = st;
            r.kw("EXISTS")
                && {
                    r.ws0();
                    r.kw("(")
                }
                && {
                    r.ws0();
                    r.kw("SELECT")
                }
                && r.ws1()
                && r.kw("1")
        };
        if !ok {
            return None;
        }
        if r.i < r.s.len() && (r.s[r.i].is_alphanumeric() || r.s[r.i] == '_') {
            return None; // \b
        }
        // lazy .*? then \sFROM: try each position.
        let rest_start = r.i;
        for k in rest_start..r.s.len() {
            if !r.s[k].is_whitespace() {
                continue;
            }
            let mut t = Re { s: r.s.clone(), i: k + 1 };
            if !(t.kw("FROM") && t.ws1()) {
                continue;
            }
            let Some(table) = t.word() else { continue };
            if !(t.ws1() && t.kw("WHERE") && t.ws1()) {
                continue;
            }
            if let Some(p) = t.col_bind("id") {
                return Some((table, p));
            }
        }
        None
    }

    /// ActiveSupport classify (singularize + camelize) for snake_case words,
    /// with the common English plural rules.
    pub fn classify(s: &str) -> String {
        camelize(&singularize(s))
    }

    fn camelize(s: &str) -> String {
        s.split('_')
            .map(|w| {
                let mut c = w.chars();
                c.next().map_or(String::new(), |f| f.to_uppercase().collect::<String>() + c.as_str())
            })
            .collect()
    }

    pub fn singularize(s: &str) -> String {
        let l = s.to_ascii_lowercase();
        for (suf, rep) in [("ies", "y"), ("sses", "ss"), ("shes", "sh"), ("ches", "ch"), ("xes", "x"), ("ss", "ss"), ("s", "")] {
            if l.ends_with(suf) && l.len() > suf.len() {
                return format!("{}{rep}", &s[..s.len() - suf.len()]);
            }
        }
        s.to_string()
    }

    fn tableize(class: &str) -> String {
        let mut out = String::new();
        for (i, c) in class.chars().enumerate() {
            if c.is_uppercase() && i > 0 {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        }
        pluralize(&out)
    }

    fn pluralize(s: &str) -> String {
        if s.ends_with('y') && !s.ends_with("ay") && !s.ends_with("ey") && !s.ends_with("oy") {
            format!("{}ies", &s[..s.len() - 1])
        } else if s.ends_with('s') || s.ends_with('x') || s.ends_with("ch") || s.ends_with("sh") {
            format!("{s}es")
        } else {
            format!("{s}s")
        }
    }

    pub fn suggestion(sql: &str, source: Option<&str>) -> Value {
        let shape: String = {
            let t: String = sql.chars().filter(|c| *c != '"' && *c != '`').collect();
            let mut out = String::new();
            let mut prev_space = false;
            for c in t.chars() {
                if c == ' ' {
                    if !prev_space {
                        out.push(c);
                    }
                    prev_space = true;
                } else {
                    out.push(c);
                    prev_space = false;
                }
            }
            out.trim().to_string()
        };
        let at = match source {
            Some(s) if crate::dashboard::rb::present(Some(s)) => format!(" at {s}"),
            _ => String::new(),
        };
        if let Some((table, as_)) = polymorphic(&shape) {
            return crate::obj! {
                "kind" => "polymorphic", "parent" => Value::Null, "association" => table.clone(), "code" => format!(".includes(:{table})"),
                "explanation" => format!("Each owner loads its {table} in a separate query. {table} is polymorphic (belongs_to :{as_}), so add .includes(:{table}) on the collection you iterate{at}."),
            };
        }
        if let Some((table, m2)) = count(&shape) {
            let parent = classify(&m2);
            return crate::obj! {
                "kind" => "counter_cache", "parent" => parent.clone(), "association" => table.clone(),
                "code" => format!("# app/models/{}.rb\nbelongs_to :{m2}, counter_cache: true\n\n# then read\n{m2}.{table}_count", singularize(&table)),
                "explanation" => format!("Each {parent} counts its {table} with its own COUNT query{at}. A counter cache keeps the total on {}.{table}_count, so {m2}.{table}_count needs no query.", tableize(&parent)),
            };
        }
        if let Some((table, m2)) = exists(&shape) {
            let parent = classify(&m2);
            return crate::obj! {
                "kind" => "exists", "parent" => parent.clone(), "association" => table.clone(),
                "code" => format!("{parent}.includes(:{table})\n\n# then\n{m2}.{table}.any?"),
                "explanation" => format!("Each {parent} checks whether it has {table} with its own query{at}. Preload with {parent}.includes(:{table}) and call .any? on the loaded collection, or add a counter cache."),
            };
        }
        if let Some((table, m2)) = child(&shape) {
            let parent = classify(&m2);
            return crate::obj! {
                "kind" => "has_many", "parent" => parent.clone(), "association" => table.clone(), "code" => format!("{parent}.includes(:{table})"),
                "explanation" => format!("Each {parent} loads its {table} in a separate query{at}. Preload them with {parent}.includes(:{table}) where the {} are loaded.", tableize(&parent)),
            };
        }
        if let Some(table) = parent(&shape) {
            let assoc = singularize(&table);
            return crate::obj! {
                "kind" => "belongs_to", "parent" => Value::Null, "association" => assoc.clone(), "code" => format!(".includes(:{assoc})"),
                "explanation" => format!("Each row loads its {assoc} one at a time. Add .includes(:{assoc}) on the collection you iterate{at}."),
            };
        }
        Value::Null
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn shapes() {
            assert_eq!(suggestion("SELECT \"comments\".* FROM \"comments\" WHERE \"comments\".\"post_id\" = ?", Some("app/x.rb:3"))["kind"], "has_many");
            assert_eq!(suggestion("SELECT \"gadgets\".* FROM \"gadgets\" WHERE \"gadgets\".\"id\" = ? LIMIT ?", None)["association"], "gadget");
            assert_eq!(suggestion("SELECT COUNT(*) FROM \"comments\" WHERE \"comments\".\"post_id\" = ?", None)["parent"], "Post");
            assert_eq!(
                suggestion("SELECT \"images\".* FROM \"images\" WHERE \"images\".\"imageable_id\" = ? AND \"images\".\"imageable_type\" = ?", None)["kind"],
                "polymorphic"
            );
            assert_eq!(suggestion("SELECT 1 AS one FROM \"comments\" WHERE \"comments\".\"post_id\" = ? LIMIT ?", None)["kind"], "exists");
            assert!(suggestion("UPDATE x SET y = 1", None).is_null());
        }
    }
}

// -- exceptions --------------------------------------------------------------

pub fn exceptions_index(conn: &Connection, ctx: &PageContext, control: &dyn ControlData) -> R {
    let (from, to) = ctx.range();
    let cond = filter_query::apply(conn, Resource::Exceptions, ctx.param("q"), from, to, &[])?;
    let page = cursor_page::call(
        conn,
        &ctx.cursor_key,
        "exceptions",
        "Railwatch::Telemetry::Exception",
        "\"exceptions\".*",
        cond,
        ctx.param("cursor"),
        ctx.param("limit"),
        Order::Recent,
        &ctx.cursor_context("exceptions"),
    )??;
    let issues = control.issues();
    let by_group: HashMap<&str, &control::Issue> = issues.iter().map(|i| (i.group_hash.as_str(), i)).collect();
    let rows: Vec<Value> = page
        .rows
        .iter()
        .map(|e| {
            let issue = e.s("group_hash").and_then(|g| by_group.get(g));
            db::merge(
                models::exception_row(e),
                crate::obj! { "issue_id" => issue.map_or(Value::Null, |i| Value::from(i.id)), "issue_key" => issue.map_or(Value::Null, |i| Value::from(i.key.clone())) },
            )
        })
        .collect();
    let classes = db::query(
        conn,
        "SELECT COUNT(*) AS \"count_all\", \"exceptions\".\"class_name\" AS \"exceptions_class_name\" FROM \"exceptions\" \
         WHERE \"exceptions\".\"occurred_at\" BETWEEN ?1 AND ?2 GROUP BY \"exceptions\".\"class_name\"",
        rusqlite::params![qt(from), qt(to)],
    )?;
    let classes = rb::sort_by_num(classes, |r| Num::I(-i(r.at(0))));
    let classes: Vec<Value> = classes.iter().take(15).map(|r| Value::Array(vec![db::raw_json(r.at(1)), db::raw_json(r.at(0))])).collect();
    let cnt = |extra: &str| {
        db::count(
            conn,
            &format!("SELECT COUNT(*) FROM \"exceptions\" WHERE \"exceptions\".\"occurred_at\" BETWEEN ?1 AND ?2{extra}"),
            rusqlite::params![qt(from), qt(to)],
        )
    };
    Ok(crate::obj! {
        "exceptions" => rows, "pagination" => page.meta, "classes" => classes,
        "series" => exception_series(conn, ctx)?, "total" => cnt("")?,
        "unhandled" => cnt(" AND \"exceptions\".\"handled\" = 0")?, "q" => ctx.param_s("q"),
    })
}

fn exception_series(conn: &Connection, ctx: &PageContext) -> rusqlite::Result<Value> {
    let (from, to) = ctx.range();
    let step = step_seconds(ctx.step).unwrap();
    let bucket = aggregations::bucket_sql("occurred_at", step);
    let rows = db::query(
        conn,
        &format!(
            "SELECT COUNT(*), {bucket}, \"exceptions\".\"handled\" FROM \"exceptions\" WHERE \"exceptions\".\"occurred_at\" BETWEEN ?1 AND ?2 \
             GROUP BY {bucket}, \"exceptions\".\"handled\""
        ),
        rusqlite::params![qt(from), qt(to)],
    )?;
    let mut order: Vec<i64> = vec![];
    let mut buckets: HashMap<i64, (i64, i64)> = HashMap::new();
    for r in &rows {
        let b = i(r.at(1));
        let c = i(r.at(0));
        let handled = i(r.at(2)) != 0;
        let e = buckets.entry(b).or_insert_with(|| {
            order.push(b);
            (0, 0)
        });
        e.0 += c;
        if !handled {
            e.1 += c;
        }
    }
    let points: Vec<aggregations::Point> = order
        .iter()
        .map(|b| {
            let (c, e) = buckets[b];
            aggregations::Point {
                t: *b,
                json: crate::obj! {
                    "t" => rb::t_json(b * 1_000_000), "count" => c, "errors" => e, "client_errors" => 0,
                    "avg" => Value::Null, "p50" => Value::Null, "p95" => Value::Null, "p99" => Value::Null,
                },
            }
        })
        .collect();
    Ok(Value::Array(aggregations::fill(points, from, to, step, aggregations::empty_point)))
}

// -- logs --------------------------------------------------------------------

pub fn logs_index(conn: &Connection, ctx: &PageContext, _control: &dyn ControlData) -> R {
    let (from, to) = ctx.range();
    let cond = filter_query::apply(conn, Resource::Logs, ctx.param("q"), from, to, &[])?;
    let page = cursor_page::call(
        conn,
        &ctx.cursor_key,
        "logs",
        "Railwatch::Telemetry::Log",
        "\"logs\".*",
        cond,
        ctx.param("cursor"),
        ctx.param("limit"),
        Order::Recent,
        &ctx.cursor_context("logs"),
    )??;
    let ids: Vec<i64> = page.rows.iter().filter_map(|r| r.i("id")).collect();
    let highlights = filter_query::fts_snippets(conn, &ids, &filter_query::parse(ctx.param("q")).text)?;
    let counts_cond = filter_query::apply(conn, Resource::Logs, ctx.param("q"), from, to, &["level", "status"])?;
    let counts = match log_level_counts_per_file(conn, &counts_cond)? {
        Some(rows) => rows,
        None => select(
            conn,
            "logs",
            "COUNT(*) AS \"count_all\", \"logs\".\"level\" AS \"logs_level\"",
            &{
                let mut c = counts_cond;
                c.parts.push("1=1 GROUP BY \"logs\".\"level\"".into());
                c
            },
            "",
            None,
        )?,
    };
    let mut cm = Map::new();
    for r in &counts {
        cm.insert(db::raw_json(r.at(1)).as_str().map_or("".into(), str::to_string), db::raw_json(r.at(0)));
    }
    Ok(crate::obj! {
        "logs" => page.rows.iter().map(models::log_row).collect::<Vec<_>>(),
        "pagination" => page.meta, "counts" => Value::Object(cm), "highlights" => Value::Object(highlights), "q" => ctx.param_s("q"),
    })
}

/// logs#index's `group(:level).count` over a per-day tenant: each day file
/// counts its own rows (on its own indexes, FTS paired to its own index) and
/// the counts are summed per level. Through the views the GROUP BY ran over
/// the union's rows: logs.fts@7d's counts 140 -> 40 ms. Levels come back
/// sorted, as the single file's GROUP BY (a sorted temp b-tree or the
/// level index) returns them, and `counts` is a hash keyed in that order.
/// None on a single file.
fn log_level_counts_per_file(conn: &Connection, cond: &filter_query::Cond) -> rusqlite::Result<Option<Vec<db::Rec>>> {
    let (Some(range), false) = (cond.range, cond.none) else { return Ok(None) };
    if !crate::storage::reader::is_union(conn) || cond.from.is_some() {
        return Ok(None);
    }
    let per_file = crate::storage::reader::per_file(conn, "logs", range, &cond.sql(), &cond.binds, |f, _, cond_sql| {
        format!("SELECT COUNT(*) AS n, \"logs\".\"level\" AS level FROM {}.\"logs\" AS \"logs\" WHERE {cond_sql} GROUP BY \"logs\".\"level\"", f.alias)
    })?;
    let Some((union, binds)) = per_file else { return Ok(None) };
    let sql = format!("SELECT SUM(n) AS \"count_all\", level AS \"logs_level\" FROM ({union}) GROUP BY level ORDER BY level");
    Ok(Some(db::query_as_is(conn, &sql, rusqlite::params_from_iter(binds.iter()))?))
}

// -- spans -------------------------------------------------------------------

pub fn spans_index(conn: &Connection, ctx: &PageContext, _control: &dyn ControlData) -> R {
    let (mut spans, summary) = grouped_and_summary(conn, ctx, "span", 200, true)?;
    let p = filter_query::parse(ctx.param("q"));
    if let Some(n) = p.field("name") {
        spans.retain(|s| s["name"].as_str().unwrap_or("").contains(n));
    }
    if p.fields.get("status").map(String::as_str) == Some("failed") {
        spans.retain(|s| s["errors"].as_i64().unwrap_or(0) > 0);
    }
    Ok(crate::obj! {
        "spans" => spans, "series" => series(conn, ctx, "span", None)?, "summary" => summary,
        "sort" => sort_param(ctx, "count"), "dir" => dir_param(ctx), "q" => ctx.param_s("q"),
    })
}

fn value_to_s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Array(_) | Value::Object(_) => ruby_inspect(v),
        other => other.to_string(),
    }
}

/// `Hash#to_s`/`Array#to_s` (Ruby 3.4+ inspect: `{"k" => 1}`).
fn ruby_inspect(v: &Value) -> String {
    match v {
        Value::String(s) => serde_json::to_string(s).unwrap(),
        Value::Null => "nil".into(),
        Value::Array(a) => format!("[{}]", a.iter().map(ruby_inspect).collect::<Vec<_>>().join(", ")),
        Value::Object(m) => {
            if m.is_empty() {
                "{}".into()
            } else {
                format!("{{{}}}", m.iter().map(|(k, v)| format!("{} => {}", serde_json::to_string(k).unwrap(), ruby_inspect(v))).collect::<Vec<_>>().join(", "))
            }
        }
        other => other.to_string(),
    }
}

pub fn spans_show(conn: &Connection, ctx: &PageContext, _control: &dyn ControlData) -> R {
    let g = ctx.param_s("id");
    let (from, to) = ctx.range();
    let rows = db::query(
        conn,
        "SELECT \"spans\".* FROM \"spans\" WHERE \"spans\".\"group_hash\" = ?1 AND \"spans\".\"occurred_at\" BETWEEN ?2 AND ?3 \
         ORDER BY \"spans\".\"occurred_at\" DESC LIMIT 200",
        rusqlite::params![g, qt(from), qt(to)],
    )?;
    let name = match rows.first().and_then(|r| r.s("name")) {
        Some(n) => Value::String(n.to_string()),
        None => super::executions::rollup_name(conn, "span", &g)?,
    };
    // attribute_facets
    let mut keys: Vec<String> = vec![];
    let mut pairs: HashMap<String, Vec<String>> = HashMap::new();
    for r in &rows {
        if let Value::Object(m) = r.json("payload") {
            for (k, v) in m {
                if !pairs.contains_key(&k) {
                    keys.push(k.clone());
                }
                pairs.entry(k).or_default().push(value_to_s(&v));
            }
        }
    }
    let facets: Vec<(String, usize, Vec<Value>)> = keys
        .into_iter()
        .map(|k| {
            let vs = pairs.remove(&k).unwrap();
            let n = vs.len();
            (k, n, tally_sorted(vs.into_iter(), Some(5)))
        })
        .collect();
    let facets = rb::sort_by_num(facets, |(_, n, _)| Num::I(-(*n as i64)));
    let facets: Vec<Value> = facets.into_iter().take(10).map(|(k, n, v)| crate::obj! { "key" => k, "count" => n, "values" => v }).collect();
    Ok(crate::obj! {
        "group_hash" => g.clone(), "name" => name,
        "summary" => summary_with_delta(conn, ctx, "span", Some(&g))?, "series" => series(conn, ctx, "span", Some(&g))?,
        "facets" => facets,
        "samples" => rows.iter().take(50).map(|s| crate::obj! {
            "id" => ji(s.i("id")), "name" => js(s.s("name")), "duration" => dur(s, 3), "status" => js(s.s("status")),
            "attributes" => s.json("payload"), "occurred_at" => jt(s.t("occurred_at")), "execution_id" => js(s.s("execution_id")),
            "execution_preview" => js(s.s("execution_preview")),
        }).collect::<Vec<_>>(),
    })
}

// -- profiles ----------------------------------------------------------------

pub const PROFILE_SCAN_LIMIT: usize = 5_000;

pub fn profiles_index(conn: &Connection, ctx: &PageContext, _control: &dyn ControlData) -> R {
    let (from, to) = ctx.range();
    let range = rusqlite::params![qt(from), qt(to)];
    let rows = db::query(
        conn,
        "SELECT \"profiles\".\"id\", \"profiles\".\"group_hash\", \"profiles\".\"execution_preview\", \"profiles\".\"duration\", \"profiles\".\"samples\", \
         \"profiles\".\"occurred_at\" FROM \"profiles\" WHERE \"profiles\".\"occurred_at\" BETWEEN ?1 AND ?2 ORDER BY \"profiles\".\"occurred_at\" DESC LIMIT 5000",
        range,
    )?;
    let count = db::count(conn, "SELECT COUNT(*) FROM \"profiles\" WHERE \"profiles\".\"occurred_at\" BETWEEN ?1 AND ?2", range)?;
    let avg = db::query_one(conn, "SELECT AVG(\"profiles\".\"samples\") FROM \"profiles\" WHERE \"profiles\".\"occurred_at\" BETWEEN ?1 AND ?2", range)?
        .map_or(0.0, |r| f(r.at(0)));
    let profiled =
        db::count(conn, "SELECT COUNT(DISTINCT \"profiles\".\"execution_id\") FROM \"profiles\" WHERE \"profiles\".\"occurred_at\" BETWEEN ?1 AND ?2", range)?;
    let executions = executions_in(conn, from, to)?;
    // groups
    let mut order: Vec<Option<String>> = vec![];
    let mut groups: HashMap<Option<String>, Vec<&db::Rec>> = HashMap::new();
    for r in &rows {
        let g = r.s("group_hash").map(str::to_string);
        if !groups.contains_key(&g) {
            order.push(g.clone());
        }
        groups.entry(g).or_default().push(r);
    }
    let gs: Vec<(Option<String>, Vec<&db::Rec>)> = order
        .into_iter()
        .map(|g| {
            let v = groups.remove(&g).unwrap();
            (g, v)
        })
        .collect();
    let gs = rb::sort_by_num(gs, |(_, v)| Num::I(-(v.len() as i64)));
    let groups: Vec<Value> = gs
        .iter()
        .take(100)
        .map(|(g, v)| {
            let newest = v[0];
            let sum: i64 = v.iter().map(|r| r.i("duration").unwrap_or(0)).sum();
            crate::obj! {
                "group_hash" => js(g.as_deref()), "name" => js(newest.s("execution_preview")), "profile_id" => ji(newest.i("id")),
                "count" => v.len(), "avg_duration" => jf(rb::round(sum as f64 / v.len() as f64 / 1000.0, 2)),
                "max_samples" => ji(v.iter().filter_map(|r| r.i("samples")).max()), "last_seen_at" => jt(newest.t("occurred_at")),
            }
        })
        .collect();
    Ok(crate::obj! {
        "profiles" => groups,
        "summary" => crate::obj! { "profiles" => count, "executions" => executions, "profiled" => profiled, "avg_samples" => rb::round0(avg) },
    })
}

fn executions_in(conn: &Connection, from: i64, to: i64) -> rusqlite::Result<i64> {
    let fh = rb::beginning_of_hour(from);
    let first = if fh == from { from } else { fh + 3_600_000_000 };
    let last = rb::beginning_of_hour(to);
    if first >= last {
        return db::count_where(conn, "executions", "\"executions\".\"occurred_at\" BETWEEN ?1 AND ?2", &[s(&qt(from)), s(&qt(to))]);
    }
    let a =
        db::count_where(conn, "executions", "\"executions\".\"occurred_at\" >= ?1 AND \"executions\".\"occurred_at\" < ?2", &[s(&qt(from)), s(&qt(first))])?;
    let b = db::count(
        conn,
        "SELECT SUM(\"rollups\".\"count\") FROM \"rollups\" WHERE \"rollups\".\"record_type\" IN ('request', 'job_attempt', 'scheduled_task', 'command', 'channel_action') \
         AND \"rollups\".\"bucket\" >= ?1 AND \"rollups\".\"bucket\" < ?2",
        rusqlite::params![qt(first), qt(last)],
    )?;
    let c = db::count_where(conn, "executions", "\"executions\".\"occurred_at\" BETWEEN ?1 AND ?2", &[s(&qt(last)), s(&qt(to))])?;
    Ok(a + b + c)
}

pub fn profiles_show(conn: &Connection, ctx: &PageContext, _control: &dyn ControlData) -> R {
    let id: i64 = ctx.param_s("id").parse().map_err(|_| PageError::NotFound)?;
    let p = db::query_one(conn, "SELECT \"profiles\".* FROM \"profiles\" WHERE \"profiles\".\"id\" = ?1 LIMIT 1", [id])?.ok_or(PageError::NotFound)?;
    let (collapsed, truncated) =
        collapsed_capped(p.blob("stacks").unwrap_or(&[]), crate::dashboard::execution_presenter::MAX_COLLAPSED_BYTES).map_err(PageError::Unprocessable)?;
    Ok(crate::obj! {
        "profile" => crate::obj! {
            "id" => ji(p.i("id")), "profiler" => js(p.s("profiler")), "mode" => js(p.s("mode")), "interval" => ji(p.i("interval")),
            "duration" => dur(&p, 2), "samples" => ji(p.i("samples")), "stacks_bytes" => ji(p.i("stacks_bytes")), "group_hash" => js(p.s("group_hash")),
            "execution_id" => js(p.s("execution_id")), "execution_preview" => js(p.s("execution_preview")), "execution_source" => js(p.s("execution_source")),
            "occurred_at" => jt(p.t("occurred_at")), "deploy" => js(p.s("deploy")), "server" => js(p.s("server")),
        },
        "collapsed" => collapsed, "truncated" => truncated,
    })
}

// -- view renders / transactions ---------------------------------------------

pub fn view_renders_index(conn: &Connection, ctx: &PageContext, _control: &dyn ControlData) -> R {
    let (from, to) = ctx.range();
    let p = filter_query::parse(ctx.param("q"));
    let rollups = rollup::between(conn, "view_render", from, to)?;
    let mut rows = aggregations::grouped_rows(&rollups, from, to, 200, ctx.param("sort"), ctx.param("dir"));
    if let Some(t) = p.field("template") {
        rows.retain(|r| r["name"].as_str().unwrap_or("").contains(t));
    }
    let kinds: HashMap<String, Value> = rollup::group_by_hash(&rollups)
        .into_iter()
        .map(|(g, rs)| (g, crate::dashboard::rb::max_by(&rs, |r| r.bucket).and_then(|r| r.extra.get("kind").cloned()).unwrap_or(Value::Null)))
        .collect();
    for r in rows.iter_mut() {
        let k = kinds.get(r["group_hash"].as_str().unwrap_or("")).cloned().unwrap_or(Value::Null);
        db::merge_into(r, "kind", k);
    }
    let slowest = db::query(
        conn,
        "SELECT \"view_renders\".* FROM \"view_renders\" WHERE \"view_renders\".\"occurred_at\" BETWEEN ?1 AND ?2 ORDER BY \"view_renders\".\"duration\" DESC LIMIT 30",
        rusqlite::params![qt(from), qt(to)],
    )?;
    Ok(crate::obj! {
        "renders" => rows, "series" => series(conn, ctx, "view_render", None)?,
        "slowest" => slowest.iter().map(|v| crate::obj! {
            "id" => ji(v.i("id")), "identifier" => js(v.s("identifier")), "kind" => js(v.s("kind")), "layout" => js(v.s("layout")),
            "duration" => dur(v, 3), "occurred_at" => jt(v.t("occurred_at")), "execution_id" => js(v.s("execution_id")),
            "execution_source" => js(v.s("execution_source")), "execution_preview" => js(v.s("execution_preview")), "group_hash" => js(v.s("group_hash")),
        }).collect::<Vec<_>>(),
        "sort" => sort_param(ctx, "count"), "dir" => dir_param(ctx), "q" => ctx.param_s("q"),
    })
}

pub fn transactions_index(conn: &Connection, ctx: &PageContext, _control: &dyn ControlData) -> R {
    let (from, to) = ctx.range();
    let recent = select(conn, "transactions", "\"transactions\".*", &between("transactions", from, to), &super::recent("transactions"), Some(50))?;
    Ok(crate::obj! {
        "transactions" => grouped(conn, ctx, "transaction", 200, true)?, "series" => series(conn, ctx, "transaction", None)?,
        "recent" => recent.iter().map(|t| crate::obj! {
            "id" => ji(t.i("id")), "outcome" => js(t.s("outcome")), "connection" => js(t.s("connection")), "duration" => dur(t, 3),
            "statement_count" => ji(t.i("statement_count")), "occurred_at" => jt(t.t("occurred_at")), "execution_id" => js(t.s("execution_id")),
            "execution_source" => js(t.s("execution_source")), "execution_preview" => js(t.s("execution_preview")), "group_hash" => js(t.s("group_hash")),
        }).collect::<Vec<_>>(),
        "sort" => sort_param(ctx, "count"), "dir" => dir_param(ctx),
    })
}

// -- deprecations ------------------------------------------------------------

pub fn deprecations_index(conn: &Connection, ctx: &PageContext, _control: &dyn ControlData) -> R {
    let (from, to) = ctx.range();
    let p = filter_query::parse(ctx.param("q"));
    let mut c = between("deprecations", from, to);
    if let Some(g) = p.field("gem") {
        c.eq("deprecations", "gem_name", s(g));
    }
    if rb::present(Some(&p.text)) {
        c.and("(message LIKE ? ESCAPE '\\')", vec![s(&format!("%{}%", rb::sanitize_sql_like(&p.text)))]);
    }
    let rows = select(conn, "deprecations", "\"deprecations\".*", &c, "", None)?;
    let mut order: Vec<String> = vec![];
    let mut groups: HashMap<String, Vec<&db::Rec>> = HashMap::new();
    for r in &rows {
        let g = r.s("group_hash").unwrap_or("").to_string();
        if !groups.contains_key(&g) {
            order.push(g.clone());
        }
        groups.entry(g).or_default().push(r);
    }
    let mut first_ever: HashMap<String, Option<i64>> = HashMap::new();
    for chunk in order.chunks(500) {
        let ph = vec!["?"; chunk.len()].join(", ");
        for r in db::query(
            conn,
            &format!(
                "SELECT MIN(\"deprecations\".\"occurred_at\") AS \"minimum_occurred_at\", \"deprecations\".\"group_hash\" AS \"deprecations_group_hash\" \
                 FROM \"deprecations\" WHERE \"deprecations\".\"group_hash\" IN ({ph}) GROUP BY \"deprecations\".\"group_hash\""
            ),
            rusqlite::params_from_iter(chunk),
        )? {
            first_ever.insert(r.s("deprecations_group_hash").unwrap_or("").to_string(), r.t("minimum_occurred_at"));
        }
    }
    let mut built: Vec<(Value, i64, Option<i64>)> = vec![];
    for g in &order {
        let grs = &groups[g];
        let mut sorted: Vec<&db::Rec> = grs.clone();
        sorted.sort_by_key(|r| r.t("occurred_at").unwrap_or(0)); // sort_by(&:occurred_at): Time keys -> stable mergesort
        let last = sorted.last().unwrap();
        let first_seen = first_ever.get(g).copied().flatten();
        let spark = aggregations::sparkline_counts(grs.iter().map(|r| (r.t("occurred_at").unwrap_or(0), 1)), from, to);
        let occ: Vec<Value> = sorted
            .iter()
            .rev()
            .take(20)
            .map(|r| {
                crate::obj! {
                    "id" => ji(r.i("id")), "occurred_at" => jt(r.t("occurred_at")), "execution_id" => js(r.s("execution_id")),
                    "execution_source" => js(r.s("execution_source")), "execution_preview" => js(r.s("execution_preview")),
                }
            })
            .collect();
        built.push((
            crate::obj! {
                "group_hash" => g.clone(), "message" => js(last.s("message")), "gem_name" => js(last.s("gem_name")), "horizon" => js(last.s("horizon")),
                "source" => js(last.s("source")), "count" => grs.len(), "first_seen_at" => jt(first_seen), "last_seen_at" => jt(last.t("occurred_at")),
                "sparkline" => spark, "occurrences" => occ,
            },
            grs.len() as i64,
            first_seen,
        ));
    }
    let built = rb::sort_by_num(built, |(_, c, _)| Num::I(-c));
    let total: i64 = built.iter().map(|b| b.1).sum();
    let new_this_window = built.iter().filter(|b| b.2.is_some_and(|t| t >= from)).count();
    let distinct = built.len();
    Ok(crate::obj! {
        "deprecations" => built.into_iter().take(200).map(|b| b.0).collect::<Vec<_>>(),
        "distinct" => distinct, "total_occurrences" => total, "new_this_window" => new_this_window, "q" => ctx.param_s("q"),
    })
}

// -- cache -------------------------------------------------------------------

pub fn cache_events_index(conn: &Connection, ctx: &PageContext, _control: &dyn ControlData) -> R {
    let (from, to) = ctx.range();
    let p = filter_query::parse(ctx.param("q"));
    let rows = match p.field("store") {
        None => rollup::between(conn, "cache_event", from, to)?,
        Some(store) => {
            let hashes: Vec<String> = db::query(
                conn,
                "SELECT DISTINCT \"cache_events\".\"group_hash\" FROM \"cache_events\" WHERE \"cache_events\".\"occurred_at\" BETWEEN ?1 AND ?2 AND \"cache_events\".\"store\" = ?3",
                rusqlite::params![qt(from), qt(to), store],
            )?
            .iter()
            .map(|r| r.s("group_hash").map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .unwrap_or_default();
            let mut out = vec![];
            for chunk in hashes.chunks(500) {
                let ph = vec!["?"; chunk.len()].join(", ");
                let mut binds: Vec<Sql> = vec![s("cache_event"), s(&qt(rb::beginning_of_hour(from))), s(&qt(to))];
                binds.extend(chunk.iter().map(|h| s(h)));
                out.extend(rollup::load(
                    conn,
                    &format!("SELECT \"rollups\".* FROM \"rollups\" WHERE \"rollups\".\"record_type\" = ? AND \"rollups\".\"bucket\" BETWEEN ? AND ? AND \"rollups\".\"group_hash\" IN ({ph})"),
                    rusqlite::params_from_iter(binds),
                )?);
            }
            out
        }
    };
    let keys: Vec<(Value, i64)> = rollup::group_by_hash(&rows)
        .into_iter()
        .map(|(g, rs)| {
            let count: i64 = rs.iter().map(|r| r.count).sum();
            let hits: i64 = rs.iter().map(|r| rollup::extra_i(r, "hits")).sum();
            let misses: i64 = rs.iter().map(|r| rollup::extra_i(r, "misses")).sum();
            let dsum: i64 = rs.iter().map(|r| r.duration_sum).sum();
            (
                crate::obj! {
                    "group_hash" => g, "key" => js(rollup::latest_name(&rs).as_deref()), "count" => count, "hits" => hits, "misses" => misses,
                    "hit_rate" => if hits + misses == 0 { Value::Null } else { jf(rb::round(hits as f64 * 100.0 / (hits + misses) as f64, 1)) },
                    "avg" => if count == 0 { Value::from(0) } else { jf(rb::round(dsum.div_euclid(count) as f64 / 1000.0, 3)) },
                    "sparkline" => aggregations::sparkline(&rs, from, to),
                },
                count,
            )
        })
        .collect();
    let keys = rb::sort_by_num(keys, |(_, c)| Num::I(-c));
    Ok(crate::obj! {
        "keys" => keys.into_iter().take(200).map(|k| k.0).collect::<Vec<_>>(),
        "series" => series(conn, ctx, "cache_event", None)?, "q" => ctx.param_s("q"),
    })
}

// -- mail / notifications / broadcasts / outgoing / storage --------------------

pub fn mails_index(conn: &Connection, ctx: &PageContext, _control: &dyn ControlData) -> R {
    let (from, to) = ctx.range();
    let p = filter_query::parse(ctx.param("q"));
    let mut c = between("mails", from, to);
    if let Some(m) = p.field("mailer") {
        c.eq("mails", "mailer", s(m));
    }
    if let Some(k) = p.field("kind") {
        c.eq("mails", "delivery_method", s(k));
    }
    let rows = select(conn, "mails", "\"mails\".*", &c, &super::recent("mails"), Some(200))?;
    Ok(crate::obj! {
        "mailers" => grouped(conn, ctx, "mail", 100, false)?, "series" => series(conn, ctx, "mail", None)?, "q" => ctx.param_s("q"),
        "recent" => rows.iter().map(|m| crate::obj! {
            "id" => ji(m.i("id")), "mailer" => js(m.s("mailer")), "subject" => js(m.s("subject")), "to" => ji(m.i("to")), "cc" => ji(m.i("cc")),
            "bcc" => ji(m.i("bcc")), "attachments" => ji(m.i("attachments")), "delivery_method" => js(m.s("delivery_method")),
            "duration" => dur(m, 1), "failed" => m.attr("mails", "failed"), "occurred_at" => jt(m.t("occurred_at")),
            "execution_id" => js(m.s("execution_id")), "execution_preview" => js(m.s("execution_preview")),
        }).collect::<Vec<_>>(),
    })
}

pub fn notifications_index(conn: &Connection, ctx: &PageContext, _control: &dyn ControlData) -> R {
    let (from, to) = ctx.range();
    let rows = select(conn, "notifications", "\"notifications\".*", &between("notifications", from, to), &super::recent("notifications"), Some(200))?;
    Ok(crate::obj! {
        "groups" => grouped(conn, ctx, "notification", 100, false)?,
        "recent" => rows.iter().map(|n| crate::obj! {
            "id" => ji(n.i("id")), "notifier" => js(n.s("notifier")), "delivery_method" => js(n.s("delivery_method")), "duration" => dur(n, 1),
            "failed" => n.attr("notifications", "failed"), "occurred_at" => jt(n.t("occurred_at")), "execution_id" => js(n.s("execution_id")),
            "execution_preview" => js(n.s("execution_preview")),
        }).collect::<Vec<_>>(),
    })
}

pub fn broadcasts_index(conn: &Connection, ctx: &PageContext, _control: &dyn ControlData) -> R {
    let (from, to) = ctx.range();
    let streams = db::query(
        conn,
        "SELECT \"broadcasts\".\"group_hash\", \"broadcasts\".\"kind\", MAX(COALESCE(stream, channel)), COUNT(*), SUM(bytes), AVG(duration) FROM \"broadcasts\" \
         WHERE \"broadcasts\".\"occurred_at\" BETWEEN ?1 AND ?2 GROUP BY \"broadcasts\".\"group_hash\", \"broadcasts\".\"kind\"",
        rusqlite::params![qt(from), qt(to)],
    )?;
    let streams = rb::sort_by_num(streams, |r| Num::I(-i(r.at(3))));
    let recent = select(conn, "broadcasts", "\"broadcasts\".*", &between("broadcasts", from, to), &super::recent("broadcasts"), Some(100))?;
    Ok(crate::obj! {
        "streams" => streams.iter().take(200).map(|r| crate::obj! {
            "group_hash" => db::raw_json(r.at(0)), "kind" => db::raw_json(r.at(1)), "name" => db::raw_json(r.at(2)), "count" => db::raw_json(r.at(3)),
            "bytes" => i(r.at(4)), "avg" => jf(rb::round(f(r.at(5)) / 1000.0, 3)),
        }).collect::<Vec<_>>(),
        "recent" => recent.iter().map(|b| crate::obj! {
            "id" => ji(b.i("id")), "kind" => js(b.s("kind")), "stream" => js(b.s("stream")), "channel" => js(b.s("channel")), "action" => js(b.s("action")),
            "bytes" => ji(b.i("bytes")), "failed" => b.attr("broadcasts", "failed"), "duration" => dur(b, 3), "occurred_at" => jt(b.t("occurred_at")),
            "execution_id" => js(b.s("execution_id")), "execution_source" => js(b.s("execution_source")), "execution_preview" => js(b.s("execution_preview")),
        }).collect::<Vec<_>>(),
    })
}

pub fn outgoing_requests_index(conn: &Connection, ctx: &PageContext, _control: &dyn ControlData) -> R {
    let (from, to) = ctx.range();
    let p = filter_query::parse(ctx.param("q"));
    let mut c = between("outgoing_requests", from, to);
    if let Some(h) = p.field("host") {
        c.and("(host LIKE ? ESCAPE '\\')", vec![s(&format!("%{}%", rb::sanitize_sql_like(h)))]);
    }
    if let Some((lo, hi)) = filter_query::status_range(p.fields.get("status").map(String::as_str)) {
        c.and("\"outgoing_requests\".\"status_code\" BETWEEN ? AND ?", vec![Sql::Integer(lo), Sql::Integer(hi)]);
    }
    let rows = select(conn, "outgoing_requests", "\"outgoing_requests\".*", &c, &super::recent("outgoing_requests"), Some(200))?;
    Ok(crate::obj! {
        "hosts" => grouped(conn, ctx, "outgoing_request", 100, false)?, "series" => series(conn, ctx, "outgoing_request", None)?, "q" => ctx.param_s("q"),
        "recent" => rows.iter().map(|r| crate::obj! {
            "id" => ji(r.i("id")), "host" => js(r.s("host")), "method" => js(r.s("method")), "url" => js(r.s("url")), "status_code" => ji(r.i("status_code")),
            "duration" => dur(r, 1), "request_size" => ji(r.i("request_size")), "response_size" => ji(r.i("response_size")), "error" => js(r.s("error")),
            "source" => js(r.s("source")), "occurred_at" => jt(r.t("occurred_at")), "execution_id" => js(r.s("execution_id")),
            "execution_preview" => js(r.s("execution_preview")), "response_body" => js(r.s("response_body")),
        }).collect::<Vec<_>>(),
    })
}

pub fn storage_ops_index(conn: &Connection, ctx: &PageContext, _control: &dyn ControlData) -> R {
    let (from, to) = ctx.range();
    let ops = db::query(
        conn,
        "SELECT \"storage_ops\".\"service\", \"storage_ops\".\"op\", COUNT(*), AVG(duration), MAX(duration) FROM \"storage_ops\" \
         WHERE \"storage_ops\".\"occurred_at\" BETWEEN ?1 AND ?2 GROUP BY \"storage_ops\".\"service\", \"storage_ops\".\"op\"",
        rusqlite::params![qt(from), qt(to)],
    )?;
    let recent = select(conn, "storage_ops", "\"storage_ops\".*", &between("storage_ops", from, to), &super::recent("storage_ops"), Some(100))?;
    Ok(crate::obj! {
        "ops" => ops.iter().map(|r| crate::obj! {
            "service" => db::raw_json(r.at(0)), "op" => db::raw_json(r.at(1)), "count" => db::raw_json(r.at(2)),
            "avg" => jf(rb::round(f(r.at(3)) / 1000.0, 1)), "max" => jf(rb::round(f(r.at(4)) / 1000.0, 1)),
        }).collect::<Vec<_>>(),
        "recent" => recent.iter().map(|r| crate::obj! {
            "id" => ji(r.i("id")), "service" => js(r.s("service")), "op" => js(r.s("op")), "key" => js(r.s("key")), "duration" => dur(r, 1),
            "occurred_at" => jt(r.t("occurred_at")), "execution_id" => js(r.s("execution_id")), "execution_preview" => js(r.s("execution_preview")),
        }).collect::<Vec<_>>(),
    })
}

// -- LLM calls ---------------------------------------------------------------

pub fn llm_calls_index(conn: &Connection, ctx: &PageContext, _control: &dyn ControlData) -> R {
    let (from, to) = ctx.range();
    let p = filter_query::parse(ctx.param("q"));
    let model_rollups = rollup::between(conn, "llm_call", from, to)?;
    let models_: Vec<(Value, f64, i64)> = rollup::group_by_hash(&model_rollups)
        .into_iter()
        .map(|(g, rs)| {
            let count: i64 = rs.iter().map(|r| r.count).sum();
            let s = rollup::summarize(&rs);
            let sum = |k: &str| rs.iter().map(|r| rollup::extra_i(r, k)).sum::<i64>();
            let (nanos, priced) = (sum("cost_nanos"), sum("priced"));
            let cost = if priced == 0 { None } else { Some(nanos as f64 / 1e9) };
            (
                crate::obj! {
                    "group_hash" => g, "name" => js(rollup::latest_name(&rs).as_deref()), "count" => count,
                    "errors" => rs.iter().map(|r| r.error_count).sum::<i64>(),
                    "input_tokens" => sum("input_tokens"), "output_tokens" => sum("output_tokens"),
                    "cache_read_tokens" => sum("cache_read_tokens"), "cache_write_tokens" => sum("cache_write_tokens"),
                    "cost" => jfo(cost), "unpriced" => sum("unpriced"), "truncated" => sum("truncated"), "with_attachments" => sum("with_attachments"),
                    "avg" => jf(rb::round(s.avg as f64 / 1000.0, 2)), "p95" => jf(rb::round(s.p95 as f64 / 1000.0, 2)),
                    "sparkline" => aggregations::sparkline(&rs, from, to),
                },
                cost.unwrap_or(0.0),
                count,
            )
        })
        .collect();
    // sort_by { [-(cost || 0), -count] }: Array keys -> ruby_qsort (stable).
    let mut models_ = models_;
    rb::sort_by_stable(&mut models_, |a, b| (-a.1).partial_cmp(&-b.1).unwrap_or(std::cmp::Ordering::Equal).then((-a.2).cmp(&-b.2)));
    let models_: Vec<Value> = models_.into_iter().take(200).map(|m| m.0).collect();

    let tool_rollups = rollup::between(conn, "llm_tool", from, to)?;
    let tools: Vec<(Value, f64)> = rollup::group_by_hash(&tool_rollups)
        .into_iter()
        .map(|(g, rs)| {
            let s = rollup::summarize(&rs);
            let p95 = rb::round(s.p95 as f64 / 1000.0, 2);
            (
                crate::obj! {
                    "group_hash" => g, "name" => js(rollup::latest_name(&rs).as_deref()), "count" => rs.iter().map(|r| r.count).sum::<i64>(),
                    "errors" => rs.iter().map(|r| r.error_count).sum::<i64>(), "avg" => jf(rb::round(s.avg as f64 / 1000.0, 2)),
                    "p95" => jf(p95), "max" => jf(rb::round(s.max as f64 / 1000.0, 2)), "sparkline" => aggregations::sparkline(&rs, from, to),
                },
                p95,
            )
        })
        .collect();
    let tools = rb::sort_by_num(tools, |(_, p)| Num::F(-p));
    let tools: Vec<Value> = tools.into_iter().take(200).map(|t| t.0).collect();

    let mut c = between("llm_calls", from, to);
    if let Some(m) = p.field("model") {
        c.and("(model LIKE ? ESCAPE '\\')", vec![s(&format!("%{}%", rb::sanitize_sql_like(m)))]);
    }
    for (f_, col) in [("provider", "provider"), ("operation", "operation"), ("workflow", "workflow_id"), ("finish", "finish_reason")] {
        if let Some(v) = p.field(f_) {
            c.eq("llm_calls", col, s(v));
        }
    }
    if let Some(a) = p.field("attachments") {
        if a == "any" {
            c.and("\"llm_calls\".\"attachments\" IS NOT NULL", vec![]);
        } else {
            c.and("(attachment_types LIKE ? ESCAPE '\\')", vec![s(&format!("%{}%", rb::sanitize_sql_like(a)))]);
        }
    }
    let recent = select(conn, "llm_calls", "\"llm_calls\".*", &c, &super::recent("llm_calls"), Some(200))?;
    let recent: Vec<Value> = recent
        .iter()
        .map(|r| {
            let a = |k: &str| r.attr("llm_calls", k);
            crate::obj! {
                "id" => a("id"), "operation" => a("operation"), "provider" => a("provider"), "model" => a("model"), "response_model" => a("response_model"),
                "tool_name" => a("tool_name"), "duration" => dur(r, 1), "status" => a("status"), "error" => a("error"), "streaming" => a("streaming"),
                "message_count" => a("message_count"), "tool_count" => a("tool_count"), "input_tokens" => a("input_tokens"), "output_tokens" => a("output_tokens"),
                "cache_read_tokens" => a("cache_read_tokens"), "cache_write_tokens" => a("cache_write_tokens"), "thinking_tokens" => a("thinking_tokens"),
                "cost" => r.i("cost_nanos").map_or(Value::Null, |n| jf(n as f64 / 1e9)), "cost_reported" => a("cost_reported"),
                "workflow_id" => a("workflow_id"), "workflow_name" => a("workflow_name"), "workflow_step_name" => a("workflow_step_name"),
                "finish_reason" => a("finish_reason"), "provider_request_id" => a("provider_request_id"), "tools" => a("tools"),
                "attachments" => a("attachments"), "attachment_types" => a("attachment_types"), "attachment_names" => a("attachment_names"),
                "tool_call_id" => a("tool_call_id"), "params" => a("params"), "prompt" => a("prompt"), "completion" => a("completion"),
                "occurred_at" => a("occurred_at"), "execution_id" => a("execution_id"), "execution_preview" => a("execution_preview"),
            }
        })
        .collect();

    let sumk = |v: &[Value], k: &str| v.iter().map(|m| m[k].as_i64().unwrap_or(0)).sum::<i64>();
    let priced: Vec<&Value> = models_.iter().filter(|m| !m["cost"].is_null()).collect();
    let totals = crate::obj! {
        "count" => sumk(&models_, "count"), "errors" => sumk(&models_, "errors"),
        "tool_count" => sumk(&tools, "count"), "tool_errors" => sumk(&tools, "errors"),
        "input_tokens" => sumk(&models_, "input_tokens"), "output_tokens" => sumk(&models_, "output_tokens"),
        "cost" => if priced.is_empty() { Value::Null } else { jf(priced.iter().map(|m| m["cost"].as_f64().unwrap_or(0.0)).sum::<f64>()) },
        "unpriced" => sumk(&models_, "unpriced"),
    };
    Ok(crate::obj! {
        "models" => models_, "tools" => tools, "recent" => recent, "totals" => totals,
        "series" => series(conn, ctx, "llm_call", None)?, "tool_series" => series(conn, ctx, "llm_tool", None)?, "q" => ctx.param_s("q"),
    })
}

// -- visits ------------------------------------------------------------------

pub const MAX_VITAL_SAMPLES: usize = 50_000;
const VITALS: [(&str, f64, f64); 4] = [("lcp", 2500.0, 4000.0), ("cls", 0.1, 0.25), ("inp", 200.0, 500.0), ("ttfb", 800.0, 1800.0)];

fn rating(metric: &str, v: f64) -> &'static str {
    let (_, good, poor) = VITALS.iter().find(|(m, _, _)| *m == metric).unwrap();
    if v <= *good {
        "good"
    } else if v > *poor {
        "poor"
    } else {
        "needs-improvement"
    }
}

/// `p75(values)`: values are Integers (lcp/inp/ttfb) or Floats (cls); the
/// result is `.round(3)` of that element (Integer#round(3) is the Integer).
fn p75(mut values: Vec<Sql>) -> Value {
    values.sort_by(|a, b| f(a).partial_cmp(&f(b)).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((values.len() as f64 * 0.75).ceil() as i64 - 1).max(0) as usize;
    match &values[idx] {
        Sql::Integer(n) => Value::from(*n),
        Sql::Real(x) => jf(rb::round(*x, 3)),
        _ => Value::Null,
    }
}

/// One visit that measured a vital: its component, its deploy, the value.
pub struct VitalSample {
    pub component: String,
    pub deploy: Option<String>,
    value: Sql,
}

/// The window's web-vital samples, per metric (VITALS order), newest first
/// and at most MAX_VITAL_SAMPLES each. The visits page, the MCP
/// web_vitals tool and /api/v1 .../visits all read these, so their
/// numbers agree.
pub struct VitalSamples(Vec<(&'static str, Vec<VitalSample>)>);

pub fn vital_samples(conn: &Connection, from: i64, to: i64) -> rusqlite::Result<VitalSamples> {
    let mut out = vec![];
    for (m, _, _) in VITALS {
        let rows = db::query(
            conn,
            &format!(
                "SELECT \"visits\".\"component\", \"visits\".\"deploy\", \"visits\".\"{m}\" FROM \"visits\" WHERE \"visits\".\"occurred_at\" BETWEEN ?1 AND ?2 \
                 AND \"visits\".\"{m}\" IS NOT NULL ORDER BY \"visits\".\"occurred_at\" DESC LIMIT {MAX_VITAL_SAMPLES}"
            ),
            rusqlite::params![qt(from), qt(to)],
        )?;
        let samples = rows
            .iter()
            .map(|r| VitalSample { component: r.s("component").unwrap_or("").to_string(), deploy: r.s("deploy").map(str::to_string), value: r.at(2).clone() })
            .collect();
        out.push((m, samples));
    }
    Ok(VitalSamples(out))
}

impl VitalSamples {
    /// `{metric => {value, rating, samples}}` for every metric with samples:
    /// the p75, rated against web.dev's good / poor thresholds.
    pub fn summary(&self) -> Map<String, Value> {
        summarize_vitals(self.0.iter().map(|(m, samples)| (*m, samples.iter().map(|s| s.value.clone()).collect())))
    }

    /// `summary` per `key` (a component, a deploy), in one pass. Samples
    /// whose key is None are left out.
    pub fn summary_by(&self, key: impl Fn(&VitalSample) -> Option<&str>) -> HashMap<String, Map<String, Value>> {
        let mut by: HashMap<String, Vec<(&'static str, Vec<Sql>)>> = HashMap::new();
        for (m, samples) in &self.0 {
            for s in samples {
                let Some(k) = key(s) else { continue };
                let metrics = by.entry(k.to_string()).or_insert_with(|| VITALS.iter().map(|(m, _, _)| (*m, vec![])).collect());
                metrics.iter_mut().find(|(x, _)| x == m).unwrap().1.push(s.value.clone());
            }
        }
        by.into_iter().map(|(k, metrics)| (k, summarize_vitals(metrics.into_iter()))).collect()
    }

    /// Only the samples of one component.
    pub fn of_component(self, component: &str) -> VitalSamples {
        VitalSamples(self.0.into_iter().map(|(m, samples)| (m, samples.into_iter().filter(|s| s.component == component).collect())).collect())
    }
}

fn summarize_vitals(metrics: impl Iterator<Item = (&'static str, Vec<Sql>)>) -> Map<String, Value> {
    let mut vitals = Map::new();
    for (m, vals) in metrics {
        if vals.is_empty() {
            continue;
        }
        let n = vals.len();
        let v = p75(vals);
        vitals.insert(m.into(), crate::obj! { "value" => v.clone(), "rating" => rating(m, v.as_f64().unwrap_or(0.0)), "samples" => n });
    }
    vitals
}

/// The good / poor thresholds the ratings use, for API and MCP readers.
pub fn vital_thresholds() -> Value {
    Value::Object(
        VITALS
            .iter()
            .map(|(m, good, poor)| {
                let unit = if *m == "cls" { "score" } else { "ms" };
                ((*m).to_string(), crate::obj! { "good" => jf(*good), "poor" => jf(*poor), "unit" => unit })
            })
            .collect(),
    )
}

/// The visits page's components (`grouped("visit", 100)`, so visit count,
/// errors and duration percentiles in ms) with each one's `vitals`. A
/// `sort` of a vital orders by its p75, worst first unless `dir` is asc,
/// components without that vital last; any other sort is `grouped`'s.
pub fn visit_components(conn: &Connection, from: i64, to: i64, samples: &VitalSamples, sort: Option<&str>, dir: Option<&str>) -> rusqlite::Result<Vec<Value>> {
    let by = samples.summary_by(|s| Some(&s.component));
    let vital = sort.filter(|s| VITALS.iter().any(|(m, _, _)| m == s));
    let rows: Vec<Value> = aggregations::grouped(conn, "visit", from, to, 100, if vital.is_some() { None } else { sort }, dir)?
        .into_iter()
        .map(|mut row| {
            let v = by.get(row["name"].as_str().unwrap_or("")).cloned().unwrap_or_default();
            db::merge_into(&mut row, "vitals", Value::Object(v));
            row
        })
        .collect();
    let Some(m) = vital else { return Ok(rows) };
    let sign = if dir == Some("asc") { 1.0 } else { -1.0 };
    let (mut with, without): (Vec<Value>, Vec<Value>) = rows.into_iter().partition(|r| r["vitals"].get(m).is_some());
    with = rb::sort_by_num(with, |r| rb::Num::F(sign * r["vitals"][m]["value"].as_f64().unwrap_or(0.0)));
    with.extend(without);
    Ok(with)
}

/// The vitals of each deploy's visits, for the deploys given.
pub fn vitals_by_deploy(samples: &VitalSamples, deploys: &[String]) -> Vec<Map<String, Value>> {
    let by = samples.summary_by(|s| s.deploy.as_deref());
    deploys.iter().map(|d| by.get(d).cloned().unwrap_or_default()).collect()
}

pub fn visits_index(conn: &Connection, ctx: &PageContext, _control: &dyn ControlData) -> R {
    let (from, to) = ctx.range();
    let p = filter_query::parse(ctx.param("q"));
    let mut rc = between("visits", from, to);
    if let Some(cmp) = p.field("component") {
        rc.eq("visits", "component", s(cmp));
    }
    let rows = select(conn, "visits", "\"visits\".*", &rc, &super::recent("visits"), Some(200))?;
    let samples = vital_samples(conn, from, to)?;
    let by_component = samples.summary_by(|s| Some(&s.component));
    let components: Vec<Value> = grouped(conn, ctx, "visit", 100, true)?
        .into_iter()
        .map(|mut row| {
            let v = by_component.get(row["name"].as_str().unwrap_or(""));
            for m in ["lcp", "inp"] {
                db::merge_into(&mut row, m, v.and_then(|v| v.get(m)).map_or(Value::Null, |x| x["value"].clone()));
            }
            row
        })
        .collect();
    Ok(crate::obj! {
        "components" => components, "series" => series(conn, ctx, "visit", None)?, "vitals" => Value::Object(samples.summary()),
        "sort" => sort_param(ctx, "count"), "dir" => dir_param(ctx), "q" => ctx.param_s("q"),
        "recent" => rows.iter().map(|v| crate::obj! {
            "id" => ji(v.i("id")), "component" => js(v.s("component")), "url" => js(v.s("url")), "method" => js(v.s("method")), "duration" => dur(v, 1),
            "status" => js(v.s("status")), "partial" => v.attr("visits", "partial"), "only" => v.json("only"), "props_bytes" => ji(v.i("props_bytes")),
            "occurred_at" => jt(v.t("occurred_at")), "user_ref" => js(v.s("user_ref")), "lcp" => ji(v.i("lcp")), "cls" => v.attr("visits", "cls"),
            "inp" => ji(v.i("inp")), "ttfb" => ji(v.i("ttfb")),
        }).collect::<Vec<_>>(),
    })
}

/// `queries_index` with its independent sections (grouped table, summary
/// with delta, series, n+1s, the cursor page) on separate read connections.
/// Same props, same key order.
pub fn queries_index_parallel(pools: &crate::dashboard::pool::ReadPools, slug: &str, ctx: &PageContext, _control: &(dyn ControlData + Sync)) -> R {
    let (from, to) = ctx.range();
    let run = |f: &(dyn Fn(&Connection) -> R + Sync)| -> R {
        let conn = pools.get(slug)?;
        f(&conn)
    };
    let (grouped_r, summary_r, page_r, npo_r) = std::thread::scope(|s| {
        let g = s.spawn(|| run(&|c| Ok(Value::from(grouped(c, ctx, "query", 200, true)?))));
        let sm = s.spawn(|| run(&|c| Ok(summary_with_delta(c, ctx, "query", None)?)));
        let pg = s.spawn(|| {
            run(&|c| {
                let cond = filter_query::apply(c, Resource::Queries, ctx.param("q"), from, to, &[])?;
                let page = cursor_page::call(
                    c,
                    &ctx.cursor_key,
                    "queries",
                    "Railwatch::Telemetry::Query",
                    models::QUERY_WITH_SQL,
                    cond,
                    ctx.param("cursor"),
                    ctx.param("limit"),
                    Order::Slowest,
                    &ctx.cursor_context("queries"),
                )??;
                Ok(crate::obj! { "rows" => page.rows.iter().map(models::query_row).collect::<Vec<_>>(), "meta" => page.meta })
            })
        });
        let np = s.spawn(|| run(&|c| Ok(n_plus_ones(c, from, to)?)));
        (g.join().unwrap(), sm.join().unwrap(), pg.join().unwrap(), np.join().unwrap())
    });
    let mut page = page_r?;
    Ok(crate::obj! {
        "queries" => grouped_r?,
        "n_plus_ones" => npo_r?,
        "slowest" => page["rows"].take(),
        "pagination" => page["meta"].take(), "q" => ctx.param_s("q"),
        "summary" => summary_r?,
        "sort" => sort_param(ctx, "count"), "dir" => dir_param(ctx),
    })
}

#[cfg(test)]
mod like_tests {
    use super::*;
    use crate::dashboard::control::StaticControl;

    /// The cloud controllers' `LIKE` filters now carry ESCAPE '\': a `%` or
    /// `_` in the search matches only itself (a Rails bug fixed on both sides).
    #[test]
    fn text_filters_match_percent_and_underscore_literally() {
        let dir = std::env::temp_dir().join(format!("rw-like-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let t = crate::tenants::open_tenant(&dir.join("t.sqlite3")).unwrap();
        let now = 1_792_000_000_000_000;
        let at = qt(now - 60_000_000);
        for (m, h, model) in [("use digest_daily", "a_b.example.com", "gpt_4"), ("use digestXdaily", "aXb.example.com", "gptX4")] {
            t.execute("INSERT INTO deprecations (occurred_at, message, group_hash) VALUES (?, ?, ?)", rusqlite::params![at, m, m]).unwrap();
            t.execute("INSERT INTO outgoing_requests (occurred_at, host, method, duration) VALUES (?, ?, 'GET', 1000)", rusqlite::params![at, h]).unwrap();
            t.execute(
                "INSERT INTO llm_calls (occurred_at, operation, duration, model, attachment_types) VALUES (?, 'chat', 1000, ?, ?)",
                rusqlite::params![at, model, if model == "gpt_4" { "100%png" } else { "1000png" }],
            )
            .unwrap();
        }
        let page = |f: fn(&Connection, &PageContext, &dyn ControlData) -> R, q: &str| {
            let params = [("q".to_string(), q.to_string())].into_iter().collect();
            f(&t, &PageContext::new(1, now, params, crate::dashboard::CursorKey(b"k".to_vec())), &StaticControl::default()).unwrap()
        };
        let d = page(deprecations_index, "digest_daily");
        assert_eq!(d["deprecations"].as_array().unwrap().iter().map(|x| x["message"].clone()).collect::<Vec<_>>(), ["use digest_daily"]);
        let o = page(outgoing_requests_index, "host:a_b");
        assert_eq!(o["recent"].as_array().unwrap().iter().map(|x| x["host"].clone()).collect::<Vec<_>>(), ["a_b.example.com"]);
        let l = page(llm_calls_index, "model:gpt_");
        assert_eq!(l["recent"].as_array().unwrap().iter().map(|x| x["model"].clone()).collect::<Vec<_>>(), ["gpt_4"]);
        let l = page(llm_calls_index, "attachments:100%");
        assert_eq!(l["recent"].as_array().unwrap().iter().map(|x| x["attachment_types"].clone()).collect::<Vec<_>>(), ["100%png"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A recent broadcast carries its parent's kind, so the page can link a
    /// job's broadcast to the job rather than to the request route.
    #[test]
    fn recent_broadcasts_carry_the_execution_source() {
        let dir = std::env::temp_dir().join(format!("rw-broadcasts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let t = crate::tenants::open_tenant(&dir.join("t.sqlite3")).unwrap();
        let now = 1_792_000_000_000_000;
        t.execute(
            "INSERT INTO broadcasts (occurred_at, kind, stream, duration, group_hash, execution_source, execution_id) VALUES (?, 'broadcast', 'widgets', 1000, 'g', 'job', 'e1')",
            rusqlite::params![qt(now - 60_000_000)],
        )
        .unwrap();
        let ctx = PageContext::new(1, now, Default::default(), crate::dashboard::CursorKey(b"k".to_vec()));
        let b = broadcasts_index(&t, &ctx, &StaticControl::default()).unwrap();
        assert_eq!(b["recent"][0]["execution_source"], "job");
        assert_eq!(b["recent"][0]["execution_id"], "e1");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod per_day_tests {
    use super::*;
    use crate::dashboard::control::StaticControl;

    /// The per-file rewrites (cursor pages, FTS paired per file, the summed
    /// aggregates) give the single file's props: four log levels, two queues
    /// with real latencies, three users, rows on five days, a text filter, a
    /// cursor page and the slowest-queries page.
    #[test]
    fn per_day_pages_equal_the_single_file() {
        let dir = std::env::temp_dir().join(format!("rw-perday-{}", crate::control::base58(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("t.sqlite3");
        let t = crate::tenants::open_tenant(&file).unwrap();
        let now = crate::time::parse_db_time("2026-10-16 12:00:00").unwrap();
        let mut id = 0;
        // Five days: enough files that per-file binds renumber past ?3.
        for day in ["2026-10-12", "2026-10-13", "2026-10-14", "2026-10-15", "2026-10-16"] {
            for (i, level) in ["warn", "info", "error", "debug", "info", "info"].iter().enumerate() {
                id += 1;
                let at = format!("{day} 0{i}:00:00.{id:06}");
                let msg = if i % 2 == 0 { format!("Completed {id}") } else { format!("Started {id}") };
                t.execute("INSERT INTO logs (id, level, message, occurred_at) VALUES (?, ?, ?, ?)", rusqlite::params![id, level, msg, at]).unwrap();
                t.execute(
                    "INSERT INTO queries (id, sql, name, duration, occurred_at, adapter, group_hash) VALUES (?, 'SELECT 1', 'Widget Load', ?, ?, 'sqlite', 'g')",
                    rusqlite::params![id, (id * 7919) % 50, at],
                )
                .unwrap();
                // Jobs on two queues with odd latencies (AVG over files must be
                // exact) and requests from three users, some failing.
                t.execute(
                    "INSERT INTO executions (id, execution_id, kind, name, occurred_at, duration, queue, queue_latency, outcome, status, user_ref, group_hash) \
                     VALUES (?1, ?2, ?3, 'X', ?4, 1000, ?5, ?6, ?7, ?8, ?9, 'g')",
                    rusqlite::params![
                        id,
                        format!("e{id}"),
                        if i % 2 == 0 { "job_attempt" } else { "request" },
                        at,
                        if i % 4 == 0 { "default" } else { "mailers" },
                        if i == 2 { None } else { Some(id * 37 + 1) },
                        if i == 4 { "failed" } else { "ok" },
                        if i == 3 { 503 } else { 200 },
                        ["ada", "bob", "cy"][id as usize % 3],
                    ],
                )
                .unwrap();
            }
        }
        t.execute("INSERT INTO logs_fts (rowid, message) SELECT id, message FROM logs", []).unwrap();
        drop(t);
        crate::storage::export::split(&file, &dir.join("t")).unwrap();
        let single = crate::dashboard::pool::open_reader(&file).unwrap();
        let (days, _) = crate::storage::reader::open(&dir.join("t")).unwrap();
        let ctx = |params: &[(&str, &str)]| {
            PageContext::new(1, now, params.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), crate::dashboard::CursorKey(b"k".to_vec()))
        };
        for params in [
            vec![("window", "7d")],
            vec![("window", "7d"), ("q", "complet")],
            vec![("window", "24h"), ("limit", "2")],
            vec![("window", "7d"), ("q", "level:info")],
            // The single file's slowest-queries probe: filters matching every
            // row (a full page from the probe) and none (the fallback).
            vec![("window", "7d"), ("q", "widget"), ("limit", "2")],
            vec![("window", "7d"), ("q", "adapter:sqlite"), ("limit", "3")],
            vec![("window", "7d"), ("q", "adapter:mysql")],
        ] {
            let c = ctx(&params);
            let a = logs_index(&single, &c, &StaticControl::default()).unwrap();
            let b = logs_index(&days, &c, &StaticControl::default()).unwrap();
            assert_eq!(serde_json::to_string(&a).unwrap(), serde_json::to_string(&b).unwrap(), "logs {params:?}");
            let a = queries_index(&single, &c, &StaticControl::default()).unwrap();
            let b = queries_index(&days, &c, &StaticControl::default()).unwrap();
            assert_eq!(serde_json::to_string(&a).unwrap(), serde_json::to_string(&b).unwrap(), "queries {params:?}");
            for page in [crate::dashboard::pages::jobs_index as fn(&Connection, &PageContext, &dyn ControlData) -> R, people_index, profiles_index] {
                let a = page(&single, &c, &StaticControl::default()).unwrap();
                let b = page(&days, &c, &StaticControl::default()).unwrap();
                assert_eq!(serde_json::to_string(&a).unwrap(), serde_json::to_string(&b).unwrap(), "{params:?}");
            }
        }
        let queues = crate::dashboard::pages::queue_stats(&days, now - 7 * 86_400_000_000, now).unwrap();
        assert_eq!(queues.as_array().unwrap().len(), 2, "{queues}");
        let counts = logs_index(&days, &ctx(&[("window", "7d")]), &StaticControl::default()).unwrap()["counts"].clone();
        assert_eq!(serde_json::to_string(&counts).unwrap(), r#"{"debug":5,"error":5,"info":15,"warn":5}"#);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
