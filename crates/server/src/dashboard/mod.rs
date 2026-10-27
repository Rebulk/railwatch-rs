//! The dashboard's page props, computed the way railwatch-cloud @ 464f0fd
//! (gem 0.9.0) computes them, as plain functions over a read connection to a
//! tenant file:
//!
//! ```ignore
//! let props = dashboard::pages::requests_index(&conn, &ctx, &control)?;
//! ```
//!
//! No HTTP, auth or Inertia here: p3-control wires these into routes and
//! adds the shared props (`environment`, `saved_views`, auth...). The
//! window-derived shared props are `shared_props(ctx)`.
//!
//! Module names follow the Ruby: `window` (Railwatch::Window),
//! `filter_query` (FilterQuery), `cursor_page` (Telemetry::CursorPage),
//! `aggregations` (Telemetry::Aggregations), `rollup` (Telemetry::Rollup),
//! `tdigest_decode` (Rollup.centroids), `execution_presenter`
//! (ExecutionPresenter), `tenant` (Telemetry::Tenant), `pages` (one function
//! per controller action). `rb` holds the Ruby semantics they depend on.

pub mod aggregations;
pub mod control;
pub mod cursor_page;
pub mod db;
pub mod execution_presenter;
pub mod filter_query;
pub mod models;
pub mod pages;
pub mod pool;
pub mod rb;
pub mod request_scan;
pub mod rollup;
pub mod tdigest_decode;
pub mod tenant;
pub mod window;

use std::collections::HashMap;

use serde_json::Value;

pub use control::{ControlData, Deploy, Issue};
pub use cursor_page::{CursorKey, InvalidCursor};
use window::Window;

/// Query string and path parameters, by name (`params[:x]`).
pub type Params = HashMap<String, String>;

/// Everything a page needs besides the tenant file and control rows.
#[derive(Clone)]
pub struct PageContext {
    pub environment_id: i64,
    /// Epoch microseconds: `Time.current` for this request.
    pub now: i64,
    pub window: Window,
    /// The chart step key (EnvironmentScoped#step_key).
    pub step: &'static str,
    pub params: Params,
    pub cursor_key: CursorKey,
}

impl PageContext {
    /// Builds the context from the request's params, at `now`.
    pub fn new(environment_id: i64, now: i64, params: Params, cursor_key: CursorKey) -> PageContext {
        let window = Window::parse(params.get("window").map(String::as_str), params.get("from").map(String::as_str), params.get("to").map(String::as_str), now);
        let step = window::step_key(window.from, window.to, params.get("step").map(String::as_str));
        PageContext { environment_id, now, window, step, params, cursor_key }
    }

    pub fn param(&self, k: &str) -> Option<&str> {
        self.params.get(k).map(String::as_str)
    }

    /// `params[:x].to_s`
    pub fn param_s(&self, k: &str) -> String {
        self.param(k).unwrap_or("").to_string()
    }

    pub fn range(&self) -> (i64, i64) {
        self.window.range()
    }

    pub fn previous_range(&self) -> (i64, i64) {
        self.window.previous().range()
    }

    /// EnvironmentScoped#telemetry_cursor_context.
    pub fn cursor_context(&self, resource: &str) -> String {
        filter_query::cursor_context(self.environment_id, resource, self.param("q"), self.window.from, self.window.to)
    }
}

/// The shared props EnvironmentScoped derives from the window.
pub fn shared_props(ctx: &PageContext) -> Value {
    crate::obj! {
        "window" => ctx.window.key.clone(),
        "range" => ctx.window.to_h(),
        "step" => ctx.step,
        "steps" => window::steps_for(ctx.window.from, ctx.window.to),
    }
}

/// What a page can fail with.
#[derive(Debug)]
pub enum PageError {
    /// 404 (ActiveRecord::RecordNotFound).
    NotFound,
    /// 422 with this message (CursorPage::InvalidCursor, BoundedGzip errors).
    Unprocessable(String),
    Sql(rusqlite::Error),
}

impl From<rusqlite::Error> for PageError {
    fn from(e: rusqlite::Error) -> PageError {
        PageError::Sql(e)
    }
}

impl From<InvalidCursor> for PageError {
    fn from(e: InvalidCursor) -> PageError {
        PageError::Unprocessable(e.to_string())
    }
}

impl std::fmt::Display for PageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PageError::NotFound => f.write_str("not found"),
            PageError::Unprocessable(m) => f.write_str(m),
            PageError::Sql(e) => write!(f, "{e}"),
        }
    }
}

pub type PageResult = Result<Value, PageError>;
