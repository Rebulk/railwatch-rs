//! railwatch-rs: a drop-in replacement for railwatch-cloud's ingest API.
//! Module names follow the Ruby reference (Ingest::Mapper, Ingest::Writer,
//! Ingest::Batch, Ingest::Payload, Ingest::ReceiptHeaders) so the two can be
//! read side by side.

/// The release this binary is (`RAILWATCH_VERSION` at build time, set by
/// bin/release from the tag), else the crate version.
pub const VERSION: &str = match option_env!("RAILWATCH_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// The GitHub repository releases are published to (install.sh, upgrade).
pub const REPO: &str = match option_env!("RAILWATCH_REPO") {
    Some(r) => r,
    None => "Rebulk/railwatch-rs",
};

/// Where this server's source is, for AGPL-3.0 section 13: every page links
/// to it (web::document) and every response names it (serve.rs's `Link`
/// header). A fork that changes the code should point this at its own source
/// (`RAILWATCH_SOURCE_URL` at build time).
pub const SOURCE_URL: &str = match option_env!("RAILWATCH_SOURCE_URL") {
    Some(u) => u,
    None => "https://github.com/Rebulk/railwatch-rs",
};

pub mod active_record_encryption;
pub mod alerts;
pub mod config;
pub mod control;
pub mod control_schema;
pub mod dashboard;
pub mod detection;
pub mod followups;
pub mod grouping;
pub mod http;
pub mod import_rails;
pub mod ingest;
pub mod jobs;
pub mod rate_limit;
pub mod release_signature;
pub mod rjson;
pub mod rollup;
pub mod schema;
pub mod serve;
pub mod storage;
pub mod tenants;
pub mod time;
pub mod tls;
pub mod web;
