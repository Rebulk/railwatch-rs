//! Detector context for performance/anomaly issues, ported from
//! railwatch-cloud: IssueDetectionSnapshot (captured by the detectors into
//! `issues.sample["detection"]`, with DetectionSnapshotting's carry-forward)
//! and IssueDetectionPresenter (the `detection` prop of issues#show and MCP
//! get_issue).

pub mod presenter;
pub mod snapshot;

/// IssueDetectionSnapshot::EXECUTION_TYPES
pub const EXECUTION_TYPES: &[&str] = &["request", "job_attempt", "scheduled_task", "command"];
/// IssueDetectionSnapshot::REPRESENTATIVE_LIMIT
pub const REPRESENTATIVE_LIMIT: usize = 10;

/// IssueDetectionSnapshot::UNITS
pub fn unit(metric: &str) -> Option<&'static str> {
    Some(match metric {
        "p95" | "max" | "avg" => "milliseconds",
        "error_rate" | "failure_rate" | "truncation_rate" => "percent",
        "count" => "events_per_minute",
        "missed" => "schedule",
        "spend" => "usd",
        "tokens" => "tokens",
        _ => return None,
    })
}

/// The telemetry table a detection type's records live in, and whether it
/// is `executions` (filtered by kind). None: unsupported (Rails raises
/// ArgumentError).
pub fn table_for(telemetry_type: &str) -> Option<(&'static str, bool)> {
    if EXECUTION_TYPES.contains(&telemetry_type) {
        return Some(("executions", true));
    }
    Some(match telemetry_type {
        "query" => ("queries", false),
        "outgoing_request" => ("outgoing_requests", false),
        "llm_call" | "llm_tool" => ("llm_calls", false),
        _ => return None,
    })
}
