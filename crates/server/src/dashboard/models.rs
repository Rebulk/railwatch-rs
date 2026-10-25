//! The row shapes models render (`as_row` and friends), read off `Rec`s.

use std::collections::HashMap;

use rusqlite::Connection;
use serde_json::Value;

use super::db::{self, Rec, jf, ji, js, jt};
use super::rb;

/// `Execution#as_row`.
pub fn execution_row(r: &Rec) -> Value {
    let kind = r.s("kind").unwrap_or("");
    let mut row = crate::obj! {
        "execution_id" => js(r.s("execution_id")), "kind" => js(r.s("kind")), "name" => js(r.s("name")),
        "status" => ji(r.i("status")), "outcome" => js(r.s("outcome")),
        "duration" => jf(rb::round(r.i("duration").unwrap_or(0) as f64 / 1000.0, 2)),
        "occurred_at" => jt(r.t("occurred_at")), "deploy" => js(r.s("deploy")), "server" => js(r.s("server")),
        "user_ref" => js(r.s("user_ref")), "tenant" => js(r.s("app_tenant")), "exception_preview" => js(r.s("exception_preview")),
    };
    let extra = match kind {
        "request" => crate::obj! {
            "method" => js(r.s("method")), "inertia_component" => js(r.s("inertia_component")),
            // The gem sends only the counters that fired: no "queries" key is
            // a request that ran none. A row with no counters at all is still nil.
            "queries" => match r.json("counters") {
                Value::Object(c) => c.get("queries").cloned().unwrap_or(Value::from(0)),
                _ => Value::Null,
            },
        },
        "job_attempt" => crate::obj! {
            "queue" => js(r.s("queue")), "attempt" => ji(r.i("attempt")), "job_id" => js(r.s("job_id")),
            "queue_latency" => r.i("queue_latency").map_or(Value::Null, |q| jf(rb::round(q as f64 / 1000.0, 1))),
        },
        "scheduled_task" => crate::obj! { "task_key" => js(r.s("task_key")) },
        _ => Value::Object(Default::default()),
    };
    row = db::merge(row, extra);
    row
}

/// `Query#as_row` (rows loaded with_sql: `resolved_sql`).
pub fn query_row(r: &Rec) -> Value {
    crate::obj! {
        "id" => ji(r.i("id")), "group_hash" => js(r.s("group_hash")),
        "sql" => rb::first_chars(r.s("resolved_sql").unwrap_or(""), 2000), "name" => js(r.s("name")),
        "duration" => jf(rb::round(r.i("duration").unwrap_or(0) as f64 / 1000.0, 3)), "occurred_at" => jt(r.t("occurred_at")),
        "deploy" => js(r.s("deploy")), "execution_id" => js(r.s("execution_id")), "execution_preview" => js(r.s("execution_preview")),
        "source" => js(r.s("source")), "connection" => js(r.s("connection")), "role" => js(r.s("role")),
        "adapter" => js(r.s("adapter")), "row_count" => ji(r.i("row_count")), "tenant" => js(r.s("app_tenant")),
        "user_ref" => js(r.s("user_ref")), "explain" => rb::present(r.s("explain")),
    }
}

/// `Query::SQL` (the select list with_sql adds).
pub const QUERY_WITH_SQL: &str = "\"queries\".*, COALESCE(NULLIF(queries.sql, ''), (SELECT query_shapes.sql FROM query_shapes WHERE query_shapes.group_hash = queries.group_hash), '') AS resolved_sql";

/// `Log#as_row`.
pub fn log_row(r: &Rec) -> Value {
    crate::obj! {
        "id" => ji(r.i("id")), "level" => js(r.s("level")), "message" => rb::first_chars(r.s("message").unwrap_or(""), 2000),
        "tags" => r.json("tags"), "occurred_at" => jt(r.t("occurred_at")), "deploy" => js(r.s("deploy")),
        "execution_id" => js(r.s("execution_id")), "execution_source" => js(r.s("execution_source")),
        "execution_preview" => js(r.s("execution_preview")), "source" => js(r.s("source")), "tenant" => js(r.s("app_tenant")),
        "user_ref" => js(r.s("user_ref")), "context" => js(r.s("context")),
    }
}

/// `Exception#as_row`.
pub fn exception_row(r: &Rec) -> Value {
    crate::obj! {
        "id" => ji(r.i("id")), "class_name" => js(r.s("class_name")), "message" => rb::first_chars(r.s("message").unwrap_or(""), 500),
        "handled" => r.b("handled").map_or(Value::Null, Value::Bool), "severity" => js(r.s("severity")), "source" => js(r.s("source")),
        "file" => js(r.s("file")), "line" => ji(r.i("line")), "occurred_at" => jt(r.t("occurred_at")), "deploy" => js(r.s("deploy")),
        "execution_id" => js(r.s("execution_id")), "execution_source" => js(r.s("execution_source")),
        "execution_preview" => js(r.s("execution_preview")), "user_ref" => js(r.s("user_ref")), "tenant" => js(r.s("app_tenant")),
        "group_hash" => js(r.s("group_hash")),
    }
}

/// `x.duration_ms.round(n)`, nil when duration is nil.
pub fn dur(r: &Rec, n: i32) -> Value {
    r.i("duration").map_or(Value::Null, |d| jf(rb::round(d as f64 / 1000.0, n)))
}

/// `x.duration_ms` (unrounded), nil when nil.
pub fn dur_ms(r: &Rec) -> Option<f64> {
    r.i("duration").map(|d| d as f64 / 1000.0)
}

pub struct Person {
    pub ref_: String,
    pub name: Option<String>,
    pub email: Option<String>,
    pub app_tenant: Option<String>,
    pub first_seen_at: Option<i64>,
    pub last_seen_at: Option<i64>,
}

impl Person {
    fn from(r: &Rec) -> Person {
        Person {
            ref_: r.s("ref").unwrap_or("").to_string(),
            name: r.s("name").map(str::to_string),
            email: r.s("email").map(str::to_string),
            app_tenant: r.s("app_tenant").map(str::to_string),
            first_seen_at: r.t("first_seen_at"),
            last_seen_at: r.t("last_seen_at"),
        }
    }

    /// `name.presence || email.presence || ref`
    pub fn display_name(&self) -> String {
        if rb::present(self.name.as_deref()) {
            self.name.clone().unwrap()
        } else if rb::present(self.email.as_deref()) {
            self.email.clone().unwrap()
        } else {
            self.ref_.clone()
        }
    }
}

/// `Person.where(ref: refs).index_by(&:ref)`.
pub fn people_by_ref(conn: &Connection, refs: &[String]) -> rusqlite::Result<HashMap<String, Person>> {
    let mut out = HashMap::new();
    for chunk in refs.chunks(500) {
        let ph = vec!["?"; chunk.len()].join(",");
        for r in db::query(conn, &format!("SELECT \"people\".* FROM \"people\" WHERE \"people\".\"ref\" IN ({ph})"), rusqlite::params_from_iter(chunk))? {
            let p = Person::from(&r);
            out.insert(p.ref_.clone(), p);
        }
    }
    Ok(out)
}

pub fn person_by_ref(conn: &Connection, ref_: &str) -> rusqlite::Result<Option<Person>> {
    Ok(db::query_one(conn, "SELECT \"people\".* FROM \"people\" WHERE \"people\".\"ref\" = ?1 LIMIT 1", [ref_])?.map(|r| Person::from(&r)))
}

/// TelemetryIdentity: `origin_people` then `as_row.merge(origin_identity)`.
pub fn rows_with_identity(conn: &Connection, rows: &[Rec]) -> rusqlite::Result<Vec<Value>> {
    let mut refs: Vec<String> = Vec::new();
    for r in rows {
        if let Some(u) = r.s("user_ref")
            && !refs.iter().any(|x| x == u)
        {
            refs.push(u.to_string());
        }
    }
    let people = if refs.is_empty() { HashMap::new() } else { people_by_ref(conn, &refs)? };
    Ok(rows
        .iter()
        .map(|r| {
            let person = r.s("user_ref").and_then(|u| people.get(u));
            db::merge(
                execution_row(r),
                crate::obj! {
                    "user_ref" => js(r.s("user_ref")), "tenant" => js(r.s("app_tenant")),
                    "person" => person.map_or(Value::Null, |p| crate::obj! { "ref" => p.ref_.clone(), "name" => p.display_name() }),
                },
            )
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_with_counters(counters: Option<&str>) -> Rec {
        let conn = Connection::open_in_memory().unwrap();
        db::query(&conn, "SELECT 'request' AS kind, ?1 AS counters", [counters]).unwrap().remove(0)
    }

    // railwatch 1.2 leaves zero counters off the wire, so a request that ran
    // no queries arrives as {"view_renders":1}. The requests list, MCP's
    // search_requests and get_route, and the API all read this row.
    #[test]
    fn a_request_whose_counters_omit_queries_ran_none() {
        assert_eq!(execution_row(&request_with_counters(Some(r#"{"view_renders":1}"#)))["queries"], 0);
        assert_eq!(execution_row(&request_with_counters(Some("{}")))["queries"], 0);
        assert_eq!(execution_row(&request_with_counters(Some(r#"{"queries":3}"#)))["queries"], 3);
        assert_eq!(execution_row(&request_with_counters(None))["queries"], Value::Null);
    }
}
