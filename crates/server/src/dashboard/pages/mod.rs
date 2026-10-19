//! One function per railwatch-cloud controller action, returning that
//! action's Inertia props (without the shared props). Each is named
//! `<controller>_<action>` and takes the tenant's read connection, the
//! PageContext and the control rows.
//!
//! SQL is written in the shape Active Record generates for the Ruby scope
//! (same predicates, order and LIMIT), so SQLite picks the same plan and
//! equal sort keys come back in the same order.

mod executions;
mod misc;
mod telemetry;

pub use executions::*;
pub use misc::*;
pub use telemetry::*;

use rusqlite::Connection;
use serde_json::Value;

use super::aggregations;
use super::control::{self, ControlData};
use super::db::{self, Rec, qt};
use super::filter_query::Cond;
use super::{PageContext, PageError};

/// `<Model>.where(<cond>).<order>.limit(n)` -> rows.
pub(crate) fn select(conn: &Connection, table: &str, select: &str, cond: &Cond, order: &str, limit: Option<usize>) -> rusqlite::Result<Vec<Rec>> {
    let from = cond.from.clone().unwrap_or_else(|| format!("\"{table}\""));
    let mut sql = format!("SELECT {select} FROM {from} WHERE {}", cond.sql());
    if !order.is_empty() {
        sql.push_str(" ORDER BY ");
        sql.push_str(order);
    }
    if let Some(n) = limit {
        sql.push_str(&format!(" LIMIT {n}"));
    }
    db::query(conn, &sql, rusqlite::params_from_iter(cond.binds.iter()))
}

/// `where(occurred_at: from..to)` on `table`.
pub(crate) fn between(table: &str, from: i64, to: i64) -> Cond {
    let mut c = Cond::new();
    c.and(format!("\"{table}\".\"occurred_at\" BETWEEN ? AND ?"), vec![super::filter_query::s(&qt(from)), super::filter_query::s(&qt(to))]);
    c
}

/// `scope.where(kind: kind).between(from, to)` for executions (kind first, as
/// the named scope chains it).
pub(crate) fn executions_between(kind: Option<&str>, from: i64, to: i64) -> Cond {
    let mut c = Cond::new();
    if let Some(k) = kind {
        c.eq("executions", "kind", super::filter_query::s(k));
    }
    c.and("\"executions\".\"occurred_at\" BETWEEN ? AND ?", vec![super::filter_query::s(&qt(from)), super::filter_query::s(&qt(to))]);
    c
}

pub(crate) const RECENT: &str = "\"{t}\".\"occurred_at\" DESC";

pub(crate) fn recent(table: &str) -> String {
    RECENT.replace("{t}", table)
}

pub(crate) fn series(conn: &Connection, ctx: &PageContext, record_type: &str, group_hash: Option<&str>) -> rusqlite::Result<Value> {
    let (from, to) = ctx.range();
    Ok(Value::Array(aggregations::points(conn, record_type, from, to, group_hash, ctx.step, None)?))
}

pub(crate) fn grouped(conn: &Connection, ctx: &PageContext, record_type: &str, limit: usize, sort: bool) -> rusqlite::Result<Vec<Value>> {
    let (from, to) = ctx.range();
    let (order, dir) = if sort { (ctx.param("sort"), ctx.param("dir")) } else { (None, None) };
    aggregations::grouped(conn, record_type, from, to, limit, order, dir)
}

pub(crate) fn summary_with_delta(conn: &Connection, ctx: &PageContext, record_type: &str, group_hash: Option<&str>) -> rusqlite::Result<Value> {
    aggregations::summary_with_delta(conn, record_type, ctx.range(), ctx.previous_range(), group_hash)
}

/// `grouped` and the all-groups `summary_with_delta` of one record type
/// from one read of the window's rollups (both read the same rows).
pub(crate) fn grouped_and_summary(conn: &Connection, ctx: &PageContext, record_type: &str, limit: usize, sort: bool) -> rusqlite::Result<(Vec<Value>, Value)> {
    let (from, to) = ctx.range();
    let (order, dir) = if sort { (ctx.param("sort"), ctx.param("dir")) } else { (None, None) };
    let rows = super::rollup::between(conn, record_type, from, to)?;
    let grouped = aggregations::grouped_rows(&rows, from, to, limit, order, dir);
    Ok((grouped, aggregations::summary_with_delta_of(conn, record_type, &rows, ctx.previous_range())?))
}

pub(crate) fn deploys_in_window(control: &dyn ControlData, ctx: &PageContext) -> Value {
    let (from, to) = ctx.range();
    control::deploys_in_window(control, from, to)
}

/// `params[:sort] || default`
pub(crate) fn sort_param(ctx: &PageContext, default: &str) -> String {
    ctx.param("sort").unwrap_or(default).to_string()
}

pub(crate) fn dir_param(ctx: &PageContext) -> String {
    ctx.param("dir").unwrap_or("desc").to_string()
}

pub(crate) type R = Result<Value, PageError>;
