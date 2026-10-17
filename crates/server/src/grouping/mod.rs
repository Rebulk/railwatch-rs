//! Exception grouping into issues: Ingest::ExceptionGrouping (railwatch-cloud
//! app/models/ingest/exception_grouping.rb), Issue (issue.rb) and
//! IngestFollowupReceipt (ingest_followup_receipt.rb).

pub mod issue;

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::rjson::{self, Value};
use crate::time;
use issue::{Occurrence, Outcome};

/// ExceptionGrouping::DEPLOY_GRACE, in microseconds.
pub const DEPLOY_GRACE: i64 = 3 * 60 * 1_000_000;
pub const DEPLOY_NETWORK_ERRORS: &[&str] = &["HttpNetworkError", "AxiosError", "InertiaException", "NetworkError", "TypeError:NetworkError"];

/// One telemetry exceptions row, the columns grouping reads.
#[derive(Debug, Clone)]
pub struct ExceptionRow {
    pub id: i64,
    pub group_hash: Option<String>,
    pub class_name: String,
    pub message: Vec<u8>,
    pub file: Option<String>,
    pub line: Option<i64>,
    pub occurred_at: i64,
    pub deploy: Option<String>,
    pub user_ref: Option<String>,
    pub source: Option<String>,
    pub handled: bool,
    pub execution_id: Option<String>,
    pub execution_preview: Option<String>,
    pub fingerprint: Value,
    pub fingerprint_source: Option<String>,
}

/// The columns `exception_row` reads, in its order.
pub const EXCEPTION_COLUMNS: &str = "id, group_hash, class_name, message, file, line, occurred_at, deploy, user_ref, source, handled, execution_id, execution_preview, fingerprint, fingerprint_source";

/// One exceptions row selected with EXCEPTION_COLUMNS.
pub fn exception_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ExceptionRow> {
    let at: String = r.get(6)?;
    let fp: Option<String> = r.get(13)?;
    Ok(ExceptionRow {
        id: r.get(0)?,
        group_hash: r.get(1)?,
        class_name: r.get(2)?,
        message: r.get_ref(3)?.as_bytes_or_null()?.map(<[u8]>::to_vec).unwrap_or_default(),
        file: r.get(4)?,
        line: r.get(5)?,
        occurred_at: time::parse_db_time(&at).unwrap_or(0),
        deploy: r.get(7)?,
        user_ref: r.get(8)?,
        source: r.get(9)?,
        handled: r.get::<_, Option<i64>>(10)?.unwrap_or(0) != 0,
        execution_id: r.get(11)?,
        execution_preview: r.get(12)?,
        fingerprint: fp.and_then(|f| rjson::parse(f.as_bytes()).ok()).unwrap_or(Value::Nil),
        fingerprint_source: r.get(14)?,
    })
}

/// `Telemetry::Exception.where(id: ids).to_a` (primary-key order).
pub fn load_exceptions(tenant: &Connection, ids: &[i64]) -> rusqlite::Result<Vec<ExceptionRow>> {
    let mut out = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(900) {
        let ph = vec!["?"; chunk.len()].join(",");
        let sql = format!("SELECT {EXCEPTION_COLUMNS} FROM exceptions WHERE id IN ({ph}) ORDER BY id");
        let mut s = tenant.prepare(&sql)?;
        let rows = s.query_map(params_from_iter(chunk), exception_row)?;
        for r in rows {
            out.push(r?);
        }
    }
    out.sort_by_key(|r| r.id);
    Ok(out)
}

/// The receipt a replayable delivery carries.
#[derive(Debug, Clone)]
pub struct Receipt {
    pub producer_id: String,
    pub delivery_id: String,
    /// epoch microseconds
    pub delivery_created_at: i64,
}

/// IngestFollowupReceipt.claim!: true exactly once per (env, delivery,
/// effect). Must run inside the transaction that performs the effect; reads
/// the floor in that transaction.
pub fn claim(c: &Connection, environment_id: i64, r: &Receipt, effect_key: &str) -> rusqlite::Result<bool> {
    let floor: Option<i64> = c
        .query_row("SELECT ingest_followup_gc_before FROM environments WHERE id = ?", [environment_id], |r| r.get::<_, Option<String>>(0))
        .optional()?
        .flatten()
        .and_then(|s| crate::time::parse_db_time(&s));
    if floor.is_some_and(|f| r.delivery_created_at < f) {
        return Ok(false);
    }
    let n = c
        .prepare_cached(
            "INSERT INTO ingest_followup_receipts (environment_id, producer_id, delivery_id, delivery_created_at, effect_key, created_at)
             VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (environment_id, producer_id, delivery_id, effect_key) DO NOTHING",
        )?
        .execute(params![environment_id, r.producer_id, r.delivery_id, time::quoted_date_micros(r.delivery_created_at), effect_key, issue::now_db()])?;
    Ok(n == 1)
}

/// IngestFollowupReceipt.prune!: up to `limit` receipts below the floor.
pub fn prune_receipts(c: &Connection, environment_id: i64, floor_micros: i64, limit: i64) -> rusqlite::Result<usize> {
    c.execute(
        "DELETE FROM ingest_followup_receipts WHERE id IN (SELECT id FROM ingest_followup_receipts WHERE environment_id = ? AND delivery_created_at < ? LIMIT ?)",
        params![environment_id, time::quoted_date_micros(floor_micros), limit],
    )
}

/// What one grouping pass did, for logs and tests.
#[derive(Debug, Default, PartialEq)]
pub struct Grouped {
    pub new: usize,
    pub regressed: usize,
    pub seen: usize,
    pub suppressed: usize,
    pub claimed_elsewhere: usize,
    /// Exceptions of groups past a new-issue cap, counted onto the overflow
    /// issue instead (still stored in the tenant with their own group hash).
    pub overflowed: usize,
}

/// Caps on new issues (Rust-only, docs/DIFFERENCES.md): group hashes are
/// client-chosen, so without them one ingest token could mint an issue per
/// exception in the shared control database. Past a cap, a group that has
/// no issue yet is counted onto the environment's overflow issue.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    /// New issues per environment in the trailing hour.
    pub new_issues_per_hour: usize,
    /// New groups per grouping pass (one batch).
    pub new_issues_per_batch: usize,
    /// New issues across all of an account's environments in the trailing
    /// hour, so creating environments does not multiply the cap above.
    pub new_issues_per_account_per_hour: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { new_issues_per_hour: 1_000, new_issues_per_batch: 200, new_issues_per_account_per_hour: 3_000 }
    }
}

static LIMITS: std::sync::OnceLock<Limits> = std::sync::OnceLock::new();

/// Sets the process's caps once, at startup (`serve`); otherwise the defaults.
pub fn set_limits(l: Limits) {
    let _ = LIMITS.set(l);
}

/// The overflow issue's group hash. Ingest only accepts 32 hex characters
/// for `_group`, so no client can address it directly.
pub const OVERFLOW_GROUP_HASH: &str = "railwatch:too-many-new-issues";
pub const OVERFLOW_TITLE: &str = "Too many new issues: further new exception groups are counted here";

/// `@environment.deploys.maximum(:deployed_at)` in microseconds.
fn last_deploy_at(control: &Connection, environment_id: i64) -> rusqlite::Result<Option<i64>> {
    let v: Option<String> = control.query_row("SELECT MAX(deployed_at) FROM deploys WHERE environment_id = ?", [environment_id], |r| r.get(0))?;
    Ok(v.and_then(|s| time::parse_db_time(&s)))
}

fn deploy_swap_noise(row: &ExceptionRow, last_deploy: Option<i64>) -> bool {
    if row.source.as_deref() != Some("browser") || !DEPLOY_NETWORK_ERRORS.contains(&row.class_name.as_str()) {
        return false;
    }
    last_deploy.is_some_and(|d| row.occurred_at >= d - DEPLOY_GRACE && row.occurred_at <= d + DEPLOY_GRACE)
}

/// update_affected_users' `COUNT(DISTINCT user_ref) ... WHERE group_hash = ?`,
/// kept per group so each call reads only the exceptions committed since the
/// last one instead of every retained occurrence (which made grouping a busy
/// issue cost O(occurrences) per batch; docs/PERF.md #4). Exact as long as
/// rows only ever arrive with larger ids (AUTOINCREMENT, one writer) and
/// none is deleted: the owner clears it whenever the tenant is pruned.
#[derive(Default)]
pub struct AffectedUsers {
    groups: HashMap<String, (i64, HashSet<Vec<u8>>)>,
}

impl AffectedUsers {
    pub fn clear(&mut self) {
        self.groups.clear();
    }

    fn count(&mut self, tenant: &Connection, group_hash: &str) -> rusqlite::Result<i64> {
        let (last_id, users) = self.groups.entry(group_hash.to_string()).or_default();
        // `+group_hash`: walk the new ids (rowid > ?), not the group's whole
        // index range, which `id > ?` cannot bound. Rows without a user are
        // read too, so the mark moves past them.
        let mut st = tenant.prepare_cached("SELECT id, user_ref FROM exceptions WHERE +group_hash = ? AND id > ?")?;
        let mut rows = st.query(params![group_hash, *last_id])?;
        while let Some(r) = rows.next()? {
            *last_id = (*last_id).max(r.get(0)?);
            // user_ref has TEXT affinity, so every stored value is TEXT (or a
            // BLOB). Anything else would need SQLite's cross-type equality:
            // count it the plain way and stop caching this group.
            match r.get_ref(1)? {
                ValueRef::Null => false,
                ValueRef::Text(t) => users.insert(t.to_vec()),
                _ => {
                    self.groups.remove(group_hash);
                    return tenant.query_row(
                        "SELECT COUNT(DISTINCT user_ref) FROM exceptions WHERE group_hash = ? AND user_ref IS NOT NULL",
                        [group_hash],
                        |r| r.get(0),
                    );
                }
            };
        }
        Ok(users.len() as i64)
    }
}

/// ExceptionGrouping#call. `control` must not be inside a transaction: each
/// group commits in its own (claim and effect together, as Rails does).
pub fn group(control: &Connection, tenant: &Connection, environment_id: i64, rows: Vec<ExceptionRow>, receipt: Option<&Receipt>) -> Result<Grouped, String> {
    group_with(control, tenant, environment_id, rows, receipt, &mut AffectedUsers::default())
}

/// `group`, counting affected users through the caller's `users`.
pub fn group_with(
    control: &Connection,
    tenant: &Connection,
    environment_id: i64,
    rows: Vec<ExceptionRow>,
    receipt: Option<&Receipt>,
    users: &mut AffectedUsers,
) -> Result<Grouped, String> {
    let limits = LIMITS.get().copied().unwrap_or_default();
    group_with_limits(control, tenant, environment_id, rows, receipt, users, limits)
}

/// `group_with` under explicit caps.
pub fn group_with_limits(
    control: &Connection,
    tenant: &Connection,
    environment_id: i64,
    rows: Vec<ExceptionRow>,
    receipt: Option<&Receipt>,
    users: &mut AffectedUsers,
    limits: Limits,
) -> Result<Grouped, String> {
    let e = |e: rusqlite::Error| e.to_string();
    let last_deploy = last_deploy_at(control, environment_id).map_err(e)?;
    let mut out = Grouped::default();
    let mut groups: IndexMap<String, Vec<ExceptionRow>> = IndexMap::new();
    for row in rows {
        if deploy_swap_noise(&row, last_deploy) {
            out.suppressed += 1;
            continue;
        }
        // A row with no group hash cannot become an issue (Followups filters
        // them; group_by would give them a nil key that record_occurrence! rejects).
        let Some(g) = row.group_hash.clone().filter(|g| !g.is_empty()) else { continue };
        groups.entry(g).or_default().push(row);
    }
    // New issues this environment created in the trailing hour, read once
    // and counted forward (only this pass creates issues meanwhile: the
    // control connection is the single writer).
    let hour_ago = time::quoted_date_micros(time::now_micros() - 3_600 * 1_000_000);
    let mut created_this_hour: usize = control
        .query_row("SELECT COUNT(*) FROM issues WHERE environment_id = ? AND created_at >= ?", params![environment_id, hour_ago], |r| r.get::<_, i64>(0))
        .map_err(e)? as usize;
    // The same for the environment's whole account.
    let mut account_created_this_hour: usize = control
        .query_row(
            "SELECT COUNT(*) FROM issues WHERE created_at >= ?2 AND environment_id IN (
               SELECT environments.id FROM environments JOIN applications ON applications.id = environments.application_id
               WHERE applications.account_id = (SELECT applications.account_id FROM environments JOIN applications ON applications.id = environments.application_id WHERE environments.id = ?1))",
            params![environment_id, hour_ago],
            |r| r.get::<_, i64>(0),
        )
        .map_err(e)? as usize;
    let mut created_this_batch = 0usize;
    for (group_hash, group) in groups {
        control.execute_batch("BEGIN IMMEDIATE").map_err(e)?;
        let result = (|| -> Result<Option<(Outcome, bool)>, String> {
            if let Some(r) = receipt
                && !claim(control, environment_id, r, &format!("group:{group_hash}")).map_err(e)?
            {
                return Ok(None);
            }
            let capped = (created_this_batch >= limits.new_issues_per_batch
                || created_this_hour >= limits.new_issues_per_hour
                || account_created_this_hour >= limits.new_issues_per_account_per_hour)
                && issue::find_by_group(control, environment_id, &group_hash).map_err(e)?.is_none();
            let target = if capped { OVERFLOW_GROUP_HASH } else { group_hash.as_str() };
            record(control, tenant, environment_id, target, &group, users).map(|o| Some((o, capped)))
        })();
        match result {
            Ok(outcome) => {
                control.execute_batch("COMMIT").map_err(e)?;
                match outcome {
                    None => out.claimed_elsewhere += 1,
                    // The overflow issue's own creation counts toward the
                    // hour (it is a row), not toward `new`.
                    Some((o, true)) => {
                        out.overflowed += group.len();
                        if o == Outcome::New {
                            created_this_hour += 1;
                            account_created_this_hour += 1;
                        }
                    }
                    Some((Outcome::New, false)) => {
                        out.new += 1;
                        created_this_batch += 1;
                        created_this_hour += 1;
                        account_created_this_hour += 1;
                    }
                    Some((Outcome::Regressed, false)) => out.regressed += 1,
                    Some((Outcome::Seen, false)) => out.seen += 1,
                }
            }
            Err(err) => {
                let _ = control.execute_batch("ROLLBACK");
                return Err(err);
            }
        }
    }
    if out.overflowed > 0 {
        tracing::warn!(
            environment = environment_id,
            overflowed = out.overflowed,
            per_hour = limits.new_issues_per_hour,
            per_batch = limits.new_issues_per_batch,
            per_account_per_hour = limits.new_issues_per_account_per_hour,
            "too many new exception groups: counted onto the overflow issue instead of creating issues"
        );
    }
    Ok(out)
}

fn record(
    control: &Connection,
    tenant: &Connection,
    environment_id: i64,
    group_hash: &str,
    group: &[ExceptionRow],
    users: &mut AffectedUsers,
) -> Result<Outcome, String> {
    let e = |e: rusqlite::Error| e.to_string();
    // max_by(&:occurred_at): the first of equal maxima.
    let latest = group.iter().fold(&group[0], |a, b| if b.occurred_at > a.occurred_at { b } else { a });
    let overflow = group_hash == OVERFLOW_GROUP_HASH;
    let title = if overflow {
        OVERFLOW_TITLE.to_string()
    } else {
        let mut t = latest.class_name.as_bytes().to_vec();
        t.extend_from_slice(b": ");
        t.extend_from_slice(&rjson::ruby::first_chars(&latest.message, 200));
        String::from_utf8_lossy(&t).into_owned()
    };
    let culprit = match (&latest.file, latest.line) {
        (Some(f), Some(l)) => Some(format!("{f}:{l}")),
        (Some(f), None) => Some(f.clone()),
        (None, Some(l)) => Some(l.to_string()),
        (None, None) => None,
    }
    .filter(|c| !rjson::ruby::blank(c.as_bytes()));
    let sample = crate::rhash! {
        "exception_id" => latest.id,
        "handled" => latest.handled,
        "execution_id" => latest.execution_id.clone(),
        "execution_preview" => latest.execution_preview.clone(),
        "deploy" => latest.deploy.clone(),
        "fingerprint" => latest.fingerprint.clone(),
        "fingerprint_source" => latest.fingerprint_source.clone(),
    };
    let (issue, outcome) = issue::record_occurrence(
        control,
        &Occurrence {
            environment_id,
            group_hash,
            kind: "exception",
            title: &title,
            culprit: culprit.as_deref(),
            occurred_at: latest.occurred_at,
            deploy: latest.deploy.as_deref(),
            sample,
            source: latest.source.as_deref(),
        },
    )?;
    if group.len() > 1 {
        control
            .execute("UPDATE issues SET occurrences = occurrences + ?, updated_at = ? WHERE id = ?", params![group.len() as i64 - 1, issue::now_db(), issue.id])
            .map_err(e)?;
    }
    // update_affected_users (no tenant row carries the overflow hash)
    if !overflow {
        let users = users.count(tenant, group_hash).map_err(e)?;
        control.execute("UPDATE issues SET affected_users = ? WHERE id = ?", params![users, issue.id]).map_err(e)?;
    }
    // alert(issue, outcome)
    let event = match outcome {
        Outcome::New => Some("new_issue"),
        Outcome::Regressed => Some("regressed_issue"),
        Outcome::Seen => None,
    };
    if let Some(event) = event {
        let issue = issue::find(control, issue.id).map_err(e)?.expect("exists");
        crate::alerts::rules::fire_for_issue_plain(control, &issue, event).map_err(e)?;
    }
    Ok(outcome)
}
