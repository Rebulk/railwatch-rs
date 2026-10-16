//! The control-database tables this track owns, with railwatch-cloud's own
//! DDL: copied verbatim from sqlite_master of rw-baseline's
//! storage/production.sqlite3 (railwatch-cloud @ 464f0fd, db/schema.rb version
//! 2026_09_20_200000), only `IF NOT EXISTS` added. Same table names, column
//! names, types, defaults, indexes and foreign keys as Rails, so p3-control
//! can include this module as is.
//!
//! Tables: issues, issue_activities, comments, alerts, alert_rules,
//! integrations, thresholds, anomaly_rules, deploys, ingest_followup_receipts.
//!
//! Applied as control_schema migration 3 (after rails_primary, so the
//! accounts/applications/environments/users the foreign keys name exist).
//! `deploys` already exists in Phase 1's control schema with the same column
//! names; `IF NOT EXISTS` keeps whichever came first.
//!
//! Plus one Rust-only table, `environment_heartbeats` (the watchdog's state,
//! docs/DIFFERENCES.md), which has no Rails counterpart.

pub const DDL: &[&str] = &[
    // table alert_rules
    r#"CREATE TABLE IF NOT EXISTS "alert_rules" ("id" integer PRIMARY KEY AUTOINCREMENT NOT NULL, "application_id" integer NOT NULL, "created_at" datetime(6) NOT NULL, "event" varchar NOT NULL, "filters" json DEFAULT '{}' NOT NULL, "integration_id" integer NOT NULL, "updated_at" datetime(6) NOT NULL, CONSTRAINT "fk_rails_e0d44bf6c7"
FOREIGN KEY ("application_id")
  REFERENCES "applications" ("id")
, CONSTRAINT "fk_rails_306e4ed2a0"
FOREIGN KEY ("integration_id")
  REFERENCES "integrations" ("id")
)"#,
    // index index_alert_rules_on_application_id
    r#"CREATE INDEX IF NOT EXISTS "index_alert_rules_on_application_id" ON "alert_rules" ("application_id")"#,
    // index index_alert_rules_on_integration_id
    r#"CREATE INDEX IF NOT EXISTS "index_alert_rules_on_integration_id" ON "alert_rules" ("integration_id")"#,
    // table alerts
    r#"CREATE TABLE IF NOT EXISTS "alerts" ("id" integer PRIMARY KEY AUTOINCREMENT NOT NULL, "alert_rule_id" integer, "burst_key" varchar, "created_at" datetime(6) NOT NULL, "delivery_attempts" integer DEFAULT 0 NOT NULL, "delivery_enqueued_until" datetime(6), "delivery_key" varchar, "delivery_lease_expires_at" datetime(6), "delivery_lease_id" varchar, "error" text, "event" varchar NOT NULL, "integration_id" integer NOT NULL, "issue_id" integer, "next_delivery_at" datetime(6), "payload" json DEFAULT '{}' NOT NULL, "sent_at" datetime(6), "status" varchar DEFAULT 'pending' NOT NULL, "summary_alert_id" integer, "updated_at" datetime(6) NOT NULL, CONSTRAINT "fk_rails_530b3fb2d0"
FOREIGN KEY ("integration_id")
  REFERENCES "integrations" ("id")
, CONSTRAINT "fk_rails_7814415382"
FOREIGN KEY ("alert_rule_id")
  REFERENCES "alert_rules" ("id")
, CONSTRAINT "fk_rails_ded9a2b152"
FOREIGN KEY ("summary_alert_id")
  REFERENCES "alerts" ("id")
 ON DELETE SET NULL, CONSTRAINT "fk_rails_1f6d662120"
FOREIGN KEY ("issue_id")
  REFERENCES "issues" ("id")
)"#,
    // index index_alerts_on_alert_rule_id
    r#"CREATE INDEX IF NOT EXISTS "index_alerts_on_alert_rule_id" ON "alerts" ("alert_rule_id")"#,
    // index index_alerts_on_burst_key
    r#"CREATE UNIQUE INDEX IF NOT EXISTS "index_alerts_on_burst_key" ON "alerts" ("burst_key")"#,
    // index index_alerts_on_delivery_key
    r#"CREATE UNIQUE INDEX IF NOT EXISTS "index_alerts_on_delivery_key" ON "alerts" ("delivery_key")"#,
    // index index_alerts_on_integration_id
    r#"CREATE INDEX IF NOT EXISTS "index_alerts_on_integration_id" ON "alerts" ("integration_id")"#,
    // index index_alerts_on_issue_id_and_event_and_created_at
    r#"CREATE INDEX IF NOT EXISTS "index_alerts_on_issue_id_and_event_and_created_at" ON "alerts" ("issue_id", "event", "created_at")"#,
    // index index_alerts_on_issue_id
    r#"CREATE INDEX IF NOT EXISTS "index_alerts_on_issue_id" ON "alerts" ("issue_id")"#,
    // index index_alerts_for_burst_reconciliation
    r#"CREATE INDEX IF NOT EXISTS "index_alerts_for_burst_reconciliation" ON "alerts" ("status", "created_at", "alert_rule_id")"#,
    // index index_alerts_for_stale_delivery_reconciliation
    r#"CREATE INDEX IF NOT EXISTS "index_alerts_for_stale_delivery_reconciliation" ON "alerts" ("status", "delivery_lease_expires_at")"#,
    // index index_alerts_for_delivery_reconciliation
    r#"CREATE INDEX IF NOT EXISTS "index_alerts_for_delivery_reconciliation" ON "alerts" ("status", "next_delivery_at", "delivery_enqueued_until")"#,
    // index index_alerts_on_summary_alert_id
    r#"CREATE INDEX IF NOT EXISTS "index_alerts_on_summary_alert_id" ON "alerts" ("summary_alert_id")"#,
    // table anomaly_rules
    r#"CREATE TABLE IF NOT EXISTS "anomaly_rules" ("id" integer PRIMARY KEY AUTOINCREMENT NOT NULL, "baseline_days" integer DEFAULT 7 NOT NULL, "created_at" datetime(6) NOT NULL, "deviation" float DEFAULT 3.0 NOT NULL, "enabled" boolean DEFAULT TRUE NOT NULL, "environment_id" integer NOT NULL, "last_fired_at" datetime(6), "metric" varchar NOT NULL, "target" varchar DEFAULT '*' NOT NULL, "target_kind" varchar NOT NULL, "updated_at" datetime(6) NOT NULL, "window_minutes" integer DEFAULT 15 NOT NULL, CONSTRAINT "fk_rails_b965f1c1fb"
FOREIGN KEY ("environment_id")
  REFERENCES "environments" ("id")
)"#,
    // index index_anomaly_rules_on_environment_id
    r#"CREATE INDEX IF NOT EXISTS "index_anomaly_rules_on_environment_id" ON "anomaly_rules" ("environment_id")"#,
    // table comments
    r#"CREATE TABLE IF NOT EXISTS "comments" ("id" integer PRIMARY KEY AUTOINCREMENT NOT NULL, "author_name" varchar, "body" text NOT NULL, "created_at" datetime(6) NOT NULL, "external_id" varchar, "external_updated_at" datetime(6), "issue_id" integer NOT NULL, "source" varchar DEFAULT 'railwatch' NOT NULL, "updated_at" datetime(6) NOT NULL, "user_id" integer, CONSTRAINT "fk_rails_90ef1875d1"
FOREIGN KEY ("issue_id")
  REFERENCES "issues" ("id")
, CONSTRAINT "fk_rails_03de2dc08c"
FOREIGN KEY ("user_id")
  REFERENCES "users" ("id")
)"#,
    // index index_comments_on_external_identity
    r#"CREATE UNIQUE INDEX IF NOT EXISTS "index_comments_on_external_identity" ON "comments" ("issue_id", "source", "external_id") WHERE external_id IS NOT NULL"#,
    // index index_comments_on_issue_id
    r#"CREATE INDEX IF NOT EXISTS "index_comments_on_issue_id" ON "comments" ("issue_id")"#,
    // index index_comments_on_user_id
    r#"CREATE INDEX IF NOT EXISTS "index_comments_on_user_id" ON "comments" ("user_id")"#,
    // table deploys
    r#"CREATE TABLE IF NOT EXISTS "deploys" ("id" integer PRIMARY KEY AUTOINCREMENT NOT NULL, "commits" json DEFAULT '[]' NOT NULL, "created_at" datetime(6) NOT NULL, "deploy" varchar(128) NOT NULL, "deployed_at" datetime(6) NOT NULL, "detail" json DEFAULT '{}' NOT NULL, "environment_id" integer NOT NULL, "name" varchar, "previous_ref" varchar(128), "ref" varchar(128), "server" varchar, "updated_at" datetime(6) NOT NULL, "url" varchar, CONSTRAINT "fk_rails_fda0a7a563"
FOREIGN KEY ("environment_id")
  REFERENCES "environments" ("id")
)"#,
    // index index_deploys_on_environment_id_and_deploy
    r#"CREATE UNIQUE INDEX IF NOT EXISTS "index_deploys_on_environment_id_and_deploy" ON "deploys" ("environment_id", "deploy")"#,
    // index index_deploys_on_environment_id_and_deployed_at
    r#"CREATE INDEX IF NOT EXISTS "index_deploys_on_environment_id_and_deployed_at" ON "deploys" ("environment_id", "deployed_at")"#,
    // index index_deploys_on_environment_id
    r#"CREATE INDEX IF NOT EXISTS "index_deploys_on_environment_id" ON "deploys" ("environment_id")"#,
    // table ingest_followup_receipts
    r#"CREATE TABLE IF NOT EXISTS "ingest_followup_receipts" ("id" integer PRIMARY KEY AUTOINCREMENT NOT NULL, "created_at" datetime(6) NOT NULL, "delivery_created_at" datetime(6) NOT NULL, "delivery_id" varchar(36) NOT NULL, "effect_key" varchar(96) NOT NULL, "environment_id" integer NOT NULL, "producer_id" varchar(36) NOT NULL, CONSTRAINT "fk_rails_b29fa34456"
FOREIGN KEY ("environment_id")
  REFERENCES "environments" ("id")
)"#,
    // index index_ingest_followup_receipts_on_environment_and_delivery_time
    r#"CREATE INDEX IF NOT EXISTS "index_ingest_followup_receipts_on_environment_and_delivery_time" ON "ingest_followup_receipts" ("environment_id", "delivery_created_at")"#,
    // index index_ingest_followup_receipts_on_effect
    r#"CREATE UNIQUE INDEX IF NOT EXISTS "index_ingest_followup_receipts_on_effect" ON "ingest_followup_receipts" ("environment_id", "producer_id", "delivery_id", "effect_key")"#,
    // index index_ingest_followup_receipts_on_environment_id
    r#"CREATE INDEX IF NOT EXISTS "index_ingest_followup_receipts_on_environment_id" ON "ingest_followup_receipts" ("environment_id")"#,
    // table integrations
    r#"CREATE TABLE IF NOT EXISTS "integrations" ("id" integer PRIMARY KEY AUTOINCREMENT NOT NULL, "account_id" integer NOT NULL, "created_at" datetime(6) NOT NULL, "enabled" boolean DEFAULT TRUE NOT NULL, "encrypted_settings" text, "external_id" varchar, "kind" varchar NOT NULL, "last_delivered_at" datetime(6), "last_error" text, "last_received_at" datetime(6), "last_synced_at" datetime(6), "name" varchar NOT NULL, "settings" json DEFAULT '{}' NOT NULL, "updated_at" datetime(6) NOT NULL, CONSTRAINT "fk_rails_63f87b241e"
FOREIGN KEY ("account_id")
  REFERENCES "accounts" ("id")
)"#,
    // index index_integrations_on_account_kind_external
    r#"CREATE UNIQUE INDEX IF NOT EXISTS "index_integrations_on_account_kind_external" ON "integrations" ("account_id", "kind", "external_id") WHERE external_id IS NOT NULL"#,
    // index index_integrations_on_account_id
    r#"CREATE INDEX IF NOT EXISTS "index_integrations_on_account_id" ON "integrations" ("account_id")"#,
    // index index_integrations_on_kind_and_external_id
    r#"CREATE INDEX IF NOT EXISTS "index_integrations_on_kind_and_external_id" ON "integrations" ("kind", "external_id")"#,
    // table issue_activities
    r#"CREATE TABLE IF NOT EXISTS "issue_activities" ("id" integer PRIMARY KEY AUTOINCREMENT NOT NULL, "created_at" datetime(6) NOT NULL, "data" json DEFAULT '{}' NOT NULL, "issue_id" integer NOT NULL, "kind" varchar NOT NULL, "user_id" integer, CONSTRAINT "fk_rails_86a52f69c1"
FOREIGN KEY ("issue_id")
  REFERENCES "issues" ("id")
, CONSTRAINT "fk_rails_286a746468"
FOREIGN KEY ("user_id")
  REFERENCES "users" ("id")
)"#,
    // index index_issue_activities_on_issue_id_and_created_at
    r#"CREATE INDEX IF NOT EXISTS "index_issue_activities_on_issue_id_and_created_at" ON "issue_activities" ("issue_id", "created_at")"#,
    // index index_issue_activities_on_user_id
    r#"CREATE INDEX IF NOT EXISTS "index_issue_activities_on_user_id" ON "issue_activities" ("user_id")"#,
    // table issues
    r#"CREATE TABLE IF NOT EXISTS "issues" ("id" integer PRIMARY KEY AUTOINCREMENT NOT NULL, "affected_users" integer DEFAULT 0 NOT NULL, "application_id" integer NOT NULL, "assignee_id" integer, "created_at" datetime(6) NOT NULL, "culprit" varchar, "environment_id" integer NOT NULL, "first_seen_at" datetime(6) NOT NULL, "group_hash" varchar(32) NOT NULL, "kind" varchar DEFAULT 'exception' NOT NULL, "last_seen_at" datetime(6) NOT NULL, "merged_into_id" integer, "number" integer NOT NULL, "occurrences" bigint DEFAULT 0 NOT NULL, "priority" varchar DEFAULT 'normal' NOT NULL, "regressed_at" datetime(6), "resolved_at" datetime(6), "resolved_in_deploy" varchar(128), "sample" json DEFAULT '{}' NOT NULL, "source" varchar(128), "status" varchar DEFAULT 'open' NOT NULL, "title" varchar NOT NULL, "updated_at" datetime(6) NOT NULL, CONSTRAINT "fk_rails_8be071551b"
FOREIGN KEY ("merged_into_id")
  REFERENCES "issues" ("id")
, CONSTRAINT "fk_rails_559944034e"
FOREIGN KEY ("application_id")
  REFERENCES "applications" ("id")
, CONSTRAINT "fk_rails_b63f23bce3"
FOREIGN KEY ("environment_id")
  REFERENCES "environments" ("id")
, CONSTRAINT "fk_rails_ff669b5916"
FOREIGN KEY ("assignee_id")
  REFERENCES "users" ("id")
)"#,
    // index index_issues_on_application_id_and_number
    r#"CREATE UNIQUE INDEX IF NOT EXISTS "index_issues_on_application_id_and_number" ON "issues" ("application_id", "number")"#,
    // index index_issues_on_application_id
    r#"CREATE INDEX IF NOT EXISTS "index_issues_on_application_id" ON "issues" ("application_id")"#,
    // index index_issues_on_assignee_id
    r#"CREATE INDEX IF NOT EXISTS "index_issues_on_assignee_id" ON "issues" ("assignee_id")"#,
    // index index_issues_on_environment_id_and_group_hash
    r#"CREATE UNIQUE INDEX IF NOT EXISTS "index_issues_on_environment_id_and_group_hash" ON "issues" ("environment_id", "group_hash")"#,
    // index index_issues_on_environment_id_and_status_and_last_seen_at
    r#"CREATE INDEX IF NOT EXISTS "index_issues_on_environment_id_and_status_and_last_seen_at" ON "issues" ("environment_id", "status", "last_seen_at")"#,
    // index index_issues_on_environment_id
    r#"CREATE INDEX IF NOT EXISTS "index_issues_on_environment_id" ON "issues" ("environment_id")"#,
    // index index_issues_on_merged_into_id
    r#"CREATE INDEX IF NOT EXISTS "index_issues_on_merged_into_id" ON "issues" ("merged_into_id")"#,
    // table thresholds
    r#"CREATE TABLE IF NOT EXISTS "thresholds" ("id" integer PRIMARY KEY AUTOINCREMENT NOT NULL, "created_at" datetime(6) NOT NULL, "environment_id" integer NOT NULL, "limit" float NOT NULL, "metric" varchar DEFAULT 'p95' NOT NULL, "target" varchar DEFAULT '*' NOT NULL, "target_kind" varchar NOT NULL, "updated_at" datetime(6) NOT NULL, "window_minutes" integer DEFAULT 5 NOT NULL, CONSTRAINT "fk_rails_8f11e3d602"
FOREIGN KEY ("environment_id")
  REFERENCES "environments" ("id")
)"#,
    // index index_thresholds_unique
    r#"CREATE UNIQUE INDEX IF NOT EXISTS "index_thresholds_unique" ON "thresholds" ("environment_id", "target_kind", "target", "metric")"#,
    // index index_thresholds_on_environment_id
    r#"CREATE INDEX IF NOT EXISTS "index_thresholds_on_environment_id" ON "thresholds" ("environment_id")"#,
];

pub const RUST_ONLY: &str = "
CREATE TABLE IF NOT EXISTS environment_heartbeats (
  environment_id INTEGER NOT NULL,
  server TEXT NOT NULL,
  -- epoch microseconds of the newest committed record from this server
  last_seen_micros INTEGER NOT NULL,
  -- epoch microseconds when the silence alert fired; NULL while healthy
  silent_since_micros INTEGER,
  PRIMARY KEY (environment_id, server)
);
";
