//! Per-day tenant storage (docs/STORAGE.md).
//!
//! An environment's telemetry is a directory instead of one file:
//!
//! ```text
//! telemetry/<slug>/meta.sqlite3            long-lived rows (rollups, receipts, people, ...)
//! telemetry/<slug>/core/2026-10-14.sqlite3 executions, exceptions, ... of one UTC day
//! telemetry/<slug>/detail/2026-10-14.sqlite3 queries, cache events, views, logs, spans of that day
//! ```
//!
//! Every file is a complete gem-schema tenant file (same DDL, same
//! schema_migrations), so any one of them opens in Rails; each simply holds
//! only its share of the rows. Which table lives where is `Tier::of`.
//! Retention is deleting whole files (`retention`), never a DELETE.
//!
//! Ids: a table's id is unique across all of the environment's files, so a
//! merged export (`export`) keeps every id and every cross-file reference
//! (executions.profile_id). A new day file's `sqlite_sequence` is seeded
//! from the environment's high-water marks in meta (`ingest_hwm`), and every
//! committed batch advances them.

pub mod backup;
pub mod budget;
pub mod exemplars;
pub mod export;
pub mod layout;
pub mod reader;
pub mod retention;
pub mod s3;
pub mod store;
pub mod writer;

pub use layout::{Layout, Tier};
