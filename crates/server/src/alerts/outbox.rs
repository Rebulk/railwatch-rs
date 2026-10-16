//! The alert outbox: Alert (railwatch-cloud app/models/alert.rb),
//! DeliverAlertJob, ReconcileAlertDeliveriesJob and SummarizeAlertBurstJob,
//! as state transitions on the `alerts` rows. No job queue: a delivery loop
//! claims due rows with a lease, delivers, and finishes or schedules a retry.

use rusqlite::{Connection, OptionalExtension, params};

use crate::grouping::issue;
use crate::rjson::{self, Value};
use crate::time;

pub const MAX_DELIVERY_ATTEMPTS: i64 = 5;
pub const DELIVERY_LEASE_SECS: i64 = 5 * 60;
/// Alert::RETRY_DELAYS
pub const RETRY_DELAYS_SECS: [i64; 4] = [60, 5 * 60, 15 * 60, 3600];
/// Alert::RECONCILE_WINDOW
pub const RECONCILE_WINDOW_SECS: i64 = 3600;
pub const BATCH_SIZE: i64 = 200;
pub const MAX_COLLAPSED_PER_SUMMARY: i64 = 5_000;

fn at(offset_secs: i64) -> String {
    time::quoted_date_micros(time::now_micros() + offset_secs * 1_000_000)
}

fn uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Alert.create! (assign_delivery_metadata: delivery_key, next_delivery_at
/// for pending). Returns the id.
#[allow(clippy::too_many_arguments)]
pub fn create(
    c: &Connection,
    alert_rule_id: Option<i64>,
    integration_id: i64,
    event: &str,
    issue_id: Option<i64>,
    payload: &Value,
    status: &str,
    burst_key: Option<&str>,
) -> rusqlite::Result<i64> {
    let now = issue::now_db();
    let next = if status == "pending" { Some(now.clone()) } else { None };
    c.prepare_cached(
        "INSERT INTO alerts (alert_rule_id, burst_key, created_at, delivery_attempts, delivery_key, event, integration_id, issue_id, next_delivery_at, payload, status, updated_at)
         VALUES (?, ?, ?, 0, ?, ?, ?, ?, ?, ?, ?, ?)",
    )?
    .execute(params![
        alert_rule_id, burst_key, now, uuid(), event, integration_id, issue_id, next,
        rjson::generate(payload).unwrap_or_else(|_| "{}".into()), status, now
    ])?;
    Ok(c.last_insert_rowid())
}

/// A "Send test" alert: no rule, no issue, created already holding a
/// delivery lease (attempt 1), so only the caller delivers it.
pub fn create_claimed(c: &Connection, integration_id: i64, event: &str, payload: &Value) -> rusqlite::Result<Claimed> {
    let id = create(c, None, integration_id, event, None, payload, "delivering", None)?;
    let lease = uuid();
    c.execute(
        "UPDATE alerts SET delivery_attempts = 1, delivery_lease_id = ?, delivery_lease_expires_at = ? WHERE id = ?",
        params![lease, at(DELIVERY_LEASE_SECS), id],
    )?;
    let delivery_key: String = c.query_row("SELECT delivery_key FROM alerts WHERE id = ?", [id], |r| r.get(0))?;
    Ok(Claimed { id, lease, event: event.into(), integration_id, issue_id: None, payload: payload.clone(), delivery_key, attempts: 1 })
}

/// Alert#deliver!'s rescue: failed at once (no retry), error on the alert
/// and the integration.
pub fn fail_now(c: &Connection, a: &Claimed, message: &str) -> rusqlite::Result<()> {
    let message: String = message.chars().take(1000).collect();
    let now = at(0);
    c.execute(
        "UPDATE alerts SET status = 'failed', error = ?, next_delivery_at = NULL, delivery_lease_id = NULL, delivery_lease_expires_at = NULL,
           delivery_enqueued_until = NULL, updated_at = ? WHERE id = ? AND delivery_lease_id = ?",
        params![message, now, a.id, a.lease],
    )?;
    c.execute("UPDATE integrations SET last_error = ? WHERE id = ?", params![message, a.integration_id])?;
    Ok(())
}

/// Alert#retry_delivery!: a failed alert gets a fresh attempt budget and is
/// due now. False for any other status (sent/skipped are not replayed).
/// `delivery_enqueued_until = now` stands for Rails' explicit enqueue: it
/// lets claim_due take the alert even when it is older than the reconcile
/// window.
pub fn retry(c: &Connection, id: i64) -> rusqlite::Result<bool> {
    let now = at(0);
    let n = c.execute(
        "UPDATE alerts SET status = 'pending', error = NULL, delivery_attempts = 0, next_delivery_at = ?1, delivery_enqueued_until = ?1,
           delivery_lease_id = NULL, delivery_lease_expires_at = NULL, updated_at = ?1
         WHERE id = ?2 AND status = 'failed'",
        params![now, id],
    )?;
    Ok(n == 1)
}

#[derive(Debug, Clone)]
pub struct Claimed {
    pub id: i64,
    pub lease: String,
    pub event: String,
    pub integration_id: i64,
    pub issue_id: Option<i64>,
    pub payload: Value,
    pub delivery_key: String,
    pub attempts: i64,
}

/// Recovers stale leases (ReconcileAlertDeliveriesJob#recover_stale_deliveries),
/// then claims up to `limit` due alerts (due_for_delivery + claim_delivery!),
/// in one transaction.
///
/// Rails' RECONCILE_WINDOW only bounds the reconciler; an alert already in
/// its retry schedule (attempts > 0) or explicitly retried was enqueued as a
/// job and is delivered whatever its age. With no job queue here, those two
/// cases are let through the window explicitly; otherwise the fifth attempt
/// (due ~81 min after creation) would never be claimed.
pub fn claim_due(c: &Connection, limit: i64) -> rusqlite::Result<Vec<Claimed>> {
    c.execute_batch("BEGIN IMMEDIATE")?;
    let r = (|| {
        let now = at(0);
        // recover_stale_delivery!: past the budget -> failed, else pending now.
        c.execute(
            "UPDATE alerts SET status = 'failed', error = COALESCE(NULLIF(error, ''), 'Delivery lease expired before completion'), next_delivery_at = NULL,
               delivery_lease_id = NULL, delivery_lease_expires_at = NULL, delivery_enqueued_until = NULL, updated_at = ?1
             WHERE status = 'delivering' AND delivery_lease_expires_at <= ?1 AND delivery_attempts >= ?2",
            params![now, MAX_DELIVERY_ATTEMPTS],
        )?;
        c.execute(
            "UPDATE alerts SET status = 'pending', error = COALESCE(NULLIF(error, ''), 'Delivery lease expired before completion'), next_delivery_at = ?1,
               delivery_lease_id = NULL, delivery_lease_expires_at = NULL, delivery_enqueued_until = NULL, updated_at = ?1
             WHERE status = 'delivering' AND delivery_lease_expires_at <= ?1",
            [&now],
        )?;
        let ids: Vec<i64> = {
            let mut s = c.prepare_cached(
                "SELECT id FROM alerts WHERE status = 'pending'
                 AND (created_at >= ? OR delivery_attempts > 0 OR delivery_enqueued_until IS NOT NULL)
                 AND (next_delivery_at IS NULL OR next_delivery_at <= ?)
                 AND (delivery_enqueued_until IS NULL OR delivery_enqueued_until <= ?) ORDER BY next_delivery_at, id LIMIT ?",
            )?;
            s.query_map(params![at(-RECONCILE_WINDOW_SECS), now, now, limit], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        let mut out = Vec::new();
        for id in ids {
            let lease = uuid();
            c.execute(
                "UPDATE alerts SET status = 'delivering', delivery_attempts = delivery_attempts + 1, delivery_key = COALESCE(NULLIF(delivery_key, ''), ?),
                   delivery_lease_id = ?, delivery_lease_expires_at = ?, delivery_enqueued_until = NULL, updated_at = ? WHERE id = ?",
                params![uuid(), lease, at(DELIVERY_LEASE_SECS), now, id],
            )?;
            let claimed = c.query_row(
                "SELECT event, integration_id, issue_id, payload, delivery_key, delivery_attempts FROM alerts WHERE id = ?",
                [id],
                |r| {
                    let p: String = r.get(3)?;
                    Ok(Claimed {
                        id,
                        lease: lease.clone(),
                        event: r.get(0)?,
                        integration_id: r.get(1)?,
                        issue_id: r.get(2)?,
                        payload: rjson::parse(p.as_bytes()).unwrap_or(Value::Hash(rjson::Hash::new())),
                        delivery_key: r.get(4)?,
                        attempts: r.get(5)?,
                    })
                },
            )?;
            out.push(claimed);
        }
        Ok(out)
    })();
    finish_tx(c, r)
}

fn finish_tx<T>(c: &Connection, r: rusqlite::Result<T>) -> rusqlite::Result<T> {
    match r {
        Ok(v) => {
            c.execute_batch("COMMIT")?;
            Ok(v)
        }
        Err(e) => {
            let _ = c.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// finish_claim!: sent or skipped, if the lease is still ours.
pub fn finish(c: &Connection, a: &Claimed, outcome: &str) -> rusqlite::Result<bool> {
    let now = at(0);
    let sent_at = if outcome == "sent" { Some(now.clone()) } else { None };
    let n = c.execute(
        "UPDATE alerts SET status = ?, error = NULL, next_delivery_at = NULL, delivery_lease_id = NULL, delivery_lease_expires_at = NULL,
           delivery_enqueued_until = NULL, sent_at = COALESCE(?, sent_at), updated_at = ? WHERE id = ? AND status = 'delivering' AND delivery_lease_id = ?",
        params![outcome, sent_at, now, a.id, a.lease],
    )?;
    if n == 1 && outcome == "sent" {
        c.execute("UPDATE integrations SET last_delivered_at = ?, last_error = NULL WHERE id = ?", params![now, a.integration_id])?;
        if let Some(issue_id) = a.issue_id {
            let kind: Option<String> = c.query_row("SELECT kind FROM integrations WHERE id = ?", [a.integration_id], |r| r.get(0)).optional()?;
            issue::add_activity(c, issue_id, "alert", crate::rhash! { "event" => a.event.clone(), "integration" => kind })?;
        }
    }
    Ok(n == 1)
}

/// fail_claim!: retry later or give up after MAX_DELIVERY_ATTEMPTS.
pub fn fail(c: &Connection, a: &Claimed, message: &str) -> rusqlite::Result<Option<i64>> {
    let message: String = message.chars().take(1000).collect();
    let now = at(0);
    let retry = if a.attempts >= MAX_DELIVERY_ATTEMPTS { None } else { Some(RETRY_DELAYS_SECS[(a.attempts - 1).clamp(0, 3) as usize]) };
    let (status, next) = match retry {
        Some(s) => ("pending", Some(at(s))),
        None => ("failed", None),
    };
    let n = c.execute(
        "UPDATE alerts SET status = ?, error = ?, next_delivery_at = ?, delivery_lease_id = NULL, delivery_lease_expires_at = NULL,
           delivery_enqueued_until = NULL, updated_at = ? WHERE id = ? AND status = 'delivering' AND delivery_lease_id = ?",
        params![status, message, next, now, a.id, a.lease],
    )?;
    if n == 1 {
        c.execute("UPDATE integrations SET last_error = ? WHERE id = ?", params![message, a.integration_id])?;
    }
    Ok(retry)
}

/// SummarizeAlertBurstJob for every rule with collapsed alerts whose window
/// has closed (enqueue_overdue_bursts + the job). Returns summaries created.
pub fn summarize_bursts(c: &Connection) -> rusqlite::Result<usize> {
    let rule_ids: Vec<i64> = {
        let mut s = c.prepare_cached(
            "SELECT DISTINCT alert_rule_id FROM (SELECT alert_rule_id FROM alerts WHERE status = 'collapsed' AND created_at >= ? AND created_at <= ? AND alert_rule_id IS NOT NULL ORDER BY created_at LIMIT ?)",
        )?;
        s.query_map(params![at(-RECONCILE_WINDOW_SECS), at(-super::rules::BURST_WINDOW_SECS), BATCH_SIZE], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    let mut made = 0;
    for rule_id in rule_ids {
        c.execute_batch("BEGIN IMMEDIATE")?;
        let r = summarize_rule(c, rule_id);
        if finish_tx(c, r)? {
            made += 1;
        }
    }
    Ok(made)
}

fn summarize_rule(c: &Connection, rule_id: i64) -> rusqlite::Result<bool> {
    let collapsed: Vec<(i64, String, Value)> = {
        let mut s = c.prepare_cached("SELECT id, created_at, payload FROM alerts WHERE alert_rule_id = ? AND status = 'collapsed' ORDER BY created_at, id LIMIT ?")?;
        s.query_map(params![rule_id, MAX_COLLAPSED_PER_SUMMARY], |r| {
            let p: String = r.get(2)?;
            Ok((r.get(0)?, r.get(1)?, rjson::parse(p.as_bytes()).unwrap_or(Value::Nil)))
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    if collapsed.is_empty() {
        return Ok(false);
    }
    let (event, integration_id): (String, i64) = c.query_row("SELECT event, integration_id FROM alert_rules WHERE id = ?", [rule_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    // tally.max_by(&:last)&.first: the first environment to reach the top count.
    let mut tally: indexmap::IndexMap<Vec<u8>, usize> = indexmap::IndexMap::new();
    for (_, _, p) in &collapsed {
        if let Value::Str(e) = p.get("environment")
            && !rjson::ruby::blank(e) {
                *tally.entry(e.clone()).or_default() += 1;
            }
    }
    let mut env: Option<Vec<u8>> = None;
    let mut best = 0;
    for (k, n) in &tally {
        if *n > best {
            best = *n;
            env = Some(k.clone());
        }
    }
    let count = collapsed.len();
    let noun = match (event.as_str(), count) {
        ("regressed_issue", 1) => "regressed issue",
        ("regressed_issue", _) => "regressed issues",
        (_, 1) => "new issue",
        _ => "new issues",
    };
    let since = time::parse_db_time(&collapsed[0].1).unwrap_or(0) / 1_000_000;
    let hhmm = &time::utc_seconds_string(since)[11..16];
    let env_s = env.as_ref().map(|e| String::from_utf8_lossy(e).into_owned());
    let where_ = env_s.as_ref().map(|e| format!(" in {e}")).unwrap_or_default();
    let payload = crate::rhash! {
        "summary" => true,
        "count" => count,
        "environment" => env_s.clone(),
        "message" => format!("{count} more {noun}{where_} since {hhmm} UTC"),
    };
    let burst_key = format!("alert-rule/{rule_id}/first-collapsed/{}", collapsed[0].0);
    let summary = create(c, Some(rule_id), integration_id, &event, None, &payload, "pending", Some(&burst_key))?;
    let ids: Vec<String> = collapsed.iter().map(|(id, _, _)| id.to_string()).collect();
    c.execute(
        &format!("UPDATE alerts SET status = 'summarized', summary_alert_id = ?, updated_at = ? WHERE id IN ({})", ids.join(",")),
        params![summary, at(0)],
    )?;
    Ok(true)
}

/// When the outbox last delivered anything, and whether something has been
/// waiting longer than `stale_secs` (the dead-man switch's /fail signal).
pub fn stuck(c: &Connection, stale_secs: i64) -> rusqlite::Result<bool> {
    c.query_row(
        "SELECT EXISTS (SELECT 1 FROM alerts WHERE status IN ('pending', 'delivering') AND created_at <= ? AND created_at >= ?)",
        params![at(-stale_secs), at(-RECONCILE_WINDOW_SECS)],
        |r| r.get(0),
    )
}
