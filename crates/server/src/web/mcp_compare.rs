//! compare: before/after for a route, a job class or the whole environment,
//! to prove a fix worked.
//!
//! Two ways to cut the sides:
//! - deploys (production): the executions each deploy ran (the gem stamps
//!   every record with its deploy). The default is the latest deploy against
//!   the one before it; `before`/`after` name others by deploy, ref prefix or
//!   name. With no deploys recorded, the deploy values the executions carry.
//! - a mark (development): `since`, a time or an execution id; the after
//!   side is everything later, the before side the `window` before it. In
//!   development every record carries the git HEAD read at boot, and Rails
//!   reloads code without rebooting, so an edit does not change the deploy
//!   value: the mark is what tells before from after.
//!
//! Per side: count, errors, p50/p95/avg, queries and db time per request,
//! read from the executions themselves (rollups have no deploy); then the
//! query shapes and N+1s of a sample of each side's executions, diffed.

use std::collections::HashMap;

use rusqlite::{Connection, params_from_iter};
use serde_json::{Value, json};

use super::{Args, EnvRow, Mcp, R, segment, time_s};
use crate::dashboard::control::{ControlData, Deploy, recent_deploys};
use crate::dashboard::db::{self, Rec, jf, qt};
use crate::dashboard::rb;

const DAY: i64 = 24 * 3_600_000_000;
/// Executions read per side for the numbers.
const SIDE_ROWS: usize = 20_000;
/// Executions per side whose queries and N+1s are diffed.
const SHAPE_SAMPLE: usize = 100;
/// Fewer executions than this on a side and its percentiles are noise.
pub const MIN_SAMPLES: usize = 20;
/// A change smaller than this (percent) is not called better or worse.
const NOISE_PCT: f64 = 10.0;
/// How far the whole environment's traffic mix may move (mix_shift, 0-100)
/// before the verdict compares route by route instead of the blend.
const MIX_PCT: f64 = 15.0;

/// One side of the comparison: which executions, and how to name them.
struct Side {
    label: String,
    /// `deploy IN (...)`, or a time range.
    deploys: Vec<String>,
    range: (i64, i64),
    /// The range's upper bound is exclusive (the before side of a mark).
    exclusive_to: bool,
    meta: Value,
}

impl Mcp<'_> {
    pub(super) fn compare(&self, a: &Args) -> R<String> {
        let Some(env) = self.resolve_environment(a)? else { return self.no_environment(a) };
        let kind = match a.presence("kind").as_deref() {
            None | Some("request") => "request",
            Some("job") => "job_attempt",
            Some(_) => return Ok(json!({"error": "kind must be request or job"}).to_string()),
        };
        let (wfrom, wto) = self.window(a);
        let deploys = self.env.control.with(|c| super::super::dashboard_bridge::DbControl::load(c, env.id))?.deploys();
        let name = a.s(if kind == "request" { "route" } else { "job" });
        let name = if rb::present(Some(&name)) { name } else { a.s("route") };
        self.with_telemetry(&env, |conn| {
            let group = if a.present("group_hash") || rb::present(Some(&name)) {
                // A route or job not seen in the window can still have run in
                // an older deploy: look a month back for its group.
                let found = match super::find_group(conn, kind, &name, a.presence("group_hash"), self.now - 30 * DAY, self.now)? {
                    Some(g) => Some(g),
                    // Not rolled up yet (the request just made): its executions.
                    None => db::query_one(
                        conn,
                        "SELECT \"executions\".\"group_hash\" FROM \"executions\" WHERE \"executions\".\"kind\" = ?1 AND \"executions\".\"occurred_at\" >= ?2 \
                         AND \"executions\".\"name\" = ?3 ORDER BY \"executions\".\"occurred_at\" DESC LIMIT 1",
                        rusqlite::params![kind, qt(self.now - DAY), name],
                    )?
                    .and_then(|r| r.s("group_hash").map(str::to_string)),
                };
                match found {
                    Some(g) => Some(g),
                    None => return Ok(json!({"error": format!("{} not found", if kind == "request" { "route" } else { "job" }),
                        "hint": "Pass route as list_slow_routes shows it (\"GET /orders/:id\"), a job class name, or a group_hash; omit both to compare the whole environment."}).to_string()),
                }
            } else {
                None
            };
            let sides = if a.present("since") {
                self.mark_sides(conn, a, (wfrom, wto))?
            } else {
                deploy_sides(conn, a, &deploys, kind, group.as_deref(), self.now)?
            };
            let (before, after) = match sides {
                Ok(s) => s,
                Err(e) => return Ok(e.to_string()),
            };
            let b = measure(conn, kind, group.as_deref(), &before)?;
            let f = measure(conn, kind, group.as_deref(), &after)?;
            // The default pair is the latest two deploys, but an app that
            // deploys several times a day often has a route or job with no
            // traffic in one of them: compare around the newest deploy that
            // has some instead, the same length of time either side of it.
            let defaulted = !(a.present("since") || a.present("before") || a.present("after"));
            if defaulted && (b.count == 0 || f.count == 0)
                && let Some((before, after, note)) = self.around_busy_deploy(conn, kind, group.as_deref(), &deploys)?
            {
                let b = measure(conn, kind, group.as_deref(), &before)?;
                let f = measure(conn, kind, group.as_deref(), &after)?;
                let mut v = self.compared(&env, kind, group.as_deref(), &name, &before, &after, &b, &f, true);
                db::merge_into(&mut v, "defaulted", Value::from(note));
                return Ok(v.to_string());
            }
            let v = self.compared(&env, kind, group.as_deref(), &name, &before, &after, &b, &f, a.present("since"));
            Ok(v.to_string())
        })
    }

    /// `since`: an execution id (the mark is its time: it and everything
    /// before it are the before side) or a time.
    fn mark_sides(&self, conn: &Connection, a: &Args, window: (i64, i64)) -> R<Result<(Side, Side), Value>> {
        let since = a.s("since");
        let span = window.1 - window.0;
        let (at, inclusive, what) = match crate::dashboard::window::parse_time(&since) {
            Some(t) => (t, false, format!("since {}", str_of(&time_s(Some(t))))),
            None => match db::query_one(conn, "SELECT \"executions\".* FROM \"executions\" WHERE \"executions\".\"execution_id\" = ?1 LIMIT 1", [&since])? {
                Some(e) => (e.t("occurred_at").unwrap_or(0), true, format!("after execution {since}")),
                None => {
                    return Ok(Err(json!({"error": "since is neither a time nor a stored execution_id",
                        "hint": "Pass since as a time (\"2026-10-25T17:00:00Z\") or the execution_id of the last request before your change (last_execution, or the X-Railwatch-Execution-Id response header in development)."})));
                }
            },
        };
        if at > self.now {
            return Ok(Err(json!({"error": "since is in the future"})));
        }
        // An execution mark goes on the before side; a time starts the after side.
        let split = if inclusive { at + 1 } else { at };
        let before = Side {
            label: "before".into(),
            deploys: vec![],
            range: (split - span, split),
            exclusive_to: true,
            meta: crate::obj! { "from" => time_s(Some(split - span)), "to" => time_s(Some(split)) },
        };
        let after = Side {
            label: what,
            deploys: vec![],
            range: (split, self.now),
            exclusive_to: false,
            meta: crate::obj! { "from" => time_s(Some(split)), "to" => time_s(Some(self.now)) },
        };
        Ok(Ok((before, after)))
    }

    /// For the default compare when a side of the latest two deploys is
    /// empty: the newest recorded deploy (in the last 30 days) whose own
    /// windows include this route or job on both sides, split at its deploy
    /// time with the same span on each side (up to a week, and no longer
    /// than the releases either side of it ran, so each side is one release).
    /// None when no deploy has traffic on both sides.
    fn around_busy_deploy(&self, conn: &Connection, kind: &str, group: Option<&str>, deploys: &[Deploy]) -> R<Option<(Side, Side, String)>> {
        let recorded = recent_deploys(deploys.to_vec());
        for (i, d) in recorded.iter().enumerate() {
            if d.deployed_at < self.now - 30 * DAY {
                break;
            }
            // Equal spans either side, so counts compare too, and each side
            // one release: up to a week, no longer than since the previous
            // deploy or until the next one (or now).
            let since_previous = recorded.get(i + 1).map_or(i64::MAX, |p| d.deployed_at - p.deployed_at);
            let until_next = i.checked_sub(1).and_then(|j| recorded.get(j)).map_or(self.now, |n| n.deployed_at) - d.deployed_at;
            let span = until_next.min(7 * DAY).min(since_previous);
            if span <= 0 {
                continue;
            }
            let probe = |range| Side { label: String::new(), deploys: vec![], range, exclusive_to: false, meta: Value::Null };
            // Both sides need traffic, or this deploy can't answer either.
            if measure_count(conn, kind, group, &probe((d.deployed_at, d.deployed_at + span)))? == 0
                || measure_count(conn, kind, group, &probe((d.deployed_at - span, d.deployed_at - 1)))? == 0
            {
                continue;
            }
            let from = d.deployed_at - span;
            let before = Side {
                label: "before".into(),
                deploys: vec![],
                range: (from, d.deployed_at),
                exclusive_to: true,
                meta: crate::obj! { "from" => time_s(Some(from)), "to" => time_s(Some(d.deployed_at)) },
            };
            let after = Side {
                label: format!("since deploy {}", d.name.clone().unwrap_or_else(|| rb::first_chars(&d.deploy, 12).to_string())),
                deploys: vec![],
                range: (d.deployed_at, d.deployed_at + span),
                exclusive_to: false,
                meta: crate::obj! {
                    "from" => time_s(Some(d.deployed_at)), "to" => time_s(Some(d.deployed_at + span)), "deploy" => d.deploy.clone(),
                    "deployed_at" => time_s(Some(d.deployed_at)), "deploy_id" => d.id,
                },
            };
            let note = if i == 0 {
                format!(
                    "The latest two deploys had no {} executions on one side, so this compares the time before and after the latest deploy instead.",
                    if kind == "request" { "matching request" } else { "matching job" }
                )
            } else {
                format!(
                    "The latest deploy{} had no matching executions, so this compares around deploy {} ({}), the newest one with traffic. Pass before/after or since to choose.",
                    if i > 1 { "s" } else { "" },
                    rb::first_chars(&d.deploy, 12),
                    str_of(&time_s(Some(d.deployed_at)))
                )
            };
            return Ok(Some((before, after, note)));
        }
        Ok(None)
    }

    #[allow(clippy::too_many_arguments)]
    fn compared(
        &self,
        env: &EnvRow,
        kind: &str,
        group: Option<&str>,
        name: &str,
        before: &Side,
        after: &Side,
        b: &Measured,
        f: &Measured,
        by_mark: bool,
    ) -> Value {
        let pct = |x: Option<f64>, y: Option<f64>| match (x, y) {
            (Some(x), Some(y)) if x > 0.0 => Some(rb::round((y - x) / x * 100.0, 1)),
            _ => None,
        };
        let deltas = crate::obj! {
            "count" => f.count as i64 - b.count as i64,
            "p50_pct" => opt(pct(b.p50, f.p50)), "p95_pct" => opt(pct(b.p95, f.p95)), "avg_pct" => opt(pct(b.avg, f.avg)),
            "queries_per_request_pct" => opt(pct(b.queries, f.queries)), "db_ms_per_request_pct" => opt(pct(b.db_ms, f.db_ms)),
            "error_rate_pts" => match (b.error_rate(), f.error_rate()) { (Some(x), Some(y)) => jf(rb::round((y - x) * 100.0, 2)), _ => Value::Null },
        };
        let mut worse: Vec<f64> =
            ["p95_pct", "avg_pct", "queries_per_request_pct", "db_ms_per_request_pct"].iter().filter_map(|k| deltas[*k].as_f64()).collect();
        // From none to some has no percent: count it as doubling, so a CI
        // gate on worst_regression_pct still trips (0 -> 500 queries).
        for (x, y) in [(b.queries, f.queries), (b.db_ms, f.db_ms)] {
            if let (Some(x), Some(y)) = (x, y)
                && x <= 0.0
                && y >= 1.0
            {
                worse.push(100.0);
            }
        }
        let mut worst = worse.iter().copied().fold(0.0f64, f64::max);
        let low = b.count < MIN_SAMPLES || f.count < MIN_SAMPLES;
        let shapes = shape_diff(&b.shapes, &f.shapes, b.sampled, f.sampled);
        let n1 = n_plus_one_diff(&b.n_plus_ones, &f.n_plus_ones);
        // The whole environment blends every route: when the mix moved (a
        // burst of sign-in redirects one side, dashboard loads the other),
        // the blend moves with it although no route changed. Then the
        // verdict holds each route to itself, weighted by the after side.
        let shift = if group.is_none() && b.count > 0 && f.count > 0 { mix_shift(b, f) } else { 0.0 };
        let adjusted = (shift >= MIX_PCT).then(|| mix_adjusted(b, f)).flatten();
        let mix = adjusted.map(|l| {
            let ((ab, af), (qb, qf), (db, df)) = (l.avg, l.queries, l.db_ms);
            let adj = crate::obj! {
                "p95_pct" => Value::Null, "avg_pct" => opt(pct(Some(ab), Some(af))),
                "queries_per_request_pct" => opt(pct(qb, qf)), "db_ms_per_request_pct" => opt(pct(db, df)),
                "error_rate_pts" => deltas["error_rate_pts"].clone(),
            };
            let mut w: Vec<f64> = ["avg_pct", "queries_per_request_pct", "db_ms_per_request_pct"].iter().filter_map(|k| adj[*k].as_f64()).collect();
            for (x, y) in [(qb, qf), (db, df)] {
                if let (Some(x), Some(y)) = (x, y)
                    && x <= 0.0
                    && y >= 1.0
                {
                    w.push(100.0);
                }
            }
            (adj, w.into_iter().fold(0.0f64, f64::max), l)
        });
        let verdict = match &mix {
            Some((adj, w, nums)) => {
                worst = *w;
                let mut v = verdict(b, f, adj, worst, &n1, low, Some(*nums));
                v.push_str(&format!(
                    " The traffic mix moved {}% between the sides, so these are like-for-like (each route against itself, weighted by the after side's traffic); the blended totals are in before/after and deltas.",
                    shift.round() as i64
                ));
                v
            }
            None => verdict(b, f, &deltas, worst, &n1, low, None),
        };
        let what = match group {
            Some(_) => {
                crate::obj! { "kind" => if kind == "request" { "route" } else { "job" }, "name" => f.name.clone().or_else(|| b.name.clone()).or_else(|| rb::present(Some(name)).then(|| name.to_string())), "group_hash" => group }
            }
            None => crate::obj! { "kind" => if kind == "request" { "all requests" } else { "all jobs" } },
        };
        let url = match group {
            Some(g) if kind == "request" => self.url(&format!("/apps/{}/envs/{}/requests/routes/{}", env.application_id, env.id, segment(g))),
            Some(g) => self.url(&format!("/apps/{}/envs/{}/jobs/classes/{}", env.application_id, env.id, segment(g))),
            None => self.url(&format!("/apps/{}/envs/{}", env.application_id, env.id)),
        };
        let side_json = |s: &Side, m: &Measured| {
            let mut v = m.to_json();
            db::merge_into(&mut v, "side", Value::from(s.label.clone()));
            if let Value::Object(meta) = &s.meta {
                for (k, x) in meta {
                    db::merge_into(&mut v, k, x.clone());
                }
            }
            if let Some(id) = s.meta.get("deploy_id").and_then(Value::as_i64) {
                db::merge_into(&mut v, "url", self.url(&format!("/apps/{}/envs/{}/deploys/{id}", env.application_id, env.id)));
            }
            v
        };
        let (bj, fj) = (side_json(before, b), side_json(after, f));
        let mut v = crate::obj! {
            "verdict" => verdict, "low_samples" => low, "worst_regression_pct" => jf(rb::round(worst, 1)),
            "compared" => what, "by" => if by_mark { "mark" } else { "deploy" }, "url" => url,
            "before" => bj, "after" => fj, "deltas" => deltas,
            "n_plus_ones" => n1, "query_shapes" => shapes,
        };
        if group.is_none() && b.count > 0 && f.count > 0 {
            let mut m = crate::obj! { "shift_pct" => jf(rb::round(shift, 1)), "moved_most" => mix_movers(b, f), "statuses" => status_shares(b, f) };
            if let Some((adj, _, l)) = &mix {
                db::merge_into(
                    &mut m,
                    "like_for_like",
                    crate::obj! {
                        "avg_ms" => crate::obj! { "before" => jf(rb::round(l.avg.0, 1)), "after" => jf(rb::round(l.avg.1, 1)) },
                        "queries_per_request" => crate::obj! { "before" => opt(l.queries.0.map(|x| rb::round(x, 1))), "after" => opt(l.queries.1.map(|x| rb::round(x, 1))) },
                        "deltas" => adj.clone(),
                    },
                );
            }
            db::merge_into(&mut v, "traffic_mix", m);
        }
        if low {
            db::merge_into(
                &mut v,
                "low_samples_hint",
                Value::from(format!(
                    "A side has fewer than {MIN_SAMPLES} executions (before {}, after {}): its percentiles are not stable. Make more requests (or wait for traffic) and compare again; queries_per_request and the N+1s are still exact for the executions there are.",
                    b.count, f.count
                )),
            );
        }
        db::merge_into(
            &mut v,
            "next",
            Value::from("get_execution on an after-side execution for detail; explain_route for where the after side's time goes now."),
        );
        v
    }
}

/// The deploys a side can be: the recorded ones, newest first, and (read
/// only when those cannot answer: a 30-day GROUP BY) the deploy values the
/// executions carry, oldest first.
struct Deploys<'a> {
    conn: &'a Connection,
    kind: &'a str,
    group: Option<&'a str>,
    now: i64,
    recorded: Vec<Deploy>,
    seen: Option<Vec<(String, i64, i64)>>,
}

impl Deploys<'_> {
    fn seen(&mut self) -> rusqlite::Result<&[(String, i64, i64)]> {
        if self.seen.is_none() {
            self.seen = Some(seen_deploys(self.conn, self.kind, self.group, self.now)?);
        }
        Ok(self.seen.as_deref().unwrap_or_default())
    }

    /// The side `v` names: an exact deploy value or name first, else the one
    /// deploy it is a prefix (7+ characters) of; recorded deploys before
    /// the seen values. An ambiguous prefix names none.
    fn find(&mut self, v: &str) -> rusqlite::Result<Option<Side>> {
        let loose = |d: &Deploy, v: &str| v.len() >= 7 && (d.deploy.starts_with(v) || d.ref_.as_deref().is_some_and(|r| r.starts_with(v)));
        if let Some(d) = super::answers::find_deploy(&self.recorded, v, loose) {
            return Ok(Some(recorded_side(d, &self.recorded, self.now)));
        }
        let seen = self.seen()?;
        if let Some(s) = seen.iter().find(|(d, ..)| d == v) {
            return Ok(Some(seen_side(s)));
        }
        let mut prefixed = seen.iter().filter(|(d, ..)| v.len() >= 7 && d.starts_with(v));
        Ok(match (prefixed.next(), prefixed.next()) {
            (Some(s), None) => Some(seen_side(s)),
            _ => None,
        })
    }

    fn no_deploy(&mut self, v: &str) -> rusqlite::Result<Value> {
        let seen = self.seen()?.to_vec();
        Ok(no_deploy(v, &self.recorded, &seen))
    }

    /// Newest first: the recorded deploys, or with fewer than two of them
    /// the seen values.
    fn order(&mut self) -> rusqlite::Result<Vec<Side>> {
        if self.recorded.len() >= 2 {
            return Ok(self.recorded.iter().map(|d| recorded_side(d, &self.recorded, self.now)).collect());
        }
        Ok(self.seen()?.iter().rev().map(seen_side).collect())
    }
}

/// The two deploys' sides: `before`/`after` when given, else the latest
/// deploy and the one before it. Deploys come from the control rows, or,
/// when fewer than two are recorded, from the deploy values the executions
/// carry (development, an app without a deploy hook).
fn deploy_sides(conn: &Connection, a: &Args, deploys: &[Deploy], kind: &str, group: Option<&str>, now: i64) -> rusqlite::Result<Result<(Side, Side), Value>> {
    let mut d = Deploys { conn, kind, group, now, recorded: recent_deploys(deploys.to_vec()), seen: None };
    let (b, f) = (a.presence("before"), a.presence("after"));
    let b_side = match &b {
        Some(v) => match d.find(v)? {
            Some(s) => Some(s),
            None => return Ok(Err(d.no_deploy(v)?)),
        },
        None => None,
    };
    let f_side = match &f {
        Some(v) => match d.find(v)? {
            Some(s) => Some(s),
            None => return Ok(Err(d.no_deploy(v)?)),
        },
        None => None,
    };
    if let (Some(b), Some(f)) = (&b_side, &f_side) {
        return Ok(Ok((clone_side(b), clone_side(f))));
    }
    // Defaults: the latest and the one before it, or the one before
    // `after`, or the one after `before`.
    let order = d.order()?;
    Ok(match (b_side, f_side) {
        (Some(bs), None) => match order.iter().position(|s| s.label == bs.label).and_then(|i| i.checked_sub(1)).and_then(|j| order.get(j)) {
            Some(n) => Ok((bs, clone_side(n))),
            None => Err(json!({"error": format!("no deploy after {}", bs.label)})),
        },
        (None, Some(fs)) => match order.iter().position(|s| s.label == fs.label).and_then(|i| order.get(i + 1)) {
            Some(p) => Ok((clone_side(p), fs)),
            None => Err(json!({"error": format!("no deploy before {}", fs.label)})),
        },
        _ if order.len() < 2 => Err(json!({"error": "fewer than two deploys to compare",
            "hint": "Each record carries the deploy (release) the app reports; there is only one in the last 30 days. To compare before and after a change without a deploy (development: Rails reloads code without changing it), pass since: the execution_id of the last request before the change, or the time you made it."})),
        _ => Ok((clone_side(&order[1]), clone_side(&order[0]))),
    })
}

fn clone_side(s: &Side) -> Side {
    Side { label: s.label.clone(), deploys: s.deploys.clone(), range: s.range, exclusive_to: s.exclusive_to, meta: s.meta.clone() }
}

/// A recorded deploy's side: the executions stamped with its value (as the
/// gem shortens a SHA: 12 characters) from its deploy time to the next.
fn recorded_side(d: &Deploy, all: &[Deploy], now: i64) -> Side {
    let next = all.iter().filter(|o| o.deployed_at > d.deployed_at).map(|o| o.deployed_at).min();
    let mut values = vec![d.deploy.clone()];
    let sha = |s: &str| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit());
    for v in [Some(d.deploy.as_str()), d.ref_.as_deref()].into_iter().flatten() {
        if sha(v) && !values.contains(&v[..12].to_string()) {
            values.push(v[..12].to_string());
        }
    }
    Side {
        label: d.deploy.clone(),
        deploys: values,
        // Records stamped with the deploy, wherever they fall: a rolling
        // deploy overlaps the previous one.
        range: (d.deployed_at - DAY, next.map_or(now, |n| n + DAY)),
        exclusive_to: false,
        meta: crate::obj! {
            "deploy" => d.deploy.clone(), "ref" => d.ref_.as_deref().map(|r| rb::first_chars(r, 12).to_string()), "name" => d.name.clone(),
            "deployed_at" => time_s(Some(d.deployed_at)), "deploy_id" => d.id,
        },
    }
}

/// (deploy, first seen, last seen) of the deploy values the window's
/// executions carry, oldest first.
fn seen_deploys(conn: &Connection, kind: &str, group: Option<&str>, now: i64) -> rusqlite::Result<Vec<(String, i64, i64)>> {
    let mut sql = String::from(
        "SELECT \"executions\".\"deploy\", MIN(occurred_at), MAX(occurred_at) FROM \"executions\" WHERE \"executions\".\"kind\" = ? AND \"executions\".\"occurred_at\" BETWEEN ? AND ? AND \"executions\".\"deploy\" IS NOT NULL",
    );
    let mut binds: Vec<rusqlite::types::Value> = vec![kind.to_string().into(), qt(now - 30 * DAY).into(), qt(now).into()];
    if let Some(g) = group {
        sql.push_str(" AND \"executions\".\"group_hash\" = ?");
        binds.push(g.to_string().into());
    }
    sql.push_str(" GROUP BY \"executions\".\"deploy\" ORDER BY 2");
    let t = |v: &rusqlite::types::Value| if let rusqlite::types::Value::Text(s) = v { crate::time::parse_db_time(s).unwrap_or(0) } else { 0 };
    Ok(db::query(conn, &sql, params_from_iter(binds))?.iter().filter_map(|r| r.s("deploy").map(|d| (d.to_string(), t(r.at(1)), t(r.at(2))))).collect())
}

fn seen_side((deploy, first, last): &(String, i64, i64)) -> Side {
    Side {
        label: deploy.clone(),
        deploys: vec![deploy.clone()],
        range: (*first, *last),
        exclusive_to: false,
        meta: crate::obj! { "deploy" => deploy.clone(), "first_seen" => time_s(Some(*first)), "last_seen" => time_s(Some(*last)), "deploy_recorded" => false },
    }
}

fn no_deploy(v: &str, recorded: &[Deploy], seen: &[(String, i64, i64)]) -> Value {
    let mut known: Vec<String> = recorded.iter().take(10).map(|d| d.deploy.clone()).collect();
    for (d, ..) in seen.iter().rev() {
        if known.len() >= 15 {
            break;
        }
        if !known.contains(d) {
            known.push(d.clone());
        }
    }
    json!({"error": format!("no deploy {}", ruby_q(v)), "hint": "Pass a deploy value, a ref prefix (7+ characters) or a deploy name from recent_deploys, or since for a time or execution mark.", "known": known})
}

fn ruby_q(s: &str) -> String {
    format!("{s:?}")
}

/// One side's numbers, and its sample's query shapes and N+1s.
struct Measured {
    name: Option<String>,
    count: usize,
    errors: usize,
    p50: Option<f64>,
    p95: Option<f64>,
    avg: Option<f64>,
    queries: Option<f64>,
    db_ms: Option<f64>,
    truncated: bool,
    sampled: usize,
    /// group_hash -> (runs, total ms, sql, source)
    shapes: HashMap<String, (usize, f64, String, Option<String>)>,
    /// (group_hash, source) -> (sql, executions it fired in)
    n_plus_ones: NPlusOnes,
    /// "group_hash status-class" for requests, group_hash for jobs -> its
    /// executions on this side, for the traffic mix.
    groups: HashMap<String, Group>,
    /// Requests by status class ("2xx", "3xx", ...): a shift between
    /// sides (sign-in redirects turning into page loads) moves the blend.
    statuses: HashMap<&'static str, usize>,
}

/// One route's or job class's executions on a side.
#[derive(Default, Clone)]
struct Group {
    name: Option<String>,
    count: usize,
    duration_ms: f64,
    queries: f64,
    queries_n: usize,
    db_ms: f64,
    db_n: usize,
}

impl Measured {
    fn error_rate(&self) -> Option<f64> {
        (self.count > 0).then(|| self.errors as f64 / self.count as f64)
    }

    fn to_json(&self) -> Value {
        let r = |x: Option<f64>, n| opt(x.map(|x| rb::round(x, n)));
        let mut v = crate::obj! {
            "count" => self.count, "errors" => self.errors,
            "error_rate_pct" => opt(self.error_rate().map(|e| rb::round(e * 100.0, 2))),
            "p50_ms" => r(self.p50, 1), "p95_ms" => r(self.p95, 1), "avg_ms" => r(self.avg, 1),
            "queries_per_request" => r(self.queries, 1), "db_ms_per_request" => r(self.db_ms, 1),
            "shapes_sampled_from" => self.sampled,
        };
        if self.truncated {
            db::merge_into(&mut v, "rows_read", Value::from(SIDE_ROWS));
        }
        v
    }
}

fn opt(x: Option<f64>) -> Value {
    x.map_or(Value::Null, jf)
}

fn str_of(v: &Value) -> String {
    v.as_str().map_or_else(|| v.to_string(), str::to_string)
}

/// 1 when the side has an execution of `kind` (and `group`), else 0: a
/// LIMIT 1 probe on the kind/occurred_at index, for around_busy_deploy.
fn measure_count(conn: &Connection, kind: &str, group: Option<&str>, side: &Side) -> rusqlite::Result<usize> {
    let mut sql = String::from("SELECT 1 FROM \"executions\" WHERE \"executions\".\"kind\" = ?");
    let mut binds: Vec<rusqlite::types::Value> = vec![kind.to_string().into()];
    if let Some(g) = group {
        sql.push_str(" AND \"executions\".\"group_hash\" = ?");
        binds.push(g.to_string().into());
    }
    sql.push_str(" AND \"executions\".\"occurred_at\" >= ? AND \"executions\".\"occurred_at\" <= ? LIMIT 1");
    binds.push(qt(side.range.0).into());
    binds.push(qt(side.range.1).into());
    Ok(db::query(conn, &sql, params_from_iter(binds))?.len())
}

fn measure(conn: &Connection, kind: &str, group: Option<&str>, side: &Side) -> rusqlite::Result<Measured> {
    let mut sql = String::from(
        "SELECT \"executions\".\"execution_id\", \"executions\".\"name\", \"executions\".\"group_hash\", \"executions\".\"duration\", \"executions\".\"status\", \"executions\".\"outcome\", \
         CASE WHEN \"executions\".\"counters\" IS NULL THEN NULL ELSE COALESCE(json_extract(\"executions\".\"counters\", '$.queries'), 0) END AS queries, \
         json_extract(\"executions\".\"detail\", '$.db_runtime') AS db FROM \"executions\" WHERE \"executions\".\"kind\" = ?",
    );
    let mut binds: Vec<rusqlite::types::Value> = vec![kind.to_string().into()];
    if let Some(g) = group {
        sql.push_str(" AND \"executions\".\"group_hash\" = ?");
        binds.push(g.to_string().into());
    }
    if !side.deploys.is_empty() {
        sql.push_str(&format!(" AND \"executions\".\"deploy\" IN ({})", vec!["?"; side.deploys.len()].join(", ")));
        binds.extend(side.deploys.iter().map(|d| d.clone().into()));
    }
    let op = if side.exclusive_to { "<" } else { "<=" };
    sql.push_str(&format!(" AND \"executions\".\"occurred_at\" >= ? AND \"executions\".\"occurred_at\" {op} ?"));
    binds.push(qt(side.range.0).into());
    binds.push(qt(side.range.1).into());
    sql.push_str(&format!(" ORDER BY \"executions\".\"occurred_at\" DESC LIMIT {}", SIDE_ROWS + 1));
    let mut rows = db::query(conn, &sql, params_from_iter(binds))?;
    let truncated = rows.len() > SIDE_ROWS;
    rows.truncate(SIDE_ROWS);
    let errored = |r: &Rec| if kind == "request" { r.i("status").unwrap_or(0) >= 500 } else { r.s("outcome") == Some("failed") };
    let mut durations: Vec<f64> = rows.iter().map(|r| r.i("duration").unwrap_or(0) as f64 / 1000.0).collect();
    durations.sort_by(f64::total_cmp);
    let rank = |q: f64| -> Option<f64> {
        if durations.is_empty() {
            return None;
        }
        let i = ((durations.len() as f64 * q).ceil() as usize).clamp(1, durations.len()) - 1;
        Some(durations[i])
    };
    let mean = |xs: Vec<f64>| (!xs.is_empty()).then(|| xs.iter().sum::<f64>() / xs.len() as f64);
    let queries = mean(rows.iter().filter_map(|r| r.i("queries").map(|q| q as f64).or_else(|| r.f("queries"))).collect());
    let db_ms = mean(rows.iter().filter_map(|r| r.f("db")).collect());
    let ids: Vec<String> = rows.iter().take(SHAPE_SAMPLE).filter_map(|r| r.s("execution_id").map(str::to_string)).collect();
    let (shapes, n_plus_ones) = sample_shapes(conn, &ids)?;
    let mut groups: HashMap<String, Group> = HashMap::new();
    let mut statuses: HashMap<&'static str, usize> = HashMap::new();
    for r in &rows {
        // By route and status class: one route that answers a sign-in
        // redirect one side and a full page the other changed its traffic,
        // not its code (rebulk-system's org root: 2 queries vs 69).
        let key = if kind == "request" {
            format!("{} {}", r.s("group_hash").unwrap_or(""), status_class(r.i("status")))
        } else {
            r.s("group_hash").unwrap_or("").to_string()
        };
        let g = groups.entry(key).or_default();
        if g.name.is_none() {
            g.name = r.s("name").map(str::to_string);
        }
        g.count += 1;
        g.duration_ms += r.i("duration").unwrap_or(0) as f64 / 1000.0;
        if let Some(q) = r.i("queries").map(|q| q as f64).or_else(|| r.f("queries")) {
            g.queries += q;
            g.queries_n += 1;
        }
        if let Some(d) = r.f("db") {
            g.db_ms += d;
            g.db_n += 1;
        }
        if kind == "request" {
            *statuses.entry(status_class(r.i("status"))).or_default() += 1;
        }
    }
    Ok(Measured {
        name: rows.first().and_then(|r| r.s("name")).map(str::to_string),
        count: rows.len(),
        errors: rows.iter().filter(|r| errored(r)).count(),
        p50: rank(0.5),
        p95: rank(0.95),
        avg: mean(durations.clone()),
        queries,
        db_ms,
        truncated,
        sampled: ids.len(),
        shapes,
        n_plus_ones,
        groups,
        statuses,
    })
}

fn status_class(status: Option<i64>) -> &'static str {
    match status.unwrap_or(0) {
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        500..=599 => "5xx",
        _ => "other",
    }
}

/// How far the traffic moved between the sides, 0 (same mix) to 100
/// (nothing in common): half the summed difference of each route-and-
/// status-class's (or job class's) share of its side. The total variation
/// distance.
fn mix_shift(b: &Measured, f: &Measured) -> f64 {
    let (nb, nf) = (b.count.max(1) as f64, f.count.max(1) as f64);
    let keys: std::collections::BTreeSet<&String> = b.groups.keys().chain(f.groups.keys()).collect();
    keys.into_iter().map(|k| (b.groups.get(k).map_or(0, |g| g.count) as f64 / nb - f.groups.get(k).map_or(0, |g| g.count) as f64 / nf).abs()).sum::<f64>()
        * 50.0
}

/// Per-request numbers with each route held to its own before and after,
/// weighted by the after side's traffic (mix_adjusted).
#[derive(Clone, Copy)]
struct LikeForLike {
    avg: (f64, f64),
    queries: (Option<f64>, Option<f64>),
    db_ms: (Option<f64>, Option<f64>),
}

/// Per-request avg ms, queries and db ms with each route held to its own
/// before and after, weighted by the after side's traffic: what changed in
/// the code rather than in what was asked of it. Only routes on both sides
/// count; None when none are.
fn mix_adjusted(b: &Measured, f: &Measured) -> Option<LikeForLike> {
    let mut w = 0.0;
    let (mut ab, mut af, mut qb, mut qf, mut qw, mut db, mut df, mut dw) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    for (k, gf) in &f.groups {
        let Some(gb) = b.groups.get(k) else { continue };
        let n = gf.count as f64;
        w += n;
        ab += n * gb.duration_ms / gb.count as f64;
        af += n * gf.duration_ms / gf.count as f64;
        if gb.queries_n > 0 && gf.queries_n > 0 {
            qw += n;
            qb += n * gb.queries / gb.queries_n as f64;
            qf += n * gf.queries / gf.queries_n as f64;
        }
        if gb.db_n > 0 && gf.db_n > 0 {
            dw += n;
            db += n * gb.db_ms / gb.db_n as f64;
            df += n * gf.db_ms / gf.db_n as f64;
        }
    }
    let per = |x: f64, n: f64| (n > 0.0).then(|| x / n);
    (w > 0.0).then(|| LikeForLike { avg: (ab / w, af / w), queries: (per(qb, qw), per(qf, qw)), db_ms: (per(db, dw), per(df, dw)) })
}

/// The routes whose share of traffic moved most, for the mix note.
fn mix_movers(b: &Measured, f: &Measured) -> Vec<Value> {
    let (nb, nf) = (b.count.max(1) as f64, f.count.max(1) as f64);
    let keys: std::collections::BTreeSet<&String> = b.groups.keys().chain(f.groups.keys()).collect();
    let mut rows: Vec<(f64, Value)> = keys
        .into_iter()
        .map(|k| {
            let (gb, gf) = (b.groups.get(k), f.groups.get(k));
            let (cb, cf) = (gb.map_or(0, |g| g.count), gf.map_or(0, |g| g.count));
            let share = |c: usize, n: f64| rb::round(c as f64 * 100.0 / n, 1);
            let per_q = |g: Option<&Group>| g.filter(|g| g.queries_n > 0).map(|g| rb::round(g.queries / g.queries_n as f64, 1));
            let moved = (cb as f64 / nb - cf as f64 / nf).abs();
            let name = gf.and_then(|g| g.name.clone()).or_else(|| gb.and_then(|g| g.name.clone()));
            let (hash, class) = k.split_once(' ').map_or((k.as_str(), None), |(h, c)| (h, Some(c)));
            (
                moved,
                crate::obj! {
                    "name" => name, "group_hash" => hash, "status" => class,
                    "before_share_pct" => jf(share(cb, nb)), "after_share_pct" => jf(share(cf, nf)),
                    "before_queries_per_request" => opt(per_q(gb)), "after_queries_per_request" => opt(per_q(gf)),
                },
            )
        })
        .collect();
    rows.sort_by(|x, y| y.0.total_cmp(&x.0));
    rows.into_iter().take(3).map(|r| r.1).collect()
}

type Shapes = HashMap<String, (usize, f64, String, Option<String>)>;
type NPlusOnes = Vec<((Option<String>, Option<String>), (String, usize))>;

/// The sample's queries by shape, and its N+1s by (shape, line).
fn sample_shapes(conn: &Connection, ids: &[String]) -> rusqlite::Result<(Shapes, NPlusOnes)> {
    let mut shapes: Shapes = HashMap::new();
    let mut n1: NPlusOnes = vec![];
    for chunk in ids.chunks(50) {
        let ph = vec!["?"; chunk.len()].join(", ");
        let rows = db::query(
            conn,
            &format!(
                "SELECT \"queries\".\"group_hash\", COUNT(*), SUM(\"queries\".\"duration\"), MIN(\"queries\".\"source\"), \
                 COALESCE(NULLIF(MIN(\"queries\".\"sql\"), ''), (SELECT query_shapes.sql FROM query_shapes WHERE query_shapes.group_hash = \"queries\".\"group_hash\"), '') \
                 FROM \"queries\" WHERE \"queries\".\"execution_id\" IN ({ph}) GROUP BY \"queries\".\"group_hash\""
            ),
            params_from_iter(chunk.iter()),
        )?;
        for r in rows {
            let Some(g) = r.s("group_hash").map(str::to_string) else { continue };
            let n = int(r.at(1)) as usize;
            let ms = int(r.at(2)) as f64 / 1000.0;
            let e = shapes.entry(g).or_insert_with(|| (0, 0.0, text(r.at(4)).unwrap_or_default(), text(r.at(3))));
            e.0 += n;
            e.1 += ms;
        }
        for r in db::query(
            conn,
            &format!(
                "SELECT \"n_plus_ones\".\"group_hash\", \"n_plus_ones\".\"source\", \"n_plus_ones\".\"sql\" FROM \"n_plus_ones\" WHERE \"n_plus_ones\".\"execution_id\" IN ({ph})"
            ),
            params_from_iter(chunk.iter()),
        )? {
            let key = (r.s("group_hash").map(str::to_string), r.s("source").map(str::to_string));
            match n1.iter_mut().find(|(k, _)| *k == key) {
                Some((_, (_, n))) => *n += 1,
                None => n1.push((key, (r.s("sql").unwrap_or("").to_string(), 1))),
            }
        }
    }
    Ok((shapes, n1))
}

fn int(v: &rusqlite::types::Value) -> i64 {
    match v {
        rusqlite::types::Value::Integer(i) => *i,
        rusqlite::types::Value::Real(f) => *f as i64,
        _ => 0,
    }
}

fn text(v: &rusqlite::types::Value) -> Option<String> {
    if let rusqlite::types::Value::Text(t) = v { Some(t.clone()) } else { None }
}

/// Shapes that appeared, disappeared, or changed runs per request the most.
fn shape_diff(b: &Shapes, f: &Shapes, nb: usize, nf: usize) -> Value {
    let per = |n: usize, of: usize| if of == 0 { 0.0 } else { n as f64 / of as f64 };
    let row = |g: &str, sql: &str, source: &Option<String>, before: f64, after: f64| {
        crate::obj! {
            "group_hash" => g, "sql" => rb::first_chars(sql, 200), "source" => source.clone(),
            "before_per_request" => jf(rb::round(before, 1)), "after_per_request" => jf(rb::round(after, 1)),
        }
    };
    let mut appeared: Vec<(f64, Value)> = vec![];
    let mut disappeared: Vec<(f64, Value)> = vec![];
    let mut changed: Vec<(f64, Value)> = vec![];
    for (g, (n, _, sql, src)) in f {
        let after = per(*n, nf);
        match b.get(g) {
            None if nb > 0 => appeared.push((after, row(g, sql, src, 0.0, after))),
            None => {}
            Some((m, ..)) => {
                let before = per(*m, nb);
                if (after - before).abs() >= 1.0 && (after - before).abs() >= before * 0.1 {
                    changed.push(((after - before).abs(), row(g, sql, src, before, after)));
                }
            }
        }
    }
    for (g, (n, _, sql, src)) in b {
        if !f.contains_key(g) && nf > 0 {
            let before = per(*n, nb);
            disappeared.push((before, row(g, sql, src, before, 0.0)));
        }
    }
    let top = |mut v: Vec<(f64, Value)>| -> Vec<Value> {
        v.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1["group_hash"].as_str().cmp(&b.1["group_hash"].as_str())));
        v.into_iter().take(8).map(|x| x.1).collect()
    };
    crate::obj! { "changed" => top(changed), "appeared" => top(appeared), "disappeared" => top(disappeared) }
}

/// The N+1s only one side has (by shape and line), and those both have.
fn n_plus_one_diff(b: &NPlusOnes, f: &NPlusOnes) -> Value {
    let row = |(g, src): &(Option<String>, Option<String>), sql: &str, n: usize| {
        let s = crate::dashboard::pages::n_plus_one::suggestion(sql, src.as_deref());
        crate::obj! { "source" => src.clone(), "group_hash" => g.clone(), "sql" => rb::first_chars(sql, 160), "executions" => n, "fix" => s.get("code").cloned().unwrap_or(Value::Null) }
    };
    let has = |list: &NPlusOnes, k: &(Option<String>, Option<String>)| list.iter().any(|(x, _)| x == k);
    let gained: Vec<Value> = f.iter().filter(|(k, _)| !has(b, k)).map(|(k, (sql, n))| row(k, sql, *n)).collect();
    let lost: Vec<Value> = b.iter().filter(|(k, _)| !has(f, k)).map(|(k, (sql, n))| row(k, sql, *n)).collect();
    let still: Vec<Value> = f.iter().filter(|(k, _)| has(b, k)).map(|(k, (sql, n))| row(k, sql, *n)).collect();
    crate::obj! { "gained" => gained, "lost" => lost, "still" => still }
}

/// "Better: p95 -38% (3669 -> 480 ms), queries per request 826 -> 80, N+1
/// at customer_documents.rb:511 gone."
/// Each status class's share of the side's requests, before and after.
fn status_shares(b: &Measured, f: &Measured) -> Value {
    let (nb, nf) = (b.count.max(1) as f64, f.count.max(1) as f64);
    let mut out = serde_json::Map::new();
    for c in ["2xx", "3xx", "4xx", "5xx", "other"] {
        let (x, y) = (b.statuses.get(c).copied().unwrap_or(0), f.statuses.get(c).copied().unwrap_or(0));
        if x + y > 0 {
            out.insert(
                c.into(),
                crate::obj! { "before_pct" => jf(rb::round(x as f64 * 100.0 / nb, 1)), "after_pct" => jf(rb::round(y as f64 * 100.0 / nf, 1)) },
            );
        }
    }
    Value::Object(out)
}

/// `adjusted`: the like-for-like numbers when the traffic mix moved; the
/// verdict leads with avg then, since percentiles cannot be held route by
/// route.
fn verdict(b: &Measured, f: &Measured, d: &Value, worst: f64, n1: &Value, low: bool, adjusted: Option<LikeForLike>) -> String {
    if b.count == 0 || f.count == 0 {
        return format!(
            "Nothing to compare: the {} side has no executions{}.",
            if b.count == 0 { "before" } else { "after" },
            if f.count == 0 { " yet (make the request again, or wait for traffic after the deploy)" } else { "" }
        );
    }
    let mut parts: Vec<String> = vec![];
    let ms = |x: Option<f64>| x.map_or("?".into(), |x| format!("{}", x.round() as i64));
    if let Some(p) = d["p95_pct"].as_f64() {
        parts.push(format!("p95 {}{p}% ({} -> {} ms)", if p > 0.0 { "+" } else { "" }, ms(b.p95), ms(f.p95)));
    }
    if let (Some(l), Some(p)) = (adjusted, d["avg_pct"].as_f64()) {
        parts.push(format!("avg per request {}{p}% ({} -> {} ms)", if p > 0.0 { "+" } else { "" }, ms(Some(l.avg.0)), ms(Some(l.avg.1))));
    }
    let (qb, qf) = adjusted.map_or((b.queries, f.queries), |l| l.queries);
    if let (Some(x), Some(y)) = (qb, qf)
        && (y - x).abs() >= 1.0
    {
        parts.push(format!("queries per request {} -> {}", x.round() as i64, y.round() as i64));
    }
    if let Some(p) = d["db_ms_per_request_pct"].as_f64().filter(|p| p.abs() >= NOISE_PCT) {
        parts.push(format!("db time per request {}{p}%", if p > 0.0 { "+" } else { "" }));
    }
    if let Some(e) = d["error_rate_pts"].as_f64().filter(|e| e.abs() >= 1.0) {
        parts.push(format!("error rate {}{e} pts", if e > 0.0 { "+" } else { "" }));
    }
    let src = |r: &Value| r["source"].as_str().map_or_else(|| "an unknown line".to_string(), |s| s.rsplit('/').next().unwrap_or(s).to_string());
    for r in n1["lost"].as_array().into_iter().flatten().take(3) {
        parts.push(format!("N+1 at {} gone", src(r)));
    }
    for r in n1["gained"].as_array().into_iter().flatten().take(3) {
        parts.push(format!("new N+1 at {}", src(r)));
    }
    let improved = ["p95_pct", "queries_per_request_pct", "avg_pct"].iter().any(|k| d[*k].as_f64().is_some_and(|p| p <= -NOISE_PCT))
        || n1["lost"].as_array().is_some_and(|l| !l.is_empty())
        || d["error_rate_pts"].as_f64().is_some_and(|e| e <= -1.0);
    let regressed = worst >= NOISE_PCT || n1["gained"].as_array().is_some_and(|g| !g.is_empty()) || d["error_rate_pts"].as_f64().is_some_and(|e| e >= 1.0);
    let head = match (improved, regressed) {
        (true, false) => "Better",
        (false, true) => "Worse",
        (true, true) => "Mixed",
        (false, false) => "No clear change",
    };
    let mut s = format!("{head}: {}.", if parts.is_empty() { "the numbers moved less than 10%".to_string() } else { parts.join(", ") });
    if low {
        s.push_str(&format!(" Few samples (before {}, after {}): treat the percentiles as rough.", b.count, f.count));
    }
    if b.truncated || f.truncated {
        s.push_str(&format!(" A side had more than {SIDE_ROWS} executions: its numbers are of its newest {SIDE_ROWS}, and count is a floor."));
    }
    if n1["lost"].as_array().is_some_and(|l| !l.is_empty()) && f.sampled < f.count {
        s.push_str(&format!(" N+1s are compared over {} executions a side; one that fires only sometimes can be missed.", SHAPE_SAMPLE));
    }
    s
}
