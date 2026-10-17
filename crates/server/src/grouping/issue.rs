//! Port of the parts of railwatch-cloud's Issue model (app/models/issue.rb)
//! that background work and IssuesController use: record_occurrence! (with
//! the merge chain), resolve!/ignore!/reopen! (record_status_change!), the
//! priority/assignee update callbacks, merge_into!/unmerge!/split!,
//! activities, and fire_alerts!. Rows live in the control database's
//! `issues` / `issue_activities` (control_schema::issues).
//!
//! `Actor` is Current.user: None for background work (activities get a NULL
//! user_id, alerts a nil actor), Some for a person acting from the web.
//!
//! The application seam: issues and alert rules belong to an application.
//! When p3-control's schema is present (`environments.application_id`,
//! `applications`), those are used; until then an environment is its own
//! application (id = environment id) and issue numbers count per that id.

use rusqlite::{Connection, OptionalExtension, params};

use crate::rjson::{self, Value};
use crate::time;

pub const MERGE_CHAIN_LIMIT: usize = 10;
pub const STATUSES: &[&str] = &["open", "resolved", "ignored", "merged"];
pub const KINDS: &[&str] = &["exception", "performance", "anomaly"];
pub const PRIORITIES: &[&str] = &["low", "normal", "high", "urgent"];
/// Issue::SPLIT_SAMPLE: how many recent occurrences a split looks at.
pub const SPLIT_SAMPLE: usize = 200;

/// Current.user for model callbacks: the user's id and name.
#[derive(Debug, Clone, Copy)]
pub struct Actor<'a> {
    pub user_id: i64,
    pub name: &'a str,
}

/// Issue::InvalidMerge, or a database error underneath.
#[derive(Debug)]
pub enum MergeError {
    Invalid(String),
    Sql(rusqlite::Error),
}

impl From<rusqlite::Error> for MergeError {
    fn from(e: rusqlite::Error) -> Self {
        MergeError::Sql(e)
    }
}

impl std::fmt::Display for MergeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MergeError::Invalid(m) => f.write_str(m),
            MergeError::Sql(e) => e.fmt(f),
        }
    }
}

fn invalid<T>(m: impl Into<String>) -> Result<T, MergeError> {
    Err(MergeError::Invalid(m.into()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    New,
    Regressed,
    Seen,
}

#[derive(Debug, Clone)]
pub struct Issue {
    pub id: i64,
    pub application_id: i64,
    pub environment_id: i64,
    pub number: i64,
    pub group_hash: String,
    pub kind: String,
    pub status: String,
    pub priority: String,
    pub title: String,
    pub culprit: Option<String>,
    pub occurrences: i64,
    pub affected_users: i64,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
    pub resolved_in_deploy: Option<String>,
    pub merged_into_id: Option<i64>,
    pub sample: Value,
}

/// What record_occurrence! is told about one occurrence.
pub struct Occurrence<'a> {
    pub environment_id: i64,
    pub group_hash: &'a str,
    pub kind: &'a str,
    pub title: &'a str,
    pub culprit: Option<&'a str>,
    /// epoch microseconds
    pub occurred_at: i64,
    pub deploy: Option<&'a str>,
    pub sample: Value,
    pub source: Option<&'a str>,
}

const COLUMNS: &str = "id, application_id, environment_id, number, group_hash, kind, status, priority, title, culprit, occurrences, affected_users, first_seen_at, last_seen_at, resolved_in_deploy, merged_into_id, sample";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Issue> {
    let t = |s: String| time::parse_db_time(&s).unwrap_or(0);
    let sample: String = r.get(16)?;
    Ok(Issue {
        id: r.get(0)?,
        application_id: r.get(1)?,
        environment_id: r.get(2)?,
        number: r.get(3)?,
        group_hash: r.get(4)?,
        kind: r.get(5)?,
        status: r.get(6)?,
        priority: r.get(7)?,
        title: r.get(8)?,
        culprit: r.get(9)?,
        occurrences: r.get(10)?,
        affected_users: r.get(11)?,
        first_seen_at: t(r.get(12)?),
        last_seen_at: t(r.get(13)?),
        resolved_in_deploy: r.get(14)?,
        merged_into_id: r.get(15)?,
        sample: rjson::parse(sample.as_bytes()).unwrap_or(Value::Hash(rjson::Hash::new())),
    })
}

pub fn find(c: &Connection, id: i64) -> rusqlite::Result<Option<Issue>> {
    c.prepare_cached(&format!("SELECT {COLUMNS} FROM issues WHERE id = ?"))?.query_row([id], row).optional()
}

pub fn find_by_group(c: &Connection, environment_id: i64, group_hash: &str) -> rusqlite::Result<Option<Issue>> {
    c.prepare_cached(&format!("SELECT {COLUMNS} FROM issues WHERE environment_id = ? AND group_hash = ?"))?
        .query_row(params![environment_id, group_hash], row)
        .optional()
}

fn has_column(c: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut s = c.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
    let names: Vec<String> = s.query_map([], |r| r.get(1))?.collect::<rusqlite::Result<_>>()?;
    Ok(names.iter().any(|n| n == column))
}

/// The environment's application id (see the module note).
pub fn application_of(c: &Connection, environment_id: i64) -> rusqlite::Result<i64> {
    if has_column(c, "environments", "application_id")?
        && let Some(Some(id)) =
            c.query_row("SELECT application_id FROM environments WHERE id = ?", [environment_id], |r| r.get::<_, Option<i64>>(0)).optional()?
    {
        return Ok(id);
    }
    Ok(environment_id)
}

/// Application#next_issue_number!
fn next_issue_number(c: &Connection, application_id: i64) -> rusqlite::Result<i64> {
    if has_column(c, "applications", "issues_count")? {
        let n: Option<i64> = c
            .query_row("UPDATE applications SET issues_count = issues_count + 1 WHERE id = ? RETURNING issues_count", [application_id], |r| r.get(0))
            .optional()?;
        if let Some(n) = n {
            return Ok(n);
        }
    }
    c.query_row("SELECT COALESCE(MAX(number), 0) + 1 FROM issues WHERE application_id = ?", [application_id], |r| r.get(0))
}

/// Issue#key: "<issue_prefix>-<number>"; "APP" until applications exist.
pub fn key(c: &Connection, issue: &Issue) -> String {
    let prefix: Option<String> = if has_column(c, "applications", "issue_prefix").unwrap_or(false) {
        c.query_row("SELECT issue_prefix FROM applications WHERE id = ?", [issue.application_id], |r| r.get(0)).optional().ok().flatten()
    } else {
        None
    };
    format!("{}-{}", prefix.unwrap_or_else(|| "APP".into()), issue.number)
}

/// The environment's display name for alert payloads (Environment#name).
pub fn environment_name(c: &Connection, environment_id: i64) -> String {
    let col = if has_column(c, "environments", "name").unwrap_or(false) { "name" } else { "slug" };
    c.query_row(&format!("SELECT {col} FROM environments WHERE id = ?"), [environment_id], |r| r.get(0)).unwrap_or_default()
}

/// Environment#display_name: "<application name> / <environment name>".
pub fn environment_display_name(c: &Connection, environment_id: i64) -> String {
    c.query_row(
        "SELECT ap.name || ' / ' || e.name FROM environments e JOIN applications ap ON ap.id = e.application_id WHERE e.id = ?",
        [environment_id],
        |r| r.get(0),
    )
    .unwrap_or_else(|_| environment_name(c, environment_id))
}

pub fn now_db() -> String {
    time::quoted_date_micros(time::now_micros())
}

/// A Time inside a json column: Active Support's `as_json` (ISO 8601, ms).
pub fn json_time(micros: i64) -> Value {
    let s = micros.div_euclid(1_000_000);
    let ms = micros.rem_euclid(1_000_000) / 1000;
    Value::from(format!("{}.{ms:03}Z", time::utc_seconds_string(s).replacen(' ', "T", 1)))
}

pub fn add_activity(c: &Connection, issue_id: i64, kind: &str, data: Value) -> rusqlite::Result<()> {
    add_activity_by(c, issue_id, kind, None, data)
}

/// `activities.create!(kind:, user: Current.user, data:)`.
pub fn add_activity_by(c: &Connection, issue_id: i64, kind: &str, user_id: Option<i64>, data: Value) -> rusqlite::Result<()> {
    c.prepare_cached("INSERT INTO issue_activities (issue_id, kind, data, user_id, created_at) VALUES (?, ?, ?, ?, ?)")?.execute(params![
        issue_id,
        kind,
        rjson::generate(&data).unwrap_or_else(|_| "{}".into()),
        user_id,
        now_db()
    ])?;
    Ok(())
}

/// Issue.record_occurrence!: follows merges (at most MERGE_CHAIN_LIMIT
/// links, cycles refused), then creates, regresses or bumps the issue.
/// Must run inside the caller's transaction (the control connection is
/// single-writer, which is what `with_lock` gives Rails).
pub fn record_occurrence(c: &Connection, o: &Occurrence<'_>) -> Result<(Issue, Outcome), String> {
    let e = |e: rusqlite::Error| e.to_string();
    let Some(mut issue) = find_by_group(c, o.environment_id, o.group_hash).map_err(e)? else {
        return create(c, o).map_err(e);
    };
    let mut visited = std::collections::HashSet::new();
    for depth in 0..=MERGE_CHAIN_LIMIT {
        if !visited.insert(issue.id) {
            return Err("cycle detected in issue merge chain".into());
        }
        if issue.status == "merged" || issue.merged_into_id.is_some() {
            let next = issue.merged_into_id.ok_or("merged issue has no merge target")?;
            if depth == MERGE_CHAIN_LIMIT {
                return Err(format!("issue merge chain exceeds {MERGE_CHAIN_LIMIT} links"));
            }
            issue = find(c, next).map_err(e)?.ok_or("issue merge target no longer exists")?;
            continue;
        }
        return bump(c, issue, o).map_err(e);
    }
    unreachable!()
}

fn create(c: &Connection, o: &Occurrence<'_>) -> rusqlite::Result<(Issue, Outcome)> {
    let application_id = application_of(c, o.environment_id)?;
    let number = next_issue_number(c, application_id)?;
    let now = now_db();
    let at = time::quoted_date_micros(o.occurred_at);
    c.prepare_cached(
        "INSERT INTO issues (affected_users, application_id, created_at, culprit, environment_id, first_seen_at, group_hash, kind, last_seen_at, number, occurrences, priority, sample, source, status, title, updated_at)
         VALUES (0, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, 'normal', ?, ?, 'open', ?, ?)",
    )?
    .execute(params![
        application_id, now, o.culprit, o.environment_id, at, o.group_hash, o.kind, at, number,
        rjson::generate(&o.sample).unwrap_or_else(|_| "{}".into()), o.source, o.title, now
    ])?;
    let id = c.last_insert_rowid();
    // after_create :record_created_activity
    add_activity(c, id, "created", Value::Hash(rjson::Hash::new()))?;
    Ok((find(c, id)?.expect("just inserted"), Outcome::New))
}

fn bump(c: &Connection, mut issue: Issue, o: &Occurrence<'_>) -> rusqlite::Result<(Issue, Outcome)> {
    let outcome = if issue.status == "resolved" && issue.resolved_in_deploy.as_deref().is_none_or(|d| o.deploy.unwrap_or("") != d) {
        Outcome::Regressed
    } else {
        Outcome::Seen
    };
    let at = time::quoted_date_micros(o.occurred_at);
    if o.occurred_at > issue.last_seen_at {
        issue.last_seen_at = o.occurred_at;
    }
    let sample = rjson::generate(&o.sample).unwrap_or_else(|_| "{}".into());
    if outcome == Outcome::Regressed {
        c.prepare_cached(
            "UPDATE issues SET status = 'open', regressed_at = ?, resolved_at = NULL, resolved_in_deploy = NULL, occurrences = occurrences + 1, last_seen_at = ?, sample = ?, source = ?, updated_at = ? WHERE id = ?",
        )?
        .execute(params![at, time::quoted_date_micros(issue.last_seen_at), sample, o.source, now_db(), issue.id])?;
        add_activity(c, issue.id, "regressed", crate::rhash! { "at" => json_time(o.occurred_at) })?;
    } else {
        c.prepare_cached("UPDATE issues SET occurrences = occurrences + 1, last_seen_at = ?, sample = ?, source = ?, updated_at = ? WHERE id = ?")?
            .execute(params![time::quoted_date_micros(issue.last_seen_at), sample, o.source, now_db(), issue.id])?;
    }
    Ok((find(c, issue.id)?.expect("exists"), outcome))
}

/// Issue#resolve!(deploy: nil) from background work (no Current.user).
/// An already resolved issue is left alone (auto-resolve and the missed-run
/// detector rely on that: no resolved_at bump).
pub fn resolve(c: &Connection, issue: &Issue, deploy: Option<&str>) -> rusqlite::Result<bool> {
    if issue.status == "resolved" {
        return Ok(false);
    }
    resolve_by(c, issue, deploy, None)
}

/// Issue#resolve!(deploy:): resolved_at is set on every call, as update!
/// does; the activity and alert only when the status changed.
pub fn resolve_by(c: &Connection, issue: &Issue, deploy: Option<&str>, actor: Option<Actor<'_>>) -> rusqlite::Result<bool> {
    let now = now_db();
    c.prepare_cached("UPDATE issues SET status = 'resolved', resolved_at = ?, resolved_in_deploy = ?, updated_at = ? WHERE id = ?")?
        .execute(params![now, deploy, now, issue.id])?;
    record_status_change(c, issue, actor)
}

/// Issue#ignore!
pub fn ignore(c: &Connection, issue: &Issue, actor: Option<Actor<'_>>) -> rusqlite::Result<bool> {
    c.prepare_cached("UPDATE issues SET status = 'ignored', updated_at = ? WHERE id = ?")?.execute(params![now_db(), issue.id])?;
    record_status_change(c, issue, actor)
}

/// Issue#reopen!
pub fn reopen(c: &Connection, issue: &Issue, actor: Option<Actor<'_>>) -> rusqlite::Result<bool> {
    c.prepare_cached("UPDATE issues SET status = 'open', resolved_at = NULL, resolved_in_deploy = NULL, updated_at = ? WHERE id = ?")?
        .execute(params![now_db(), issue.id])?;
    record_status_change(c, issue, actor)
}

/// record_status_change!: `issue` is the row before the update. A changed
/// status gets a 'status' activity and fires resolved_issue/ignored_issue/
/// regressed_issue (open). Returns whether the status changed.
fn record_status_change(c: &Connection, before: &Issue, actor: Option<Actor<'_>>) -> rusqlite::Result<bool> {
    let after = find(c, before.id)?.expect("exists");
    if after.status == before.status {
        return Ok(false);
    }
    add_activity_by(c, after.id, "status", actor.map(|a| a.user_id), crate::rhash! { "from" => before.status.clone(), "to" => after.status.clone() })?;
    let event = match after.status.as_str() {
        "resolved" => Some("resolved_issue"),
        "ignored" => Some("ignored_issue"),
        "open" => Some("regressed_issue"),
        _ => None,
    };
    if let Some(event) = event {
        crate::alerts::rules::fire_for_issue_by(c, &after, event, actor.map(|a| a.name), rjson::Hash::new())?;
    }
    Ok(true)
}

/// `update!(priority:)` with after_update :record_priority_activity.
/// Returns false when the value is not one of PRIORITIES (the validation
/// failure update! would raise).
pub fn set_priority(c: &Connection, issue: &Issue, priority: &str, actor: Option<Actor<'_>>) -> rusqlite::Result<bool> {
    if !PRIORITIES.contains(&priority) {
        return Ok(false);
    }
    if priority != issue.priority {
        c.prepare_cached("UPDATE issues SET priority = ?, updated_at = ? WHERE id = ?")?.execute(params![priority, now_db(), issue.id])?;
        add_activity_by(c, issue.id, "priority", actor.map(|a| a.user_id), crate::rhash! { "from" => issue.priority.clone(), "to" => priority })?;
    }
    Ok(true)
}

/// `update!(assignee:)` with after_update :record_assignee_activity: an
/// 'assignee' activity on change, and assigned_issue when someone is
/// assigned. `to` is (user id, user name) of an account member, or None.
pub fn set_assignee(c: &Connection, issue: &Issue, to: Option<(i64, &str)>, actor: Option<Actor<'_>>) -> rusqlite::Result<()> {
    let from: Option<i64> = c.query_row("SELECT assignee_id FROM issues WHERE id = ?", [issue.id], |r| r.get(0))?;
    let to_id = to.map(|t| t.0);
    if from == to_id {
        return Ok(());
    }
    c.prepare_cached("UPDATE issues SET assignee_id = ?, updated_at = ? WHERE id = ?")?.execute(params![to_id, now_db(), issue.id])?;
    add_activity_by(c, issue.id, "assignee", actor.map(|a| a.user_id), crate::rhash! { "from_id" => from, "to_id" => to_id })?;
    if let Some((id, name)) = to {
        let extra = crate::alerts::as_hash(crate::rhash! { "assignee" => name, "assignee_id" => id });
        crate::alerts::rules::fire_for_issue_by(c, issue, "assigned_issue", actor.map(|a| a.name), extra)?;
    }
    Ok(())
}

/// Issue#merge_into!(target), inside the caller's transaction (the
/// single-writer control connection is the lock!). Both rows are re-read
/// first, as lock! reloads them.
pub fn merge_into(c: &Connection, source_id: i64, target_id: i64, actor: Option<Actor<'_>>) -> Result<(), MergeError> {
    let (Some(source), Some(target)) = (find(c, source_id)?, find(c, target_id)?) else {
        return invalid("both issues must be persisted before merging");
    };
    if target.environment_id != source.environment_id {
        return invalid("can only merge issues in the same environment");
    }
    if target.id == source.id {
        return invalid("cannot merge an issue into itself");
    }
    if source.status == "merged" || source.merged_into_id.is_some() {
        return invalid("cannot merge an issue that is already merged");
    }
    if target.status != "open" {
        return invalid("merge target must be open");
    }
    if target.merged_into_id.is_some() {
        return invalid("merge target must not itself be merged");
    }
    let now = now_db();
    c.prepare_cached("UPDATE comments SET issue_id = ? WHERE issue_id = ?")?.execute(params![target.id, source.id])?;
    c.prepare_cached("UPDATE issues SET occurrences = ?, affected_users = ?, first_seen_at = ?, last_seen_at = ?, updated_at = ? WHERE id = ?")?.execute(
        params![
            target.occurrences + source.occurrences,
            target.affected_users.max(source.affected_users),
            time::quoted_date_micros(target.first_seen_at.min(source.first_seen_at)),
            time::quoted_date_micros(target.last_seen_at.max(source.last_seen_at)),
            now,
            target.id
        ],
    )?;
    c.prepare_cached("UPDATE issues SET status = 'merged', merged_into_id = ?, updated_at = ? WHERE id = ?")?.execute(params![target.id, now, source.id])?;
    let uid = actor.map(|a| a.user_id);
    add_activity_by(c, source.id, "merge", uid, crate::rhash! { "target_id" => target.id, "target_key" => key(c, &target) })?;
    add_activity_by(
        c,
        target.id,
        "absorbed",
        uid,
        crate::rhash! { "source_id" => source.id, "source_key" => key(c, &source), "occurrences" => source.occurrences },
    )?;
    Ok(())
}

/// Issue#unmerge!: gives back the count that came along at merge time.
/// Chains must be unwound from the live end.
pub fn unmerge(c: &Connection, issue_id: i64, actor: Option<Actor<'_>>) -> Result<(), MergeError> {
    let Some(source) = find(c, issue_id)? else { return invalid("cannot unmerge an issue that is not merged") };
    let Some(target_id) = source.merged_into_id else { return invalid("cannot unmerge an issue that is not merged") };
    let Some(target) = find(c, target_id)? else { return invalid("issue merge changed; reload and try again") };
    if source.status != "merged" {
        return invalid("issue merge changed; reload and try again");
    }
    if target.status == "merged" || target.merged_into_id.is_some() {
        return invalid(format!("cannot unmerge this issue yet; unmerge {} first (top-down order)", key(c, &target)));
    }
    let now = now_db();
    c.prepare_cached("UPDATE issues SET occurrences = ?, updated_at = ? WHERE id = ?")?.execute(params![
        (target.occurrences - source.occurrences).max(0),
        now,
        target.id
    ])?;
    c.prepare_cached("UPDATE issues SET status = 'open', merged_into_id = NULL, updated_at = ? WHERE id = ?")?.execute(params![now, source.id])?;
    add_activity_by(c, source.id, "unmerge", actor.map(|a| a.user_id), crate::rhash! { "target_id" => target.id, "target_key" => key(c, &target) })?;
    Ok(())
}

/// Issue.split_group_hash: MD5 hex of group_hash + "\x1f" + message, first
/// 32 (the whole digest); the separator is one the gem never sends.
pub fn split_group_hash(group_hash: &str, message: &[u8]) -> String {
    use md5::Digest;
    let mut h = md5::Md5::new();
    h.update(group_hash.as_bytes());
    h.update(b"\x1f");
    h.update(message);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()[..32].to_string()
}

/// Issue#split!(by:). `rows` are the issue's most recent SPLIT_SAMPLE
/// telemetry exceptions, newest first (the caller reads them from the
/// tenant). Creates one issue per distinct message not already split off,
/// moves their counts, and returns the new issues' keys; empty when there
/// is nothing to split.
pub fn split(c: &Connection, issue: &Issue, by: &str, rows: &[crate::grouping::ExceptionRow], actor: Option<Actor<'_>>) -> rusqlite::Result<Vec<String>> {
    if by != "message" {
        return Ok(vec![]);
    }
    // group_by keeps first-seen order (Ruby hashes are ordered).
    let mut groups: indexmap::IndexMap<&[u8], Vec<&crate::grouping::ExceptionRow>> = indexmap::IndexMap::new();
    for r in rows {
        groups.entry(r.message.as_slice()).or_default().push(r);
    }
    if groups.len() < 2 {
        return Ok(vec![]);
    }
    let uid = actor.map(|a| a.user_id);
    let source_key = key(c, issue);
    let source: Option<String> = c.query_row("SELECT source FROM issues WHERE id = ?", [issue.id], |r| r.get(0))?;
    let mut created: Vec<(String, i64)> = vec![];
    for (message, message_rows) in groups {
        let gh = split_group_hash(&issue.group_hash, message);
        if find_by_group(c, issue.environment_id, &gh)?.is_some() {
            continue;
        }
        // max_by/min_by return the first of equal elements.
        let latest = message_rows.iter().copied().reduce(|a, b| if b.occurred_at > a.occurred_at { b } else { a }).expect("non-empty");
        let earliest = message_rows.iter().copied().reduce(|a, b| if b.occurred_at < a.occurred_at { b } else { a }).expect("non-empty");
        let mut title = latest.class_name.as_bytes().to_vec();
        title.extend_from_slice(b": ");
        title.extend_from_slice(&rjson::ruby::first_chars(message, 200));
        let sample = crate::rhash! {
            "exception_id" => latest.id, "handled" => latest.handled, "execution_id" => latest.execution_id.clone(),
            "execution_preview" => latest.execution_preview.clone(), "deploy" => latest.deploy.clone(),
            "fingerprint" => latest.fingerprint.clone(), "fingerprint_source" => latest.fingerprint_source.clone(),
        };
        let number = next_issue_number(c, issue.application_id)?;
        let now = now_db();
        let n = message_rows.len() as i64;
        c.prepare_cached(
            "INSERT INTO issues (affected_users, application_id, created_at, culprit, environment_id, first_seen_at, group_hash, kind, last_seen_at, number, occurrences, priority, sample, source, status, title, updated_at)
             VALUES (0, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'normal', ?, ?, 'open', ?, ?)",
        )?
        .execute(params![
            issue.application_id, now, issue.culprit, issue.environment_id, time::quoted_date_micros(earliest.occurred_at), gh, issue.kind,
            time::quoted_date_micros(latest.occurred_at), number, n, rjson::generate(&sample).unwrap_or_else(|_| "{}".into()), source,
            String::from_utf8_lossy(&title).into_owned(), now
        ])?;
        let id = c.last_insert_rowid();
        // after_create :record_created_activity, then split_off's own.
        add_activity_by(c, id, "created", uid, Value::Hash(rjson::Hash::new()))?;
        add_activity_by(c, id, "split", uid, crate::rhash! { "from_id" => issue.id, "from_key" => source_key.clone(), "occurrences" => n })?;
        let new = find(c, id)?.expect("just inserted");
        created.push((key(c, &new), n));
    }
    if created.is_empty() {
        return Ok(vec![]);
    }
    let moved: i64 = created.iter().map(|(_, n)| n).sum();
    let current: i64 = c.query_row("SELECT occurrences FROM issues WHERE id = ?", [issue.id], |r| r.get(0))?;
    c.prepare_cached("UPDATE issues SET occurrences = ?, updated_at = ? WHERE id = ?")?.execute(params![(current - moved).max(0), now_db(), issue.id])?;
    let keys: Vec<String> = created.into_iter().map(|(k, _)| k).collect();
    add_activity_by(
        c,
        issue.id,
        "split",
        uid,
        crate::rhash! { "by" => by, "occurrences" => moved, "into" => keys.iter().map(|k| Value::from(k.clone())).collect::<Vec<_>>() },
    )?;
    Ok(keys)
}
