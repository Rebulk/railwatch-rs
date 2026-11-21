//! Exemplars: before a day's detail file is deleted, the child rows of the
//! executions worth opening later are copied into that day's core file, so
//! an old slow or failed trace still shows its queries, cache events, view
//! renders, logs and spans.
//!
//! Chosen per (kind, group_hash, hour) of the day's executions: the
//! `slowest` by duration (ties by id), plus every errored one, by the same
//! error predicate the rollups use (rollup_job.rs `errored`): request
//! status >= 500, job/task/channel outcome "failed", command status != 0.
//!
//! Rows keep their ids (the core file's tables share the environment's id
//! space), so the reader's union view and `export` see each row once.
//! The copy is idempotent: INSERT OR IGNORE by id, and a marker row in the
//! core file's ar_internal_metadata records that the day is done.
//!
//! Besides the exemplars' own logs, every log line whose message starts with
//! one of `keep_logs` (RAILWATCH_KEEP_LOGS_MATCHING) is copied whatever its
//! execution: an app's audit or security lines then live as long as the
//! requests they describe, and stay findable by full-text search, while the
//! rest of the detail tier goes on schedule.

use std::path::Path;

use rusqlite::Connection;

use super::layout::{self, DETAIL_TABLES, Tier};

/// ar_internal_metadata key marking a core day whose exemplars are copied.
pub const DONE_KEY: &str = "railwatch_exemplars_copied";

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Copied {
    pub executions: i64,
    pub rows: i64,
}

/// The errored predicate of RollupJob, per execution kind.
const ERRORED: &str = "(CASE kind \
    WHEN 'request' THEN CAST(status AS INTEGER) >= 500 \
    WHEN 'command' THEN CAST(status AS INTEGER) != 0 \
    WHEN 'job_attempt' THEN outcome = 'failed' \
    WHEN 'scheduled_task' THEN outcome = 'failed' \
    WHEN 'channel_action' THEN outcome = 'failed' \
    ELSE 0 END)";

/// Copies the exemplars of `day` from its detail file into its core file.
pub fn copy(dir: &Path, day: &str, slowest: usize, keep_logs: &[String]) -> rusqlite::Result<Copied> {
    let core_path = layout::day_path(dir, Tier::Core, day);
    let detail_path = layout::day_path(dir, Tier::Detail, day);
    if !core_path.is_file() || !detail_path.is_file() {
        return Ok(Copied::default());
    }
    let core = crate::tenants::open_tenant(&core_path)?;
    if done(&core)? {
        return Ok(Copied::default());
    }
    core.execute("ATTACH DATABASE ?1 AS d", [format!("file:{}?mode=ro", detail_path.display())])?;
    core.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> rusqlite::Result<Copied> {
        core.execute_batch("CREATE TEMP TABLE IF NOT EXISTS exemplar_ids (execution_id TEXT PRIMARY KEY); DELETE FROM exemplar_ids;")?;
        let executions = core.execute(
            &format!(
                "INSERT OR IGNORE INTO exemplar_ids (execution_id)
                 SELECT execution_id FROM (
                   SELECT execution_id, {ERRORED} AS errored,
                          ROW_NUMBER() OVER (PARTITION BY kind, group_hash, substr(occurred_at, 1, 13) ORDER BY duration DESC, id DESC) AS rank
                   FROM main.executions WHERE execution_id IS NOT NULL)
                 WHERE errored OR rank <= ?1"
            ),
            [slowest as i64],
        )? as i64;
        let mut rows = 0i64;
        for t in DETAIL_TABLES {
            let cols: Vec<String> = core
                .prepare(&format!("SELECT name FROM pragma_table_info('{t}', 'd')"))?
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<_>>()?;
            let cols = cols.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(", ");
            let (keep, prefixes) = if *t == "logs" { kept_logs(keep_logs) } else { (String::new(), Vec::new()) };
            if *t == "logs" {
                // logs_fts is external-content: its rowids read through to
                // logs, so the new rows must be listed before they land.
                core.execute_batch("CREATE TEMP TABLE IF NOT EXISTS exemplar_logs (id INTEGER PRIMARY KEY); DELETE FROM exemplar_logs;")?;
                core.execute(
                    &format!(
                        "INSERT INTO exemplar_logs (id) SELECT id FROM d.logs
                         WHERE (execution_id IN (SELECT execution_id FROM exemplar_ids){keep}) AND id NOT IN (SELECT id FROM main.logs)"
                    ),
                    rusqlite::params_from_iter(prefixes.iter()),
                )?;
            }
            let n = core.execute(
                &format!(
                    "INSERT OR IGNORE INTO main.\"{t}\" ({cols}) SELECT {cols} FROM d.\"{t}\" \
                     WHERE execution_id IN (SELECT execution_id FROM exemplar_ids){keep} ORDER BY id"
                ),
                rusqlite::params_from_iter(prefixes.iter()),
            )? as i64;
            if *t == "logs" {
                core.execute("INSERT INTO logs_fts(rowid, message) SELECT id, message FROM main.logs WHERE id IN (SELECT id FROM exemplar_logs)", [])?;
            }
            rows += n;
        }
        core.execute(
            "INSERT OR REPLACE INTO ar_internal_metadata (key, value, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
            rusqlite::params![DONE_KEY, format!("{executions} executions, {rows} rows"), crate::time::quoted_date_micros(crate::time::now_micros())],
        )?;
        Ok(Copied { executions, rows })
    })();
    match result {
        Ok(c) => {
            core.execute_batch("COMMIT")?;
            core.execute_batch("DETACH DATABASE d")?;
            Ok(c)
        }
        Err(e) => {
            let _ = core.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// The `OR instr(message, ?) = 1` clause that keeps `prefixes` (one
/// placeholder each: an exact, case-sensitive prefix, nothing to escape), and
/// its values.
fn kept_logs(prefixes: &[String]) -> (String, Vec<String>) {
    let clause = prefixes.iter().map(|_| " OR instr(message, ?) = 1").collect::<String>();
    (clause, prefixes.to_vec())
}

pub fn done(core: &Connection) -> rusqlite::Result<bool> {
    core.query_row("SELECT EXISTS (SELECT 1 FROM ar_internal_metadata WHERE key = ?)", [DONE_KEY], |r| r.get(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::layout::{day_path, meta_path, open_or_create};

    #[test]
    fn keeps_slowest_and_errored_with_their_children() {
        let dir = std::env::temp_dir().join(format!("rw-exemplars-{}", crate::control::base58(8)));
        open_or_create(&meta_path(&dir)).unwrap();
        let day = "2026-10-01";
        let core = open_or_create(&day_path(&dir, Tier::Core, day)).unwrap();
        let detail = open_or_create(&day_path(&dir, Tier::Detail, day)).unwrap();
        // 10 requests of one route in one hour; #3 is a 500.
        for i in 1..=10i64 {
            let status = if i == 3 { 500 } else { 200 };
            core.execute(
                "INSERT INTO executions (id, execution_id, kind, name, group_hash, status, occurred_at, duration) VALUES (?, ?, 'request', 'GET /', 'g', ?, ?, ?)",
                rusqlite::params![i, format!("e{i}"), status, format!("{day} 10:{i:02}:00"), i as f64],
            )
            .unwrap();
            for q in 0..2 {
                detail
                    .execute(
                        "INSERT INTO queries (id, execution_id, sql, occurred_at, duration) VALUES (?, ?, 'SELECT 1', ?, 1.0)",
                        rusqlite::params![i * 10 + q, format!("e{i}"), format!("{day} 10:{i:02}:00")],
                    )
                    .unwrap();
            }
            detail
                .execute(
                    "INSERT INTO logs (id, level, message, execution_id, occurred_at) VALUES (?, 'info', 'hello', ?, ?)",
                    rusqlite::params![i, format!("e{i}"), format!("{day} 10:{i:02}:00")],
                )
                .unwrap();
        }
        detail.execute("INSERT INTO logs_fts(rowid, message) SELECT id, message FROM logs", []).unwrap();
        drop((core, detail));
        let c = copy(&dir, day, 3, &[]).unwrap();
        // slowest 3 (#10, #9, #8) + the errored #3
        assert_eq!(c, Copied { executions: 4, rows: 4 * 3 });
        assert_eq!(copy(&dir, day, 3, &[]).unwrap(), Copied::default(), "idempotent");
        let core = Connection::open(day_path(&dir, Tier::Core, day)).unwrap();
        let kept: Vec<String> =
            core.prepare("SELECT DISTINCT execution_id FROM queries ORDER BY 1").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
        assert_eq!(kept, vec!["e10", "e3", "e8", "e9"]);
        let fts: i64 = core.query_row("SELECT COUNT(*) FROM logs_fts WHERE logs_fts MATCH 'hello'", [], |r| r.get(0)).unwrap();
        assert_eq!(fts, 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A security line on an ordinary request (not an exemplar) must outlive
    /// the detail day, and stay findable by full-text search; other lines of
    /// that request, a line in another case, and a prefix found mid-message
    /// must not.
    #[test]
    fn keeps_log_lines_matching_a_prefix_whatever_their_execution() {
        let dir = std::env::temp_dir().join(format!("rw-exemplars-keep-{}", crate::control::base58(8)));
        open_or_create(&meta_path(&dir)).unwrap();
        let day = "2026-10-01";
        let core = open_or_create(&day_path(&dir, Tier::Core, day)).unwrap();
        let detail = open_or_create(&day_path(&dir, Tier::Detail, day)).unwrap();
        // Two requests of one route in one hour: with slowest = 1 only #2 is
        // an exemplar; #1 is the probe's ordinary request.
        for i in 1..=2i64 {
            core.execute(
                "INSERT INTO executions (id, execution_id, kind, name, group_hash, status, occurred_at, duration) VALUES (?, ?, 'request', 'POST /sign_in', 'g', 302, ?, ?)",
                rusqlite::params![i, format!("e{i}"), format!("{day} 10:0{i}:00"), i as f64],
            )
            .unwrap();
        }
        let lines: [(i64, Option<&str>, &str); 5] = [
            (1, Some("e1"), r#"{"event":"auth.login","ip":"203.0.113.9","outcome":"failed"}"#),
            (2, Some("e1"), "an ordinary line of the same request"),
            (3, None, r#"{"event":"mcp.call","ip":"203.0.113.9","tool":"rebulk_search_actions"}"#),
            (4, Some("e1"), r#"{"EVENT":"auth.login","ip":"203.0.113.9"}"#),
            (5, Some("e1"), r#"prefix later: {"event":"auth.login","ip":"203.0.113.9"}"#),
        ];
        for (id, exe, message) in lines {
            detail
                .execute(
                    "INSERT INTO logs (id, level, message, execution_id, occurred_at) VALUES (?, 'info', ?, ?, ?)",
                    rusqlite::params![id, message, exe, format!("{day} 10:01:00")],
                )
                .unwrap();
        }
        detail
            .execute(
                "INSERT INTO logs (id, level, message, execution_id, occurred_at) VALUES (6, 'info', 'exemplar line', 'e2', ?)",
                [format!("{day} 10:02:00")],
            )
            .unwrap();
        detail.execute("INSERT INTO logs_fts(rowid, message) SELECT id, message FROM logs", []).unwrap();
        drop((core, detail));

        let keep = vec![r#"{"event":"auth."#.to_string(), r#"{"event":"mcp."#.to_string()];
        copy(&dir, day, 1, &keep).unwrap();
        let core = Connection::open(day_path(&dir, Tier::Core, day)).unwrap();
        let ids: Vec<i64> = core.prepare("SELECT id FROM logs ORDER BY id").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
        assert_eq!(ids, vec![1, 3, 6], "the two security lines and the exemplar's line");
        let fts: Vec<i64> = core
            .prepare("SELECT rowid FROM logs_fts WHERE logs_fts MATCH '\"203.0.113.9\"' ORDER BY rowid")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(fts, vec![1, 3], "searchable by the address in the core file");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
