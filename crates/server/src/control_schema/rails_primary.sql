-- railwatch-cloud @ 464f0fd primary schema (db/schema.rb version 2026_09_20_200000),
-- copied from sqlite_master of a Rails-created production.sqlite3, for the tables
-- railwatch-rs owns. Issues/alerts and friends are in issues.rs (p2-rollups).
CREATE TABLE "accounts" ("id" integer PRIMARY KEY AUTOINCREMENT NOT NULL, "additional_events_cap" bigint DEFAULT 0 NOT NULL, "auto_resolve_after_days" integer DEFAULT 14, "created_at" datetime(6) NOT NULL, "monthly_event_quota" bigint DEFAULT 1000000 NOT NULL, "name" varchar NOT NULL, "plan" varchar DEFAULT 'free' NOT NULL, "quota_alerted_100_at" date, "quota_alerted_80_at" date, "retention_days" integer DEFAULT 30 NOT NULL, "slug" varchar NOT NULL, "updated_at" datetime(6) NOT NULL, CONSTRAINT accounts_retention_days_in_supported_range CHECK (retention_days BETWEEN 1 AND 3650 AND retention_days = CAST(retention_days AS INTEGER)));
CREATE UNIQUE INDEX "index_accounts_on_slug" ON "accounts" ("slug");
CREATE TABLE "users" ("id" integer PRIMARY KEY AUTOINCREMENT NOT NULL, "created_at" datetime(6) NOT NULL, "editor" varchar DEFAULT 'github' NOT NULL, "editor_root" varchar, "email" varchar NOT NULL, "name" varchar NOT NULL, "password_digest" varchar NOT NULL, "password_reset_sent_at" datetime(6), "provider" varchar, "terms_accepted_at" datetime(6), "uid" varchar, "updated_at" datetime(6) NOT NULL, "verified" boolean DEFAULT FALSE NOT NULL, "weekly_digest" boolean DEFAULT TRUE NOT NULL);
CREATE UNIQUE INDEX "index_users_on_email" ON "users" ("email");
CREATE UNIQUE INDEX "index_users_on_provider_and_uid" ON "users" ("provider", "uid");
CREATE TABLE "sessions" ("id" integer PRIMARY KEY AUTOINCREMENT NOT NULL, "authenticated_at" datetime(6), "created_at" datetime(6) NOT NULL, "ip_address" varchar, "updated_at" datetime(6) NOT NULL, "user_agent" varchar, "user_id" integer NOT NULL, CONSTRAINT "fk_rails_758836b4f0"
FOREIGN KEY ("user_id")
  REFERENCES "users" ("id")
);
CREATE INDEX "index_sessions_on_user_id" ON "sessions" ("user_id");
CREATE TABLE "memberships" ("id" integer PRIMARY KEY AUTOINCREMENT NOT NULL, "account_id" integer NOT NULL, "created_at" datetime(6) NOT NULL, "role" varchar DEFAULT 'member' NOT NULL, "updated_at" datetime(6) NOT NULL, "user_id" integer NOT NULL, CONSTRAINT "fk_rails_edbc202c67"
FOREIGN KEY ("account_id")
  REFERENCES "accounts" ("id")
, CONSTRAINT "fk_rails_99326fb65d"
FOREIGN KEY ("user_id")
  REFERENCES "users" ("id")
);
CREATE INDEX "index_memberships_on_account_id" ON "memberships" ("account_id");
CREATE UNIQUE INDEX "index_memberships_on_account_id_and_user_id" ON "memberships" ("account_id", "user_id");
CREATE INDEX "index_memberships_on_user_id" ON "memberships" ("user_id");
CREATE TABLE "invitations" ("id" integer PRIMARY KEY AUTOINCREMENT NOT NULL, "accepted_at" datetime(6), "account_id" integer NOT NULL, "created_at" datetime(6) NOT NULL, "email" varchar NOT NULL, "expires_at" datetime(6) NOT NULL, "inviter_id" integer NOT NULL, "role" varchar DEFAULT 'member' NOT NULL, "token_digest" varchar NOT NULL, "updated_at" datetime(6) NOT NULL, CONSTRAINT "fk_rails_efd7f68691"
FOREIGN KEY ("account_id")
  REFERENCES "accounts" ("id")
, CONSTRAINT "fk_rails_7480156672"
FOREIGN KEY ("inviter_id")
  REFERENCES "users" ("id")
);
CREATE INDEX "index_invitations_on_account_id" ON "invitations" ("account_id");
CREATE UNIQUE INDEX "index_invitations_on_account_id_and_email" ON "invitations" ("account_id", "email") WHERE accepted_at IS NULL;
CREATE INDEX "index_invitations_on_inviter_id" ON "invitations" ("inviter_id");
CREATE UNIQUE INDEX "index_invitations_on_token_digest" ON "invitations" ("token_digest");
CREATE TABLE "applications" ("id" integer PRIMARY KEY AUTOINCREMENT NOT NULL, "account_id" integer NOT NULL, "created_at" datetime(6) NOT NULL, "default_branch" varchar DEFAULT 'main' NOT NULL, "issue_prefix" varchar(8) NOT NULL, "issues_count" integer DEFAULT 0 NOT NULL, "name" varchar NOT NULL, "repository_url" varchar, "slug" varchar NOT NULL, "updated_at" datetime(6) NOT NULL, CONSTRAINT "fk_rails_ba46a1ce3e"
FOREIGN KEY ("account_id")
  REFERENCES "accounts" ("id")
);
CREATE INDEX "index_applications_on_account_id" ON "applications" ("account_id");
CREATE UNIQUE INDEX "index_applications_on_account_id_and_slug" ON "applications" ("account_id", "slug");
CREATE TABLE "environments" ("id" integer PRIMARY KEY AUTOINCREMENT NOT NULL, "application_id" integer NOT NULL, "crash_free_threshold" float DEFAULT 99.0, "created_at" datetime(6) NOT NULL, "events_this_month" bigint DEFAULT 0 NOT NULL, "expected_servers" json DEFAULT '[]' NOT NULL, "ingest_followup_gc_before" datetime(6) DEFAULT '1970-01-01 00:00:00' NOT NULL, "kamal_config" json DEFAULT '{}' NOT NULL, "last_seen_at" datetime(6), "name" varchar NOT NULL, "paused_at" datetime(6), "raw_pruned_through" datetime(6), "slug" varchar NOT NULL, "token_digest" varchar NOT NULL, "token_prefix" varchar(12) NOT NULL, "updated_at" datetime(6) NOT NULL, "usage_month" date, CONSTRAINT "fk_rails_8c2f0b0160"
FOREIGN KEY ("application_id")
  REFERENCES "applications" ("id")
);
CREATE INDEX "index_environments_on_application_id" ON "environments" ("application_id");
CREATE UNIQUE INDEX "index_environments_on_application_id_and_name" ON "environments" ("application_id", "name");
CREATE UNIQUE INDEX "index_environments_on_slug" ON "environments" ("slug");
CREATE UNIQUE INDEX "index_environments_on_token_digest" ON "environments" ("token_digest");
CREATE TABLE "api_tokens" ("id" integer PRIMARY KEY AUTOINCREMENT NOT NULL, "account_id" integer, "application_id" integer, "created_at" datetime(6) NOT NULL, "expires_at" datetime(6), "last_used_at" datetime(6), "name" varchar NOT NULL, "revoked_at" datetime(6), "scopes" json DEFAULT '["api","mcp","read","write"]' NOT NULL, "token_digest" varchar NOT NULL, "updated_at" datetime(6) NOT NULL, "user_id" integer NOT NULL, CONSTRAINT "fk_rails_2de9c2c283"
FOREIGN KEY ("application_id")
  REFERENCES "applications" ("id")
, CONSTRAINT "fk_rails_899213cf3b"
FOREIGN KEY ("account_id")
  REFERENCES "accounts" ("id")
, CONSTRAINT "fk_rails_f16b5e0447"
FOREIGN KEY ("user_id")
  REFERENCES "users" ("id")
);
CREATE INDEX "index_api_tokens_on_account_id" ON "api_tokens" ("account_id");
CREATE INDEX "index_api_tokens_on_application_id" ON "api_tokens" ("application_id");
CREATE UNIQUE INDEX "index_api_tokens_on_token_digest" ON "api_tokens" ("token_digest");
CREATE INDEX "index_api_tokens_on_user_id" ON "api_tokens" ("user_id");
CREATE INDEX "index_api_tokens_on_user_id_and_revoked_at" ON "api_tokens" ("user_id", "revoked_at");
CREATE TABLE "deploys" ("id" integer PRIMARY KEY AUTOINCREMENT NOT NULL, "commits" json DEFAULT '[]' NOT NULL, "created_at" datetime(6) NOT NULL, "deploy" varchar(128) NOT NULL, "deployed_at" datetime(6) NOT NULL, "detail" json DEFAULT '{}' NOT NULL, "environment_id" integer NOT NULL, "name" varchar, "previous_ref" varchar(128), "ref" varchar(128), "server" varchar, "updated_at" datetime(6) NOT NULL, "url" varchar, CONSTRAINT "fk_rails_fda0a7a563"
FOREIGN KEY ("environment_id")
  REFERENCES "environments" ("id")
);
CREATE INDEX "index_deploys_on_environment_id" ON "deploys" ("environment_id");
CREATE UNIQUE INDEX "index_deploys_on_environment_id_and_deploy" ON "deploys" ("environment_id", "deploy");
CREATE INDEX "index_deploys_on_environment_id_and_deployed_at" ON "deploys" ("environment_id", "deployed_at");
CREATE TABLE "saved_views" ("id" integer PRIMARY KEY AUTOINCREMENT NOT NULL, "created_at" datetime(6) NOT NULL, "environment_id" integer NOT NULL, "name" varchar NOT NULL, "page" varchar NOT NULL, "params" json DEFAULT '{}' NOT NULL, "pinned" boolean DEFAULT TRUE NOT NULL, "query" varchar, "shared" boolean DEFAULT TRUE NOT NULL, "updated_at" datetime(6) NOT NULL, "user_id" integer NOT NULL, "window" varchar(8), CONSTRAINT "fk_rails_cb4a1ab7e5"
FOREIGN KEY ("environment_id")
  REFERENCES "environments" ("id")
, CONSTRAINT "fk_rails_ecb864ffd2"
FOREIGN KEY ("user_id")
  REFERENCES "users" ("id")
);
CREATE INDEX "index_saved_views_on_environment_id" ON "saved_views" ("environment_id");
CREATE INDEX "index_saved_views_on_environment_id_and_page" ON "saved_views" ("environment_id", "page");
CREATE INDEX "index_saved_views_on_user_id" ON "saved_views" ("user_id");
CREATE TABLE "ingest_usage_months" ("id" integer PRIMARY KEY AUTOINCREMENT NOT NULL, "created_at" datetime(6) NOT NULL, "environment_id" integer NOT NULL, "events" bigint DEFAULT 0 NOT NULL, "month" date NOT NULL, "updated_at" datetime(6) NOT NULL, CONSTRAINT "fk_rails_2193d660c8"
FOREIGN KEY ("environment_id")
  REFERENCES "environments" ("id")
);
CREATE INDEX "index_ingest_usage_months_on_environment_id" ON "ingest_usage_months" ("environment_id");
CREATE UNIQUE INDEX "index_ingest_usage_months_on_environment_id_and_month" ON "ingest_usage_months" ("environment_id", "month");
CREATE TABLE "ingest_followup_receipts" ("id" integer PRIMARY KEY AUTOINCREMENT NOT NULL, "created_at" datetime(6) NOT NULL, "delivery_created_at" datetime(6) NOT NULL, "delivery_id" varchar(36) NOT NULL, "effect_key" varchar(96) NOT NULL, "environment_id" integer NOT NULL, "producer_id" varchar(36) NOT NULL, CONSTRAINT "fk_rails_b29fa34456"
FOREIGN KEY ("environment_id")
  REFERENCES "environments" ("id")
);
CREATE UNIQUE INDEX "index_ingest_followup_receipts_on_effect" ON "ingest_followup_receipts" ("environment_id", "producer_id", "delivery_id", "effect_key");
CREATE INDEX "index_ingest_followup_receipts_on_environment_and_delivery_time" ON "ingest_followup_receipts" ("environment_id", "delivery_created_at");
CREATE INDEX "index_ingest_followup_receipts_on_environment_id" ON "ingest_followup_receipts" ("environment_id");
