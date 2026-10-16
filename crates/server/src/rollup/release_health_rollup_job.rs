//! Port of ReleaseHealthRollupJob (railwatch-cloud app/jobs/
//! release_health_rollup_job.rb): one hour of `sessions` rows collapsed per
//! session id (worst status, longest duration, first user), then per deploy.

use indexmap::{IndexMap, IndexSet};
use rusqlite::{Connection, params_from_iter};

use super::sqlval::{Sql, row_values};

const CRASHED: i64 = 3;
const ERRORED: i64 = 2;

fn rank(status: &Sql) -> i64 {
    match status.text() {
        Some(b"started") => 0,
        Some(b"ok") => 1,
        Some(b"errored") => 2,
        Some(b"crashed") => 3,
        _ => 0,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReleaseHealthRow {
    pub deploy: Vec<u8>,
    pub bucket: i64,
    pub sessions: i64,
    pub sessions_errored: i64,
    pub sessions_crashed: i64,
    pub users: i64,
    pub users_crashed: i64,
    pub duration_sum: i64,
    pub duration_count: i64,
}

struct Session {
    rank: i64,
    duration: Option<i64>,
    user: Option<Vec<u8>>,
}

/// The pluck, in the order `index_sessions_on_occurred_at` returns rows.
pub const SELECT: &str = "SELECT \"sessions\".\"deploy\", \"sessions\".\"session_id\", \"sessions\".\"status\", \"sessions\".\"duration\", \"sessions\".\"user_ref\" FROM \"sessions\" WHERE \"sessions\".\"occurred_at\" >= ?1 AND \"sessions\".\"occurred_at\" < ?2 AND \"sessions\".\"deploy\" IS NOT NULL ORDER BY \"sessions\".\"occurred_at\", \"sessions\".\"id\"";

pub fn aggregate(bucket: i64, rows: &[Vec<Sql>]) -> Vec<ReleaseHealthRow> {
    let mut by_deploy: IndexMap<&[u8], IndexMap<&[u8], Session>> = IndexMap::new();
    for r in rows {
        let sessions = by_deploy.entry(r[0].s()).or_default();
        let s = sessions.entry(r[1].s()).or_insert(Session { rank: 0, duration: None, user: None });
        s.rank = s.rank.max(rank(&r[2]));
        if let Some(d) = r[3].int() {
            s.duration = Some(s.duration.unwrap_or(0).max(d));
        }
        if s.user.is_none() {
            s.user = r[4].text().map(<[u8]>::to_vec);
        }
    }
    by_deploy
        .into_iter()
        .map(|(deploy, sessions)| {
            let durations: Vec<i64> = sessions.values().filter_map(|s| s.duration).collect();
            let users: IndexSet<&Vec<u8>> = sessions.values().filter_map(|s| s.user.as_ref()).collect();
            let crashed_users: IndexSet<&Vec<u8>> = sessions.values().filter(|s| s.rank == CRASHED).filter_map(|s| s.user.as_ref()).collect();
            ReleaseHealthRow {
                deploy: deploy.to_vec(),
                bucket,
                sessions: sessions.len() as i64,
                sessions_errored: sessions.values().filter(|s| s.rank == ERRORED).count() as i64,
                sessions_crashed: sessions.values().filter(|s| s.rank == CRASHED).count() as i64,
                users: users.len() as i64,
                users_crashed: crashed_users.len() as i64,
                duration_sum: durations.iter().sum(),
                duration_count: durations.len() as i64,
            }
        })
        .collect()
}

pub fn read(conn: &Connection, bucket: i64) -> rusqlite::Result<Vec<Vec<Sql>>> {
    let (from, to) = (crate::time::utc_seconds_string(bucket), crate::time::utc_seconds_string(bucket + 3600));
    let mut stmt = conn.prepare_cached(SELECT)?;
    let rows = stmt.query_map([from, to], |r| row_values(r, 5))?;
    rows.collect()
}

/// `perform(environment, bucket)`. `retention_floor` is
/// `retention_days.days.ago.utc.beginning_of_hour` in epoch seconds; a
/// pruned hour is never rebuilt. Returns None when skipped.
pub fn perform(read_conn: &Connection, write: &Connection, bucket: i64, retention_floor: i64) -> rusqlite::Result<Option<usize>> {
    if bucket < retention_floor {
        return Ok(None);
    }
    let rows = aggregate(bucket, &read(read_conn, bucket)?);
    write_hour(write, bucket, &rows)?;
    Ok(Some(rows.len()))
}

pub fn write_hour(conn: &Connection, bucket: i64, rows: &[ReleaseHealthRow]) -> rusqlite::Result<()> {
    let b = crate::time::utc_seconds_string(bucket);
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        conn.prepare_cached("DELETE FROM \"release_health\" WHERE \"release_health\".\"bucket\" = ?")?.execute([&b])?;
        if rows.is_empty() {
            return Ok(());
        }
        // One insert_all; release_health rows are 9 columns, so up to ~3,600
        // deploys in a statement. Sliced well under that.
        for slice in rows.chunks(1000) {
            let values = vec!["(?, ?, ?, ?, ?, ?, ?, ?, ?)"; slice.len()].join(", ");
            let sql = format!(
                "INSERT INTO \"release_health\" (\"deploy\",\"bucket\",\"sessions\",\"sessions_errored\",\"sessions_crashed\",\"users\",\"users_crashed\",\"duration_sum\",\"duration_count\") VALUES {values} ON CONFLICT  DO NOTHING"
            );
            let mut binds: Vec<rusqlite::types::Value> = Vec::new();
            for r in slice {
                binds.push(rusqlite::types::Value::Text(String::from_utf8_lossy(&r.deploy).into_owned()));
                binds.push(b.clone().into());
                for n in [r.sessions, r.sessions_errored, r.sessions_crashed, r.users, r.users_crashed, r.duration_sum, r.duration_count] {
                    binds.push(n.into());
                }
            }
            conn.prepare_cached(&sql)?.execute(params_from_iter(binds))?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => conn.execute_batch("COMMIT"),
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}
