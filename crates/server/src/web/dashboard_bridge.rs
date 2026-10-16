//! Wires the dashboard module's page functions (crate::dashboard::pages) into
//! the environment routes: ControlData over the control DB, the read pool,
//! and the component -> function table.
//!
//! Issues come from the control `issues` table (written by exception
//! grouping); thresholds and anomaly rules still answer empty.

use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;

use crate::dashboard::control::{ControlData, Deploy, Issue};
use crate::dashboard::pages::*;
use crate::dashboard::{PageContext, PageError};

pub type PageFn = fn(&Connection, &PageContext, &dyn ControlData) -> Result<Value, PageError>;

/// The dashboard function for an Inertia component, as railwatch-cloud's
/// routes map them (one component can come from several routes).
pub fn page_fn(component: &str) -> Option<PageFn> {
    Some(match component {
        "overview/show" => overview_show as PageFn,
        "requests/index" => requests_index,
        "requests/route" => requests_route,
        "jobs/index" => jobs_index,
        "jobs/klass" => jobs_klass,
        "scheduled_tasks/index" => scheduled_tasks_index,
        "commands/index" => commands_index,
        "executions/show" => executions_show,
        "traces/show" => traces_show,
        "people/index" => people_index,
        "people/show" => people_show,
        "tenants/index" => tenants_index,
        "tenants/show" => tenants_show,
        "queries/index" => queries_index,
        "queries/show" => queries_show,
        "exceptions/index" => exceptions_index,
        "logs/index" => logs_index,
        "spans/index" => spans_index,
        "spans/show" => spans_show,
        "profiles/index" => profiles_index,
        "profiles/show" => profiles_show,
        "view_renders/index" => view_renders_index,
        "transactions/index" => transactions_index,
        "deprecations/index" => deprecations_index,
        "cache_events/index" => cache_events_index,
        "mails/index" => mails_index,
        "notifications/index" => notifications_index,
        "broadcasts/index" => broadcasts_index,
        "outgoing_requests/index" => outgoing_requests_index,
        "llm_calls/index" => llm_calls_index,
        "storage_ops/index" => storage_ops_index,
        "visits/index" => visits_index,
        "processes/index" => processes_index,
        "releases/index" => releases_index,
        "releases/show" => releases_show,
        "deploys/index" => deploys_index,
        "deploys/show" => deploys_show,
        "thresholds/index" => thresholds_index,
        "usage/show" => usage_show,
        _ => return None,
    })
}

/// One environment's control rows, read once per request.
pub struct DbControl {
    deploys: Vec<Deploy>,
    issues: Vec<Issue>,
    expected_servers: Vec<String>,
}

impl DbControl {
    pub fn load(c: &Connection, environment_id: i64) -> rusqlite::Result<DbControl> {
        let deploys = c
            .prepare(
                "SELECT id, deploy, ref, name, url, server, deployed_at, previous_ref, commits, detail \
                 FROM deploys WHERE environment_id = ? ORDER BY id",
            )?
            .query_map([environment_id], |r| {
                let commits: String = r.get(8)?;
                let detail: String = r.get(9)?;
                Ok(Deploy {
                    id: r.get(0)?,
                    deploy: r.get(1)?,
                    ref_: r.get(2)?,
                    name: r.get(3)?,
                    url: r.get(4)?,
                    server: r.get(5)?,
                    deployed_at: crate::time::parse_db_time(&r.get::<_, String>(6)?).unwrap_or(0),
                    previous_ref: r.get(7)?,
                    commits: serde_json::from_str(&commits).unwrap_or_default(),
                    detail: serde_json::from_str(&detail).unwrap_or_default(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let servers: Option<String> =
            c.query_row("SELECT expected_servers FROM environments WHERE id = ?", [environment_id], |r| r.get(0)).optional()?;
        let expected_servers = servers.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        // environment.issues (merged ones included, as the association is),
        // keyed "<issue_prefix>-<number>" like Issue#key.
        let issues = c
            .prepare(
                "SELECT i.id, ap.issue_prefix || '-' || i.number, i.title, i.kind, i.status, i.priority, i.occurrences, \
                        i.affected_users, i.culprit, i.group_hash, i.first_seen_at, i.last_seen_at, i.resolved_in_deploy, i.sample \
                 FROM issues i JOIN applications ap ON ap.id = i.application_id WHERE i.environment_id = ? ORDER BY i.id",
            )?
            .query_map([environment_id], |r| {
                let sample: String = r.get(13)?;
                Ok(Issue {
                    id: r.get(0)?,
                    key: r.get(1)?,
                    title: r.get(2)?,
                    kind: r.get(3)?,
                    status: r.get(4)?,
                    priority: r.get(5)?,
                    occurrences: r.get(6)?,
                    affected_users: r.get(7)?,
                    culprit: r.get(8)?,
                    group_hash: r.get(9)?,
                    first_seen_at: crate::time::parse_db_time(&r.get::<_, String>(10)?).unwrap_or(0),
                    last_seen_at: crate::time::parse_db_time(&r.get::<_, String>(11)?).unwrap_or(0),
                    resolved_in_deploy: r.get(12)?,
                    sample: serde_json::from_str(&sample).unwrap_or_default(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(DbControl { deploys, issues, expected_servers })
    }
}

impl ControlData for DbControl {
    fn deploys(&self) -> Vec<Deploy> {
        self.deploys.clone()
    }
    fn issues(&self) -> Vec<Issue> {
        self.issues.clone()
    }
    fn expected_servers(&self) -> Vec<String> {
        self.expected_servers.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{Control, create_account, create_environment, insert_application, insert_user};

    #[test]
    fn grouped_issues_reach_the_pages() {
        // The overview's "open issues" read ControlData::issues; an empty stub showed 0 while grouping had opened issues.
        let dir = std::env::temp_dir().join(format!("rw-bridge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let c = Control::open(&dir.join("control.sqlite3")).unwrap();
        let user = c.tx(|tx| insert_user(tx, "Ada", "ada@example.com", "correct horse battery", true)).unwrap();
        let account = c.tx(|tx| create_account(tx, "Acme", user)).unwrap();
        let app = c.tx(|tx| insert_application(tx, account, "Shop", "shop", "SHOP", None, "main")).unwrap();
        let (env, _) = c.tx(|tx| create_environment(tx, app, "production")).unwrap();
        let (other, _) = c.tx(|tx| create_environment(tx, app, "staging")).unwrap();
        c.with(|x| {
            for (e, n) in [(env, 7), (other, 8)] {
                x.execute(
                    "INSERT INTO issues (application_id, environment_id, number, title, group_hash, occurrences, first_seen_at, last_seen_at, created_at, updated_at, sample)
                     VALUES (?, ?, ?, 'Boom', 'g1', 3, '2026-10-14 03:00:00.000000', '2026-10-14 03:05:00.000000', '2026-10-14 03:00:00', '2026-10-14 03:05:00', '{\"a\":1}')",
                    [app, e, n],
                )?;
            }
            Ok(())
        })
        .unwrap();
        let issues = c.with(|x| DbControl::load(x, env)).unwrap().issues();
        assert_eq!(issues.len(), 1, "only this environment's issues");
        let i = &issues[0];
        assert_eq!((i.key.as_str(), i.status.as_str(), i.kind.as_str(), i.occurrences), ("SHOP-7", "open", "exception", 3));
        assert_eq!(i.last_seen_at - i.first_seen_at, 300_000_000);
        assert_eq!(i.sample["a"], 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
