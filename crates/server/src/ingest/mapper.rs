//! Port of Railwatch::Ingest::Mapper (gem 0.9.0/0.9.1, app/models/railwatch/
//! ingest/mapper.rb). Maps one wire record to `(table, row)` where the row is
//! an ordered list of (column, bind value) already cast the way the Ruby
//! writer binds it: JSON columns as JSON text, times as UTC
//! "YYYY-MM-DD HH:MM:SS.ffffff", booleans 0/1, strings scrubbed of control
//! characters and cut to the column's limit.
//!
//! Errors are what Ruby raises; Ingest::Batch turns them into rejection
//! reasons ("TypeError: t must be a string"), so their class and message are
//! part of the contract.

use crate::rjson::{self, Value, ruby};
use crate::schema::{ColType, Column, DefaultValue, schema};

pub const PARENTS: &[&str] = &["request", "job_attempt", "scheduled_task", "command", "channel_action"];

/// Railwatch::Record::VERSIONS: every known type is at v1.
pub const TYPES: &[&str] = &[
    "request",
    "job_attempt",
    "scheduled_task",
    "command",
    "channel_action",
    "query",
    "n_plus_one",
    "transaction",
    "exception",
    "cache_event",
    "mail",
    "broadcast",
    "notification",
    "outgoing_request",
    "storage_op",
    "view_render",
    "log",
    "enqueued_job",
    "user",
    "deprecation",
    "visit",
    "process",
    "span",
    "health",
    "profile",
    "attachment",
    "session",
    "llm_call",
];

const PROFILE_MAX_INGEST_BYTES: i64 = 4 * 1024 * 1024;
const ATTACHMENT_MAX_BODY_BYTES: i64 = 32 * 1024 * 1024;

/// A Ruby exception raised while mapping: (class, message).
#[derive(Debug, Clone, PartialEq)]
pub struct MapError {
    pub class: &'static str,
    pub message: String,
}

impl MapError {
    pub fn new(class: &'static str, message: impl Into<String>) -> Self {
        MapError { class, message: message.into() }
    }
    /// The rejection reason Ingest::Batch#map_all records: the classes it
    /// rescues by name report "Class: message", anything else "invalid record".
    pub fn rejection_reason(&self) -> String {
        const NAMED: &[&str] = &[
            "ArgumentError",
            "TypeError",
            "KeyError",
            "NoMethodError",
            "RangeError",
            "FloatDomainError",
            "JSON::GeneratorError",
            "Railwatch::Telemetry::BoundedGzip::Malformed",
            "Railwatch::Telemetry::BoundedGzip::TooLarge",
            "Railwatch::Telemetry::BoundedGzip::SizeMismatch",
        ];
        if NAMED.contains(&self.class) { format!("{}: {}", self.class, self.message) } else { "invalid record".into() }
    }

    fn type_error(message: impl Into<String>) -> Self {
        Self::new("TypeError", message)
    }
    fn argument(message: impl Into<String>) -> Self {
        Self::new("ArgumentError", message)
    }
}

type R<T> = Result<T, MapError>;

/// A value bound into an INSERT.
#[derive(Debug, Clone, PartialEq)]
pub enum Bind {
    Null,
    Int(i64),
    Float(f64),
    Text(String),
    Blob(Vec<u8>),
    /// A value the sqlite3 gem cannot bind (true/false into a non-boolean
    /// column). The mapper lets it through, as Ruby's does, and the writer
    /// fails the whole batch with "can't prepare TrueClass" -- a 500, which is
    /// what railwatch-cloud answers for such a record.
    Unbindable(&'static str),
}

/// One mapped row: (column, value) in the order the Ruby mapper's attrs hash
/// has them after `serialize` (which walks the class's columns_hash, i.e.
/// the table's column order, keeping only the attrs present).
pub type Row = Vec<(&'static str, Bind)>;

pub struct Mapped {
    pub table: &'static str,
    pub row: Row,
}

pub struct PersonRecord {
    pub id: Option<String>,
    pub name: Option<String>,
    pub email: Option<String>,
    pub tenant: Option<String>,
    pub timestamp: Value,
}

// ---------------------------------------------------------------------------
// validate_record!

pub fn validate_record(rec: &Value) -> R<()> {
    let Value::Hash(_) = rec else { return Err(MapError::type_error("record must be an object")) };
    let Value::Str(t) = rec.get("t") else { return Err(MapError::type_error("t must be a string")) };
    validate_version(rec, t)?;
    validate_identifier(rec, "_group", is_group_hash, "32 hexadecimal characters")?;
    validate_identifier(rec, "execution_id", is_identifier, "a printable token of at most 36 characters")?;
    validate_identifier(rec, "trace_id", is_identifier, "a printable token of at most 36 characters")?;
    for (field, expected) in [
        ("stages", "hash"),
        ("counters", "hash"),
        ("inertia", "hash"),
        ("params", "hash"),
        ("frames", "array"),
        ("fingerprint", "array"),
        ("tags", "array"),
        ("only", "array"),
        ("attributes", "hash"),
        ("locals", "hash"),
    ] {
        let v = rec.get(field);
        let ok = match v {
            Value::Nil => true,
            Value::Hash(_) => expected == "hash",
            Value::Array(_) => expected == "array",
            _ => false,
        };
        if !ok {
            return Err(MapError::type_error(format!("{field} must be a {expected}")));
        }
    }
    validate_numeric_hash(rec.get("stages"), "stages")?;
    validate_numeric_hash(rec.get("counters"), "counters")?;
    validate_frames(rec.get("frames"))?;
    validate_locals(rec.get("locals"))?;
    Ok(())
}

fn validate_version(rec: &Value, t: &[u8]) -> R<()> {
    let Ok(t) = std::str::from_utf8(t) else {
        // `rec["t"].to_sym` on an invalid string raises EncodingError, which
        // the batch reports as "invalid record".
        return Err(MapError::new("EncodingError", "invalid record"));
    };
    if !TYPES.contains(&t) {
        return Ok(());
    }
    match rec.get("v") {
        Value::Int(1) => Ok(()),
        v => Err(MapError::type_error(format!("{t} v{} is not v1", String::from_utf8_lossy(&ruby::inspect(v))))),
    }
}

fn validate_identifier(rec: &Value, field: &str, ok: fn(&[u8]) -> bool, description: &str) -> R<()> {
    match rec.get(field) {
        Value::Nil => Ok(()),
        Value::Str(s) => {
            // String#match? raises on an invalid string.
            if !ruby::valid_utf8(s) {
                return Err(MapError::argument("invalid byte sequence in UTF-8"));
            }
            if ok(s) { Ok(()) } else { Err(MapError::type_error(format!("{field} must be {description}"))) }
        }
        _ => Err(MapError::type_error(format!("{field} must be {description}"))),
    }
}

fn is_group_hash(s: &[u8]) -> bool {
    // /\A[0-9a-f]{32}\z/i -- \z, but note Ruby's \A..\z with no newline games.
    s.len() == 32 && s.iter().all(u8::is_ascii_hexdigit)
}

fn is_identifier(s: &[u8]) -> bool {
    (1..=36).contains(&s.len()) && s.iter().all(|b| (0x21..=0x7e).contains(b))
}

fn validate_numeric_hash(v: &Value, field: &str) -> R<()> {
    let Value::Hash(h) = v else { return Ok(()) };
    let ok = h.values().all(|n| match n {
        Value::Int(i) => *i >= 0,
        Value::BigInt(s) => !s.starts_with('-') && false_if_infinite(s),
        Value::Float(f) => f.is_finite() && *f >= 0.0,
        _ => false,
    });
    if ok { Ok(()) } else { Err(MapError::type_error(format!("{field} must contain non-negative numbers"))) }
}

/// `number.to_f.finite?` for a bignum: false only past f64's range.
fn false_if_infinite(s: &str) -> bool {
    s.parse::<f64>().map(f64::is_finite).unwrap_or(false)
}

fn validate_frames(v: &Value) -> R<()> {
    let Value::Array(frames) = v else { return Ok(()) };
    let valid = frames.iter().all(|frame| {
        let Value::Hash(_) = frame else { return false };
        ["file", "function"].iter().all(|f| matches!(frame.get(f), Value::Nil | Value::Str(_)))
            && match frame.get("line") {
                Value::Nil => true,
                Value::Int(i) => *i >= 0,
                Value::BigInt(s) => !s.starts_with('-'),
                _ => false,
            }
            && matches!(frame.get("in_app"), Value::Nil | Value::Bool(_))
            && match frame.get("code") {
                Value::Nil => true,
                Value::Hash(code) => code.values().all(|s| matches!(s, Value::Str(_))),
                _ => false,
            }
    });
    if valid { Ok(()) } else { Err(MapError::type_error("frames must contain frame objects")) }
}

fn validate_locals(v: &Value) -> R<()> {
    let Value::Hash(h) = v else { return Ok(()) };
    if h.values().all(|v| !matches!(v, Value::Hash(_) | Value::Array(_))) { Ok(()) } else { Err(MapError::type_error("locals must contain scalar values")) }
}

// ---------------------------------------------------------------------------
// row_for / mapped

/// Attributes before serialization: (attr name, raw value) in the order the
/// Ruby mapper builds its attrs hash.
type Attrs = Vec<(&'static str, Value)>;

/// Mapper.row_for. `Ok(None)` is an unknown type.
pub fn row_for(rec: &Value, truncations: &mut Vec<String>) -> R<Option<Mapped>> {
    validate_record(rec)?;
    let t = ruby::to_s(rec.get("t"));
    let t = String::from_utf8_lossy(&t);
    let pair = if PARENTS.contains(&t.as_ref()) { Some(execution(rec)?) } else { mapped(&t, rec)? };
    let Some((table, attrs)) = pair else { return Ok(None) };
    let row = serialize(table, attrs, truncations)?;
    Ok(Some(Mapped { table, row }))
}

pub fn person_record(rec: &Value) -> R<PersonRecord> {
    validate_record(rec)?;
    let cast = |key: &str, col: &str| -> R<Option<String>> {
        let column = schema().column("people", col);
        match cast_value(rec.get(key).clone(), column, "people", &mut Vec::new())? {
            Bind::Null => Ok(None),
            Bind::Text(s) => Ok(Some(s)),
            _ => unreachable!("people string columns"),
        }
    };
    Ok(PersonRecord {
        id: cast("id", "ref")?,
        name: cast("name", "name")?,
        email: cast("email", "email")?,
        tenant: cast("tenant", "app_tenant")?,
        timestamp: rec.get("timestamp").clone(),
    })
}

fn take(rec: &Value, key: &str) -> Value {
    rec.get(key).clone()
}

fn envelope(rec: &Value) -> R<Attrs> {
    Ok(vec![
        ("occurred_at", Value::Str(timestamp_string(rec.get("timestamp"))?.into_bytes())),
        ("deploy", take(rec, "deploy")),
        ("server", take(rec, "server")),
        ("group_hash", take(rec, "_group")),
        ("trace_id", take(rec, "trace_id")),
        ("execution_source", take(rec, "execution_source")),
        ("execution_id", take(rec, "execution_id")),
        ("execution_preview", take(rec, "execution_preview")),
        ("execution_stage", take(rec, "execution_stage")),
        ("user_ref", take(rec, "user")),
        ("app_tenant", take(rec, "tenant")),
    ])
}

fn child(rec: &Value, table: &'static str, keys: &[&'static str]) -> R<(&'static str, Attrs)> {
    let mut attrs = envelope(rec)?;
    for k in keys {
        set(&mut attrs, k, take(rec, k));
    }
    Ok((table, attrs))
}

/// `attrs[key] = value`: replace in place if present, else append.
fn set(attrs: &mut Attrs, key: &'static str, value: Value) {
    if let Some(slot) = attrs.iter_mut().find(|(k, _)| *k == key) {
        slot.1 = value;
    } else {
        attrs.push((key, value));
    }
}

fn mapped(t: &str, rec: &Value) -> R<Option<(&'static str, Attrs)>> {
    let pair = match t {
        "query" => child(
            rec,
            "queries",
            &["sql", "name", "duration", "connection", "adapter", "role", "async", "in_transaction", "row_count", "source", "allocations", "explain"],
        )?,
        "span" => {
            let mut attrs = envelope(rec)?;
            set(&mut attrs, "name", take(rec, "name"));
            set(&mut attrs, "duration", take(rec, "duration"));
            set(&mut attrs, "payload", take(rec, "attributes"));
            set(&mut attrs, "status", take(rec, "status"));
            ("spans", attrs)
        }
        "health" => health(rec)?,
        "exception" => exception(rec)?,
        "cache_event" => child(rec, "cache_events", &["store", "key", "type", "duration", "ttl", "hits"])?,
        "mail" => child(
            rec,
            "mails",
            &["mailer", "subject", "to", "cc", "bcc", "attachments", "delivery_method", "perform_deliveries", "duration", "failed", "message_id"],
        )?,
        "broadcast" => child(rec, "broadcasts", &["kind", "stream", "channel", "action", "bytes", "duration", "failed"])?,
        "notification" => child(rec, "notifications", &["notifier", "delivery_method", "channel", "duration", "failed"])?,
        "outgoing_request" => child(
            rec,
            "outgoing_requests",
            &["host", "method", "url", "duration", "status_code", "request_size", "response_size", "error", "source", "response_body", "in_transaction"],
        )?,
        "llm_call" => child(
            rec,
            "llm_calls",
            &[
                "operation",
                "provider",
                "model",
                "response_model",
                "tool_name",
                "duration",
                "status",
                "error",
                "streaming",
                "message_count",
                "tool_count",
                "input_tokens",
                "output_tokens",
                "cache_read_tokens",
                "cache_write_tokens",
                "thinking_tokens",
                "cost_nanos",
                "workflow_id",
                "workflow_name",
                "workflow_step_id",
                "workflow_step_name",
                "workflow_step_parent_id",
                "prompt",
                "completion",
                "finish_reason",
                "provider_request_id",
                "tools",
                "cost_reported",
                "attachments",
                "attachment_types",
                "attachment_names",
                "tool_call_id",
                "params",
            ],
        )?,
        "profile" => profile(rec)?,
        "attachment" => attachment(rec)?,
        "storage_op" => child(rec, "storage_ops", &["service", "op", "key", "duration"])?,
        "view_render" => child(rec, "view_renders", &["identifier", "kind", "layout", "count", "cache_hits", "duration"])?,
        "log" => child(rec, "logs", &["level", "message", "tags", "context", "source"])?,
        "enqueued_job" => {
            let mut attrs = envelope(rec)?;
            set(&mut attrs, "job_id", take(rec, "job_id"));
            set(&mut attrs, "name", Value::Str(ruby::to_s(rec.get("name"))));
            set(&mut attrs, "queue", take(rec, "queue"));
            set(&mut attrs, "adapter", take(rec, "adapter"));
            set(&mut attrs, "priority", take(rec, "priority"));
            let scheduled = match rec.get("scheduled_at") {
                v if v.truthy() => Value::Str(timestamp_string(v)?.into_bytes()),
                v => v.clone(),
            };
            set(&mut attrs, "scheduled_at", scheduled);
            set(&mut attrs, "duration", take(rec, "duration"));
            set(&mut attrs, "failed", Value::Bool(rec.get("failed").truthy()));
            ("enqueued_jobs", attrs)
        }
        "transaction" => child(rec, "transactions", &["duration", "outcome", "connection", "statement_count"])?,
        "n_plus_one" => child(rec, "n_plus_ones", &["sql", "count", "source"])?,
        "deprecation" => child(rec, "deprecations", &["message", "gem_name", "horizon", "source"])?,
        "visit" => child(
            rec,
            "visits",
            &["component", "url", "method", "duration", "status", "partial", "only", "props_bytes", "user_agent", "lcp", "cls", "inp", "ttfb"],
        )?,
        "session" => {
            let mut attrs = envelope(rec)?;
            set(&mut attrs, "session_id", Value::Str(ruby::to_s(rec.get("id"))));
            set(&mut attrs, "source", take(rec, "source"));
            set(&mut attrs, "status", take(rec, "status"));
            let started = match rec.get("started_at") {
                v if v.truthy() => Value::Str(timestamp_string(v)?.into_bytes()),
                v => v.clone(),
            };
            set(&mut attrs, "started_at", started);
            set(&mut attrs, "duration", take(rec, "duration"));
            set(&mut attrs, "requests", take(rec, "requests"));
            set(&mut attrs, "visits", take(rec, "visits"));
            set(&mut attrs, "error_count", take(rec, "errors"));
            set(&mut attrs, "ended", Value::Bool(rec.get("ended").truthy()));
            ("sessions", attrs)
        }
        // Batch handles users before row_for; row_for itself maps one to a
        // bare person (never written).
        "user" => ("people", vec![("ref", Value::Str(ruby::to_s(rec.get("id"))))]),
        "process" => {
            let detail = slice(rec, &["app", "environment", "database_adapter", "queue_adapter", "cache_store"]);
            let attrs = vec![
                ("booted_at", Value::Str(timestamp_string(rec.get("timestamp"))?.into_bytes())),
                ("pid", take(rec, "pid")),
                ("role", take(rec, "role")),
                ("server", take(rec, "server")),
                ("deploy", take(rec, "deploy")),
                ("ruby_version", take(rec, "ruby_version")),
                ("rails_version", take(rec, "rails_version")),
                ("railwatch_version", take(rec, "railwatch_version")),
                ("boot_seconds", take(rec, "boot_seconds")),
                ("detail", detail),
            ];
            ("processes", attrs)
        }
        _ => return Ok(None),
    };
    Ok(Some(pair))
}

/// `rec.slice(*keys)`: present keys, in the order given.
fn slice(rec: &Value, keys: &[&str]) -> Value {
    let mut h = rjson::Hash::new();
    if let Value::Hash(src) = rec {
        for k in keys {
            if let Some(v) = src.get(k.as_bytes()) {
                h.insert(k.as_bytes().to_vec(), v.clone());
            }
        }
    }
    Value::Hash(h)
}

fn execution(rec: &Value) -> R<(&'static str, Attrs)> {
    let kind = ruby::to_s(rec.get("t"));
    let kind_s = String::from_utf8_lossy(&kind).into_owned();
    let interp = |a: &str, sep: &str, b: &str| -> Vec<u8> {
        let mut s = ruby::to_s(rec.get(a));
        s.extend_from_slice(sep.as_bytes());
        s.extend_from_slice(&ruby::to_s(rec.get(b)));
        s
    };
    let name: Vec<u8> = match kind_s.as_str() {
        "request" => interp("method", " ", "route"),
        "command" => ruby::to_s(rec.get("command").or(rec.get("name"))),
        "scheduled_task" => ruby::to_s(rec.get("task_key").or(rec.get("name"))),
        "channel_action" => interp("channel", "#", "action"),
        _ => ruby::to_s(rec.get("name")),
    };
    let detail_keys: &[&str] = match kind_s.as_str() {
        "request" => &[
            "url",
            "path",
            "ip",
            "headers",
            "payload",
            "context",
            "user_agent",
            "view_runtime",
            "db_runtime",
            "redirect_to",
            "halted_callback",
            "unpermitted_parameters",
            "rate_limited",
            "format",
            "inertia",
            "request_size",
            "response_size",
            "gc_time",
            "dropped_records",
            "dropped_bytes",
        ],
        "command" => &["context", "gc_time", "interactive", "dropped_records", "dropped_bytes"],
        "channel_action" => &["channel", "context", "gc_time", "dropped_records", "dropped_bytes"],
        _ => &[
            "job_id",
            "provider_job_id",
            "attempt_id",
            "adapter",
            "priority",
            "db_runtime",
            "arguments_preview",
            "context",
            "gc_time",
            "schedule",
            "dropped_records",
            "dropped_bytes",
        ],
    };
    let mut attrs = envelope(rec)?;
    let inertia = rec.get("inertia");
    let partial_only = inertia.get("partial_only");
    let fields: Vec<(&'static str, Value)> = vec![
        ("kind", Value::Str(kind.clone())),
        ("name", Value::Str(name)),
        ("duration", take(rec, "duration")),
        ("status", rec.get("status_code").or(rec.get("exit_code")).clone()),
        ("outcome", take(rec, "status")),
        ("method", take(rec, "method")),
        ("route", take(rec, "route")),
        ("controller", take(rec, "controller")),
        ("action", take(rec, "action")),
        ("queue", take(rec, "queue")),
        ("attempt", take(rec, "attempt")),
        ("queue_latency", take(rec, "queue_latency")),
        ("queue_time", take(rec, "queue_time")),
        ("parent_id", take(rec, "parent_id")),
        ("job_id", take(rec, "job_id")),
        ("task_key", take(rec, "task_key")),
        ("inertia_component", inertia.get("component").clone()),
        ("inertia_partial", Value::Bool(partial_only.present())),
        ("allocations", take(rec, "allocations")),
        ("peak_memory", take(rec, "peak_memory")),
        ("exception_preview", take(rec, "exception_preview")),
        ("stages", or_empty_hash(rec.get("stages"))),
        ("counters", or_empty_hash(rec.get("counters"))),
        ("detail", slice(rec, detail_keys)),
    ];
    for (k, v) in fields {
        set(&mut attrs, k, v);
    }
    Ok(("executions", attrs))
}

/// `value || {}`
fn or_empty_hash(v: &Value) -> Value {
    if v.truthy() { v.clone() } else { Value::Hash(rjson::Hash::new()) }
}

/// `value || []`
fn or_empty_array(v: &Value) -> Value {
    if v.truthy() { v.clone() } else { Value::Array(Vec::new()) }
}

fn exception(rec: &Value) -> R<(&'static str, Attrs)> {
    let mut attrs = envelope(rec)?;
    let locals = match rec.get("locals") {
        v @ Value::Hash(_) => v.clone(),
        _ => Value::Nil,
    };
    for (k, v) in [
        ("class_name", Value::Str(ruby::to_s(rec.get("class")))),
        ("message", Value::Str(ruby::to_s(rec.get("message")))),
        ("handled", Value::Bool(rec.get("handled").truthy())),
        ("severity", take(rec, "severity")),
        ("source", take(rec, "source")),
        ("file", take(rec, "file")),
        ("line", take(rec, "line")),
        ("frames", or_empty_array(rec.get("frames"))),
        ("cause", take(rec, "cause")),
        ("locals", locals),
        ("context", take(rec, "context")),
        ("fingerprint", or_empty_array(rec.get("fingerprint"))),
        ("fingerprint_source", take(rec, "fingerprint_source")),
        ("ruby_version", take(rec, "ruby_version")),
        ("rails_version", take(rec, "rails_version")),
    ] {
        set(&mut attrs, k, v);
    }
    Ok(("exceptions", attrs))
}

fn health(rec: &Value) -> R<(&'static str, Attrs)> {
    Ok((
        "health_samples",
        vec![
            ("sampled_at", Value::Str(timestamp_string(rec.get("timestamp"))?.into_bytes())),
            ("pid", take(rec, "pid")),
            ("role", take(rec, "role")),
            ("server", take(rec, "server")),
            ("deploy", take(rec, "deploy")),
            ("threads_max", take(rec, "threads_max")),
            ("threads_busy", take(rec, "threads_busy")),
            ("backlog", take(rec, "backlog")),
            ("pool_size", take(rec, "pool_size")),
            ("pool_busy", take(rec, "pool_busy")),
            ("pool_waiting", take(rec, "pool_waiting")),
            ("queue_depth", take(rec, "queue_depth")),
            ("queue_latency", take(rec, "queue_latency")),
            ("memory", take(rec, "memory")),
            ("detail", or_empty_hash(rec.get("detail"))),
        ],
    ))
}

fn profile(rec: &Value) -> R<(&'static str, Attrs)> {
    let mut attrs = envelope(rec)?;
    set(&mut attrs, "profiler", take(rec, "profiler"));
    set(&mut attrs, "mode", take(rec, "mode"));
    set(&mut attrs, "interval", take(rec, "interval"));
    set(&mut attrs, "duration", take(rec, "duration"));
    set(&mut attrs, "samples", take(rec, "samples"));
    let declared = declared_bytes(rec.get("stacks_bytes"), "stacks_bytes", PROFILE_MAX_INGEST_BYTES)?;
    set(&mut attrs, "stacks_bytes", Value::Int(declared));
    let stacks = compressed_blob(rec.get("stacks"), "stacks")?;
    crate::ingest::bounded_gzip::verify(&stacks, PROFILE_MAX_INGEST_BYTES as u64, declared, true).map_err(|e| MapError::new(e.class(), e.to_string()))?;
    set(&mut attrs, "stacks", Value::Str(blob_marker(stacks)));
    Ok(("profiles", attrs))
}

fn attachment(rec: &Value) -> R<(&'static str, Attrs)> {
    let mut attrs = envelope(rec)?;
    set(&mut attrs, "name", take(rec, "name"));
    set(&mut attrs, "content_type", take(rec, "content_type"));
    let declared = declared_bytes(rec.get("bytes"), "bytes", ATTACHMENT_MAX_BODY_BYTES)?;
    set(&mut attrs, "bytes", Value::Int(declared));
    let data = compressed_blob(rec.get("data"), "data")?;
    crate::ingest::bounded_gzip::verify(&data, ATTACHMENT_MAX_BODY_BYTES as u64, declared, false).map_err(|e| MapError::new(e.class(), e.to_string()))?;
    set(&mut attrs, "data", Value::Str(blob_marker(data)));
    set(&mut attrs, "exception_group_hash", take(rec, "exception_group_hash"));
    set(&mut attrs, "truncated", Value::Bool(rec.get("truncated").truthy()));
    Ok(("attachments", attrs))
}

/// Binary columns carry raw bytes through Value::Str; serialize binds them as
/// a BLOB (a binary String in Ruby). The marker prefix keeps them apart from
/// ordinary strings, which bind as TEXT.
const BLOB_MARK: &[u8] = b"\x00rw-blob\x00";

fn blob_marker(bytes: Vec<u8>) -> Vec<u8> {
    let mut v = BLOB_MARK.to_vec();
    v.extend(bytes);
    v
}

fn compressed_blob(value: &Value, field: &str) -> R<Vec<u8>> {
    use base64::Engine;
    // Base64.strict_decode64(value.to_s)
    base64::engine::general_purpose::STANDARD.decode(ruby::to_s(value)).map_err(|_| MapError::argument(format!("{field} is not valid base64")))
}

fn declared_bytes(value: &Value, field: &str, max: i64) -> R<i64> {
    let bytes = match value {
        Value::Int(i) => Some(*i),
        Value::Str(s) => {
            let max_digits = max.to_string().len();
            if s.len() <= max_digits && !s.is_empty() && s.iter().all(u8::is_ascii_digit) {
                std::str::from_utf8(s).ok().and_then(|t| t.parse::<i64>().ok())
            } else {
                None
            }
        }
        _ => None,
    };
    match bytes {
        Some(b) if (0..=max).contains(&b) => Ok(b),
        _ => Err(MapError::argument(format!("{field} must be an integer between 0 and {max}"))),
    }
}

// ---------------------------------------------------------------------------
// serialize / cast_value

fn serialize(table: &'static str, attrs: Attrs, truncations: &mut Vec<String>) -> R<Row> {
    let columns = schema().columns(table);
    let mut attrs = attrs;
    let mut out: Row = Vec::with_capacity(attrs.len());
    for column in columns {
        let Some(pos) = attrs.iter().position(|(k, _)| *k == column.name) else { continue };
        let (name, mut value) = std::mem::replace(&mut attrs[pos], ("", Value::Nil));
        if value.is_nil()
            && !column.null
            && let Some(default) = &column.default
        {
            value = default_value(default);
        }
        out.push((name, cast_value(value, column, table, truncations)?));
    }
    Ok(out)
}

/// A column default as the Ruby value AR hands back for it.
fn default_value(d: &DefaultValue) -> Value {
    match d {
        DefaultValue::Int(i) => Value::Int(*i),
        DefaultValue::Float(f) => Value::Float(*f),
        DefaultValue::Bool(b) => Value::Bool(*b),
        // A JSON column's default comes back as the decoded structure.
        DefaultValue::Text(s) if s == "{}" => Value::Hash(rjson::Hash::new()),
        DefaultValue::Text(s) if s == "[]" => Value::Array(Vec::new()),
        DefaultValue::Text(s) => Value::Str(s.as_bytes().to_vec()),
    }
}

const JSON_TRUNCATED: &str = r#"{"truncated":true}"#;
const JSON_ARRAY_TRUNCATED: &str = r#"[{"truncated":true}]"#;

pub fn cast_value(value: Value, column: &Column, table: &str, truncations: &mut Vec<String>) -> R<Bind> {
    if value.is_nil() {
        return Ok(Bind::Null);
    }
    match column.ty {
        ColType::Json => {
            // Health detail is already JSON-encoded on the wire: decode it.
            let value = match value {
                Value::Str(s) => rjson::parse(&sanitize_string(&s)).map_err(|e| MapError::new("JSON::ParserError", e.0))?,
                v => v,
            };
            let sanitized = sanitize_json(value);
            let encoded = rjson::generate(&sanitized).map_err(|e| MapError::new("JSON::GeneratorError", e.0))?;
            let replacement = if matches!(sanitized, Value::Array(_)) { JSON_ARRAY_TRUNCATED } else { JSON_TRUNCATED };
            let limit = text_limit_required(table, &column.name)?;
            return Ok(Bind::Text(cap_text_json(encoded, limit, table, &column.name, truncations, replacement)));
        }
        ColType::Datetime => {
            if let Value::Str(s) = value {
                return Ok(Bind::Text(bytes_to_string(s)));
            }
            // Other values fall through to the generic case below.
            return generic(value, column, table, truncations);
        }
        ColType::Boolean => {
            return match value {
                Value::Bool(b) => Ok(Bind::Int(b as i64)),
                _ => Err(MapError::type_error("expected boolean")),
            };
        }
        _ => {}
    }
    generic(value, column, table, truncations)
}

fn generic(value: Value, column: &Column, table: &str, truncations: &mut Vec<String>) -> R<Bind> {
    if matches!(value, Value::Hash(_) | Value::Array(_)) {
        return Err(MapError::type_error(format!("unexpected {} for :{} column", value.class_name(), ty_name(column.ty))));
    }
    match column.ty {
        ColType::String => {
            let s = sanitize_string(&ruby::to_s(&value));
            Ok(Bind::Text(cap_text(s, column.limit, table, &column.name, truncations)))
        }
        ColType::Text => {
            let s = sanitize_string(&ruby::to_s(&value));
            let limit = text_limit_required(table, &column.name)?;
            Ok(Bind::Text(cap_text(s, Some(limit), table, &column.name, truncations)))
        }
        ColType::Integer => match value {
            // The parser yields Int only for values that fit i64 (Ruby's INTEGER_MIN..INTEGER_MAX);
            // anything wider arrives as BigInt and is out of range.
            Value::Int(i) => Ok(Bind::Int(i)),
            Value::BigInt(_) => Err(MapError::new("RangeError", "integer out of range")),
            _ => Err(MapError::type_error("expected integer")),
        },
        ColType::Float => {
            let Some(f) = value.to_f() else { return Err(MapError::type_error("expected number")) };
            if !f.is_finite() {
                return Err(MapError::new("RangeError", "number out of range"));
            }
            Ok(Bind::Float(f))
        }
        ColType::Binary => match value {
            Value::Str(s) if s.starts_with(BLOB_MARK) => Ok(Bind::Blob(s[BLOB_MARK.len()..].to_vec())),
            Value::Str(s) => Ok(Bind::Text(bytes_to_string(s))),
            other => Ok(Bind::Text(bytes_to_string(ruby::to_s(&other)))),
        },
        // A datetime given a non-String (an Integer, say) is bound as is.
        ColType::Datetime => Ok(match value {
            Value::Int(i) => Bind::Int(i),
            Value::Float(f) => Bind::Float(f),
            Value::Bool(b) => Bind::Unbindable(if b { "TrueClass" } else { "FalseClass" }),
            Value::BigInt(s) => Bind::Text(s),
            other => Bind::Text(bytes_to_string(ruby::to_s(&other))),
        }),
        ColType::Boolean | ColType::Json => unreachable!(),
    }
}

fn ty_name(t: ColType) -> &'static str {
    match t {
        ColType::String => "string",
        ColType::Text => "text",
        ColType::Integer => "integer",
        ColType::Float => "float",
        ColType::Datetime => "datetime",
        ColType::Boolean => "boolean",
        ColType::Json => "json",
        ColType::Binary => "binary",
    }
}

/// TEXT_LIMITS.fetch: a json/text column without a cap raises KeyError.
fn text_limit_required(table: &str, column: &str) -> R<usize> {
    let key = format!("{table}.{column}");
    schema()
        .text_limits
        .get(&key)
        .copied()
        .ok_or_else(|| MapError::new("KeyError", format!("key not found: {}", String::from_utf8_lossy(&ruby::inspect(&Value::Str(key.into_bytes()))))))
}

/// Strings bound by the Ruby writer are UTF-8 Ruby strings; sanitize_string
/// has already scrubbed them, so this conversion is lossless in practice.
fn bytes_to_string(s: Vec<u8>) -> String {
    String::from_utf8(s).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

fn cap_text(value: Vec<u8>, limit: Option<usize>, table: &str, column: &str, truncations: &mut Vec<String>) -> String {
    match limit {
        Some(limit) if value.len() > limit => {
            truncations.push(format!("{table}.{column}"));
            // byteslice(0, limit).scrub(""): the value is valid UTF-8 here
            // (sanitized), so only a character split at the cut is dropped.
            let cut = &value[..limit];
            let valid = match std::str::from_utf8(cut) {
                Ok(_) => cut.len(),
                Err(e) => e.valid_up_to(),
            };
            bytes_to_string(cut[..valid].to_vec())
        }
        _ => bytes_to_string(value),
    }
}

fn cap_text_json(encoded: String, limit: usize, table: &str, column: &str, truncations: &mut Vec<String>, replacement: &str) -> String {
    if encoded.len() > limit {
        truncations.push(format!("{table}.{column}"));
        replacement.to_string()
    } else {
        encoded
    }
}

fn sanitize_json(value: Value) -> Value {
    match value {
        Value::Hash(h) => {
            let mut out = rjson::Hash::with_capacity(h.len());
            for (k, v) in h {
                out.insert(sanitize_string(&k), sanitize_json(v));
            }
            Value::Hash(out)
        }
        Value::Array(a) => Value::Array(a.into_iter().map(sanitize_json).collect()),
        Value::Str(s) => Value::Str(sanitize_string(&s)),
        v => v,
    }
}

/// `value.scrub.delete(CONTROL_RANGE)`, skipped when nothing needs it.
pub fn sanitize_string(s: &[u8]) -> Vec<u8> {
    let control = |b: &u8| matches!(*b, 0x00..=0x08 | 0x0B | 0x0C | 0x0E..=0x1F | 0x7F);
    let valid = ruby::valid_utf8(s);
    if valid && !s.iter().any(control) {
        return s.to_vec();
    }
    let scrubbed = if valid { s.to_vec() } else { ruby::scrub(s) };
    scrubbed.into_iter().filter(|b| !control(b)).collect()
}

/// Mapper.timestamp_string: microseconds rounded half away from zero, then
/// the UTC calendar time of the whole seconds.
pub fn timestamp_string(value: &Value) -> R<String> {
    let f = ruby::float(value).map_err(|(class, message)| MapError::new(class, message))?;
    let scaled = f * 1_000_000.0;
    if !scaled.is_finite() {
        return Err(MapError::new("FloatDomainError", ruby::float_to_s(scaled)));
    }
    // Float#round -> Integer (exact), then integer floor division.
    let micro = scaled.round();
    if micro.abs() >= 9.0e18 {
        return Err(MapError::new("RangeError", "timestamp out of range"));
    }
    let micro = micro as i64;
    let seconds = micro.div_euclid(1_000_000);
    let usec = micro.rem_euclid(1_000_000);
    Ok(format!("{}.{:06}", crate::time::utc_seconds_string(seconds), usec))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_string_matches_ruby() {
        let ts = |f: f64| timestamp_string(&Value::Float(f)).unwrap();
        assert_eq!(ts(1767225600.0), "2026-01-01 00:00:00.000000");
        assert_eq!(ts(1767225600.123), "2026-01-01 00:00:00.123000");
        assert_eq!(ts(-1.5), "1969-12-31 23:59:58.500000");
        assert_eq!(ts(253402300800.5), "10000-01-01 00:00:00.500000");
        assert_eq!(timestamp_string(&Value::Str(b"0x10".to_vec())).unwrap(), "1970-01-01 00:00:16.000000");
        assert_eq!(timestamp_string(&Value::Bool(true)).unwrap_err(), MapError::new("TypeError", "can't convert true into Float"));
    }

    // railwatch 1.2 puts dropped_records/dropped_bytes on a parent whose
    // tree outgrew the gem's per-execution buffer. They live in detail,
    // where the execution page and MCP get_execution read them.
    #[test]
    fn a_truncated_parent_keeps_its_dropped_counts_in_detail() {
        for t in ["request", "job_attempt", "scheduled_task", "command", "channel_action"] {
            let rec =
                rjson::parse(format!(r#"{{"v":1,"t":"{t}","timestamp":1767225600.0,"duration":1,"dropped_records":21,"dropped_bytes":55977}}"#).as_bytes())
                    .unwrap();
            let mapped = row_for(&rec, &mut vec![]).unwrap().unwrap();
            let detail = mapped.row.iter().find(|(k, _)| *k == "detail").map(|(_, v)| v.clone());
            let Some(Bind::Text(detail)) = detail else { panic!("{t}: no detail") };
            let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
            assert_eq!(detail["dropped_records"], 21, "{t}");
            assert_eq!(detail["dropped_bytes"], 55977, "{t}");
        }
    }
}
