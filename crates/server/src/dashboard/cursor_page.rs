//! Port of Railwatch::Telemetry::CursorPage: keyset pagination over
//! (occurred_at|duration, id) descending, with signed cursors.
//!
//! Cursor format (Rust-only; cursors only round-trip within this server, a
//! Rails cursor is rejected and vice versa): base64url(JSON payload) + "--" +
//! hex(HMAC-SHA256(key, base64 part)). The payload is the same shape Rails
//! signs with its MessageVerifier: {v:1, model, order, context, value, id}.
//! The key comes from `CursorKey` (p3-control holds the secret).

use rusqlite::Connection;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::db::{self, Rec};
use super::filter_query::{Cond, s};

pub const DEFAULT_LIMIT: i64 = 50;
pub const MAX_LIMIT: i64 = 200;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Order {
    Recent,
    Slowest,
}

impl Order {
    fn name(self) -> &'static str {
        match self {
            Order::Recent => "recent",
            Order::Slowest => "slowest",
        }
    }
    fn column(self) -> &'static str {
        match self {
            Order::Recent => "occurred_at",
            Order::Slowest => "duration",
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct InvalidCursor;

impl std::fmt::Display for InvalidCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid telemetry cursor")
    }
}

/// The signing key for cursors.
#[derive(Clone)]
pub struct CursorKey(pub Vec<u8>);

fn hmac(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = Sha256::new();
    ipad.update(k.map(|b| b ^ 0x36));
    ipad.update(msg);
    let inner = ipad.finalize();
    let mut opad = Sha256::new();
    opad.update(k.map(|b| b ^ 0x5c));
    opad.update(inner);
    opad.finalize().into()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl CursorKey {
    pub fn generate(&self, payload: &Value) -> String {
        use base64::Engine;
        let data = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).unwrap());
        let sig = hex(&hmac(&self.0, data.as_bytes()));
        format!("{data}--{sig}")
    }

    pub fn verify(&self, cursor: &str) -> Option<Value> {
        use base64::Engine;
        let (data, sig) = cursor.rsplit_once("--")?;
        let expected = hex(&hmac(&self.0, data.as_bytes()));
        // constant-time compare
        if expected.len() != sig.len() || expected.bytes().zip(sig.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) != 0 {
            return None;
        }
        serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(data).ok()?).ok()
    }
}

pub struct Page {
    pub rows: Vec<Rec>,
    pub meta: Value,
}

/// `CursorPage.call(scope, cursor:, limit:, order:, context:)` for `table`
/// (model `model`, e.g. "Railwatch::Telemetry::Log") with `select` columns
/// and base conditions `cond`.
#[allow(clippy::too_many_arguments)]
pub fn call(
    conn: &Connection,
    key: &CursorKey,
    table: &str,
    model: &str,
    select: &str,
    mut cond: Cond,
    cursor: Option<&str>,
    limit: Option<&str>,
    order: Order,
    context: &str,
) -> Result<rusqlite::Result<Page>, InvalidCursor> {
    let limit = match limit.and_then(ruby_to_i) {
        Some(n) if n > 0 => n.min(MAX_LIMIT),
        _ => DEFAULT_LIMIT,
    };
    let col = order.column();
    if let Some(c) = cursor.filter(|c| !c.trim().is_empty()) {
        let p = key.verify(c).ok_or(InvalidCursor)?;
        let ok = p["v"] == 1 && p["model"] == model && p["order"] == order.name() && p["context"] == context;
        if !ok {
            return Err(InvalidCursor);
        }
        let id = value_int(&p["id"]).ok_or(InvalidCursor)?;
        let v: rusqlite::types::Value = match order {
            Order::Recent => {
                let t = p["value"].as_str().and_then(super::window::parse_time).ok_or(InvalidCursor)?;
                s(&db::qt(t))
            }
            Order::Slowest => rusqlite::types::Value::Integer(value_int(&p["value"]).ok_or(InvalidCursor)?),
        };
        cond.and(
            format!("(\"{table}\".\"{col}\" < ? OR (\"{table}\".\"{col}\" = ? AND \"{table}\".\"id\" < ?))"),
            vec![v.clone(), v, rusqlite::types::Value::Integer(id)],
        );
    }
    let from = if order == Order::Slowest && table == "queries" {
        format!("\"{table}\" INDEXED BY idx_queries_slowest")
    } else {
        cond.from.clone().unwrap_or_else(|| format!("\"{table}\""))
    };
    let sql = format!("SELECT {select} FROM {from} WHERE {} ORDER BY \"{table}\".\"{col}\" DESC, \"{table}\".\"id\" DESC LIMIT {}", cond.sql(), limit + 1);
    let per_file = match per_file(conn, table, select, &cond, order, limit + 1) {
        Ok(p) => p,
        Err(e) => return Ok(Err(e)),
    };
    let rows = match per_file.map_or_else(
        || db::query(conn, &sql, rusqlite::params_from_iter(cond.binds.iter())),
        |(sql, binds)| db::query_as_is(conn, &sql, rusqlite::params_from_iter(binds.iter())),
    ) {
        Ok(r) => r,
        Err(e) => return Ok(Err(e)),
    };
    let has_more = rows.len() as i64 > limit;
    let rows: Vec<Rec> = rows.into_iter().take(limit as usize).collect();
    let next = if has_more {
        let last = rows.last().unwrap();
        let value = match order {
            Order::Recent => Value::String(crate::time::iso8601_6(last.t("occurred_at").unwrap_or(0))),
            Order::Slowest => Value::from(last.i("duration").unwrap_or(0)),
        };
        Value::String(key.generate(&crate::obj! {
            "v" => 1, "model" => model, "order" => order.name(), "context" => context, "value" => value, "id" => last.i("id").unwrap_or(0),
        }))
    } else {
        Value::Null
    };
    Ok(Ok(Page { rows, meta: crate::obj! { "limit" => limit, "next_cursor" => next, "has_more" => has_more } }))
}

/// The page's statement over a per-day tenant as each day file's own top
/// `n` (`ORDER BY col DESC, id DESC LIMIT n` inside the file, on its
/// indexes), merged and cut to `n`. (col, id) is a total order and ids are
/// environment-wide, so these are the rows the view returns, in its order.
/// Through the view, SQLite sorted every row in range of every file first:
/// queries@7d 31 -> 5 ms, logs.fts@7d (FTS paired per file) 150 -> 50 ms.
/// `idx_queries_slowest` (the single file's hint) is used only in files
/// whose whole day is in the window: at a window's edge it walks the file's
/// slowest rows outside the window first (docs/PERF.md #5). As in the single
/// file it is slow when a filter matches almost nothing (queries.q). None
/// (use the view) on a single file, or for conditions with their own hints.
fn per_file(
    conn: &Connection,
    table: &str,
    select: &str,
    cond: &Cond,
    order: Order,
    n: i64,
) -> rusqlite::Result<Option<(String, Vec<rusqlite::types::Value>)>> {
    let Some(range) = cond.range else { return Ok(None) };
    let cond_sql = cond.sql();
    if !crate::storage::reader::is_union(conn) || cond.from.is_some() || cond_sql.contains("INDEXED") {
        return Ok(None);
    }
    let col = order.column();
    let Some((union, binds)) = crate::storage::reader::per_file(conn, table, range, &cond_sql, &cond.binds, |f, covered, cond_sql| {
        let hint = if covered && order == Order::Slowest && table == "queries" { " INDEXED BY idx_queries_slowest" } else { "" };
        format!(
            "SELECT {select} FROM {}.\"{table}\" AS \"{table}\"{hint} WHERE {cond_sql} ORDER BY \"{table}\".\"{col}\" DESC, \"{table}\".\"id\" DESC LIMIT {n}",
            f.alias
        )
    })?
    else {
        return Ok(None);
    };
    Ok(Some((format!("{union} ORDER BY \"{col}\" DESC, \"id\" DESC LIMIT {n}"), binds)))
}

fn value_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// `String#to_i`: leading integer, else 0.
pub fn ruby_to_i(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let (neg, rest) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '_').filter(|c| *c != '_').collect();
    let n: i64 = digits.parse().unwrap_or(0);
    Some(if neg { -n } else { n })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_rfc4231_case2() {
        assert_eq!(hex(&hmac(b"Jefe", b"what do ya want for nothing?")), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
    }

    #[test]
    fn round_trip_and_tamper() {
        let k = CursorKey(b"secret".to_vec());
        let c = k.generate(&serde_json::json!({"v": 1, "id": 7}));
        assert_eq!(k.verify(&c).unwrap()["id"], 7);
        let tampered = c.replacen('e', "f", 1);
        assert!(tampered == c || k.verify(&tampered).is_none());
        assert!(CursorKey(b"other".to_vec()).verify(&c).is_none());
    }
}
