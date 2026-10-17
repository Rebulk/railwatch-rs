//! Reading telemetry rows the way Active Record hands them to a controller,
//! and turning values into the JSON Rails renders for them.
//!
//! A `Rec` is one result row by column name. `attr` casts a column by its
//! schema type (bench/export/schema_export.json), which is what a model
//! attribute does: datetime -> Time (as_json: ISO 8601, milliseconds, "Z"),
//! boolean -> true/false, json -> the parsed document. Columns that are not
//! attributes (an aggregate in a `pluck`) are left as SQLite returned them,
//! which is also what Active Record does.

use std::sync::Arc;

use rusqlite::types::{Value as Sql, ValueRef};
use rusqlite::{Connection, Params};
use serde_json::{Map, Value};

use super::rb;
use crate::schema::{ColType, schema};

#[derive(Debug, Clone)]
pub struct Rec {
    cols: Arc<Vec<String>>,
    vals: Vec<Sql>,
}

impl Rec {
    pub fn raw(&self, name: &str) -> &Sql {
        static NULL: Sql = Sql::Null;
        self.cols.iter().position(|c| c == name).map_or(&NULL, |i| &self.vals[i])
    }
    pub fn at(&self, i: usize) -> &Sql {
        &self.vals[i]
    }
    pub fn i(&self, name: &str) -> Option<i64> {
        match self.raw(name) {
            Sql::Integer(i) => Some(*i),
            Sql::Real(f) => Some(*f as i64),
            Sql::Text(t) => t.parse().ok(),
            _ => None,
        }
    }
    pub fn f(&self, name: &str) -> Option<f64> {
        match self.raw(name) {
            Sql::Integer(i) => Some(*i as f64),
            Sql::Real(f) => Some(*f),
            _ => None,
        }
    }
    pub fn s(&self, name: &str) -> Option<&str> {
        match self.raw(name) {
            Sql::Text(t) => Some(t.as_str()),
            _ => None,
        }
    }
    pub fn b(&self, name: &str) -> Option<bool> {
        match self.raw(name) {
            Sql::Integer(i) => Some(*i != 0),
            Sql::Text(t) => Some(!matches!(t.as_str(), "f" | "0" | "false" | "FALSE" | "")),
            _ => None,
        }
    }
    /// A datetime column as epoch microseconds.
    pub fn t(&self, name: &str) -> Option<i64> {
        self.s(name).and_then(crate::time::parse_db_time)
    }
    pub fn blob(&self, name: &str) -> Option<&[u8]> {
        match self.raw(name) {
            Sql::Blob(b) => Some(b.as_slice()),
            Sql::Text(t) => Some(t.as_bytes()),
            _ => None,
        }
    }
    /// A json column parsed (Active Record's :json type); nil stays nil.
    pub fn json(&self, name: &str) -> Value {
        match self.s(name) {
            Some(t) => serde_json::from_str(t).unwrap_or(Value::Null),
            None => Value::Null,
        }
    }
    /// The column as the model attribute renders in JSON.
    pub fn attr(&self, table: &str, name: &str) -> Value {
        let ty = column_type(table, name);
        match ty {
            Some(ColType::Datetime) => jt(self.t(name)),
            Some(ColType::Boolean) => self.b(name).map_or(Value::Null, Value::Bool),
            Some(ColType::Json) => self.json(name),
            Some(ColType::Float) => self.f(name).map_or(Value::Null, jf),
            Some(ColType::Integer) => self.i(name).map_or(Value::Null, Value::from),
            _ => raw_json(self.raw(name)),
        }
    }
}

fn column_type(table: &str, name: &str) -> Option<ColType> {
    schema().tables.get(table)?.iter().find(|c| c.name == name).map(|c| c.ty)
}

/// A non-attribute value (an aggregate) as the sqlite3 gem returns it.
pub fn raw_json(v: &Sql) -> Value {
    match v {
        Sql::Null => Value::Null,
        Sql::Integer(i) => Value::from(*i),
        Sql::Real(f) => jf(*f),
        Sql::Text(t) => Value::String(t.clone()),
        Sql::Blob(b) => Value::String(String::from_utf8_lossy(b).into_owned()),
    }
}

/// A cached statement; over a per-day tenant (storage::reader) the
/// INDEXED BY / NOT INDEXED hints go to the reader's hinted views, as views
/// cannot take them.
pub fn prepare<'c>(conn: &'c Connection, sql: &str) -> rusqlite::Result<rusqlite::CachedStatement<'c>> {
    if crate::storage::reader::is_union(conn) { conn.prepare_cached(&crate::storage::reader::rewrite_index_hints(sql)) } else { conn.prepare_cached(sql) }
}

pub fn query<P: Params>(conn: &Connection, sql: &str, params: P) -> rusqlite::Result<Vec<Rec>> {
    let mut stmt = prepare(conn, sql)?;
    let cols: Arc<Vec<String>> = Arc::new(stmt.column_names().into_iter().map(str::to_string).collect());
    let n = cols.len();
    let mut rows = stmt.query(params)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let mut vals = Vec::with_capacity(n);
        for i in 0..n {
            vals.push(match row.get_ref(i)? {
                ValueRef::Null => Sql::Null,
                ValueRef::Integer(i) => Sql::Integer(i),
                ValueRef::Real(f) => Sql::Real(f),
                ValueRef::Text(t) => Sql::Text(String::from_utf8_lossy(t).into_owned()),
                ValueRef::Blob(b) => Sql::Blob(b.to_vec()),
            });
        }
        out.push(Rec { cols: cols.clone(), vals });
    }
    Ok(out)
}

pub fn query_one<P: Params>(conn: &Connection, sql: &str, params: P) -> rusqlite::Result<Option<Rec>> {
    Ok(query(conn, sql, params)?.into_iter().next())
}

pub fn count<P: Params>(conn: &Connection, sql: &str, params: P) -> rusqlite::Result<i64> {
    prepare(conn, sql)?.query_row(params, |r| r.get::<_, Option<i64>>(0)).map(|v| v.unwrap_or(0))
}

/// A bind for a time, as Active Record quotes it (quoted_date).
pub fn qt(micros: i64) -> String {
    crate::time::quoted_date_micros(micros)
}

// -- JSON values -----------------------------------------------------------

/// A Float as Ruby's json gem writes it (Grisu2 shortest, "1.0", "1.0e-07").
pub fn jf(f: f64) -> Value {
    if !f.is_finite() {
        return Value::Null;
    }
    let mut s = String::with_capacity(24);
    crate::rjson::fpconv::dtoa(f, &mut s);
    serde_json::from_str(&s).unwrap_or(Value::Null)
}

pub fn jfo(f: Option<f64>) -> Value {
    f.map_or(Value::Null, jf)
}

/// A Time as `as_json` renders it.
pub fn jt(micros: Option<i64>) -> Value {
    micros.map_or(Value::Null, |m| Value::String(rb::t_json(m)))
}

pub fn js(s: Option<&str>) -> Value {
    s.map_or(Value::Null, |s| Value::String(s.to_string()))
}

pub fn ji(i: Option<i64>) -> Value {
    i.map_or(Value::Null, Value::from)
}

/// Builds a JSON object in insertion order: obj![("a", v), ...].
#[macro_export]
macro_rules! obj {
    ($($k:expr => $v:expr),* $(,)?) => {{
        #[allow(unused_mut)]
        let mut m = serde_json::Map::new();
        $( m.insert(($k).to_string(), serde_json::Value::from($v)); )*
        serde_json::Value::Object(m)
    }};
}

pub fn merge(mut a: Value, b: Value) -> Value {
    if let (Value::Object(am), Value::Object(bm)) = (&mut a, b) {
        for (k, v) in bm {
            am.insert(k, v);
        }
    }
    a
}

pub fn object(pairs: Vec<(String, Value)>) -> Value {
    Value::Object(pairs.into_iter().collect::<Map<_, _>>())
}

/// `hash[key] = value` on a JSON object (appends a new key, keeps an
/// existing key's position, as a Ruby Hash does).
pub fn merge_into(v: &mut Value, key: &str, value: Value) {
    if let Value::Object(m) = v {
        m.insert(key.to_string(), value);
    }
}
