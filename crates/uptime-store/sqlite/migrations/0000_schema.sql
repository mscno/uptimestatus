CREATE TABLE "incident_updates" (
    "id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    "incident_id" BIGINT NOT NULL,
    "status" TEXT NOT NULL,
    "body" TEXT NOT NULL,
    "created_at" TEXT NOT NULL
);
-- #[toasty::breakpoint]
CREATE INDEX "index_incident_updates_by_incident_id" ON "incident_updates" ("incident_id");
-- #[toasty::breakpoint]
CREATE TABLE "notification_outbox" (
    "id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    "channel_id" BIGINT NOT NULL,
    "monitor_id" BIGINT,
    "event" TEXT NOT NULL,
    "payload" TEXT NOT NULL,
    "attempts" INTEGER NOT NULL,
    "next_attempt_at" TEXT NOT NULL,
    "created_at" TEXT NOT NULL,
    "sent_at" TEXT,
    "failed_at" TEXT,
    "last_error" TEXT
);
-- #[toasty::breakpoint]
CREATE INDEX "index_notification_outbox_by_channel_id" ON "notification_outbox" ("channel_id");
-- #[toasty::breakpoint]
CREATE INDEX "index_notification_outbox_by_next_attempt_at" ON "notification_outbox" ("next_attempt_at");
-- #[toasty::breakpoint]
CREATE TABLE "custom_domains" (
    "id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    "page_id" BIGINT NOT NULL,
    "hostname" TEXT NOT NULL,
    "status" TEXT NOT NULL,
    "last_error" TEXT,
    "verified_at" TEXT,
    "cert_ok_at" TEXT,
    "last_checked_at" TEXT,
    "created_at" TEXT NOT NULL
);
-- #[toasty::breakpoint]
CREATE INDEX "index_custom_domains_by_page_id" ON "custom_domains" ("page_id");
-- #[toasty::breakpoint]
CREATE UNIQUE INDEX "index_custom_domains_by_hostname" ON "custom_domains" ("hostname");
-- #[toasty::breakpoint]
CREATE TABLE "sessions" (
    "token_hash" TEXT NOT NULL,
    "user_id" BIGINT NOT NULL,
    "created_at" TEXT NOT NULL,
    "expires_at" TEXT NOT NULL,
    "last_seen_at" TEXT NOT NULL,
    "user_agent" TEXT,
    PRIMARY KEY ("token_hash")
);
-- #[toasty::breakpoint]
CREATE INDEX "index_sessions_by_user_id" ON "sessions" ("user_id");
-- #[toasty::breakpoint]
CREATE INDEX "index_sessions_by_expires_at" ON "sessions" ("expires_at");
-- #[toasty::breakpoint]
CREATE TABLE "monitor_runtime" (
    "monitor_id" BIGINT NOT NULL,
    "state" TEXT NOT NULL,
    "consecutive_failures" INTEGER NOT NULL,
    "next_run_at" TEXT NOT NULL,
    "scheduled_for" TEXT,
    "claimed_by" TEXT,
    "last_checked_at" TEXT,
    "last_latency_ms" BIGINT,
    "last_status_code" INTEGER,
    "last_error" TEXT,
    "state_changed_at" TEXT,
    "last_push_at" TEXT,
    "last_push_up" BOOLEAN,
    "last_push_message" TEXT,
    "last_push_ms" BIGINT,
    "cert_expires_at" TEXT,
    "cert_warned_days" INTEGER,
    PRIMARY KEY ("monitor_id")
);
-- #[toasty::breakpoint]
CREATE INDEX "index_monitor_runtime_by_next_run_at" ON "monitor_runtime" ("next_run_at");
-- #[toasty::breakpoint]
CREATE TABLE "monitors" (
    "id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    "key" TEXT NOT NULL,
    "name" TEXT NOT NULL,
    "check" TEXT NOT NULL,
    "interval_ms" BIGINT NOT NULL,
    "retry_interval_ms" BIGINT NOT NULL,
    "timeout_ms" BIGINT NOT NULL,
    "retries" INTEGER NOT NULL,
    "invert" BOOLEAN NOT NULL,
    "degraded_after_ms" BIGINT,
    "resend_every" INTEGER NOT NULL,
    "active" BOOLEAN NOT NULL,
    "created_at" TEXT NOT NULL,
    "updated_at" TEXT NOT NULL
);
-- #[toasty::breakpoint]
CREATE UNIQUE INDEX "index_monitors_by_key" ON "monitors" ("key");
-- #[toasty::breakpoint]
CREATE TABLE "page_components" (
    "id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    "section_id" BIGINT NOT NULL,
    "monitor_id" BIGINT NOT NULL,
    "label" TEXT,
    "position" INTEGER NOT NULL
);
-- #[toasty::breakpoint]
CREATE INDEX "index_page_components_by_section_id" ON "page_components" ("section_id");
-- #[toasty::breakpoint]
CREATE INDEX "index_page_components_by_monitor_id" ON "page_components" ("monitor_id");
-- #[toasty::breakpoint]
CREATE TABLE "incident_monitors" (
    "incident_id" BIGINT NOT NULL,
    "monitor_id" BIGINT NOT NULL,
    PRIMARY KEY ("incident_id", "monitor_id")
);
-- #[toasty::breakpoint]
CREATE INDEX "index_incident_monitors_by_monitor_id" ON "incident_monitors" ("monitor_id");
-- #[toasty::breakpoint]
CREATE TABLE "maintenances" (
    "id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    "title" TEXT NOT NULL,
    "description" TEXT,
    "starts_at" TEXT NOT NULL,
    "ends_at" TEXT NOT NULL,
    "created_at" TEXT NOT NULL
);
-- #[toasty::breakpoint]
CREATE INDEX "index_maintenances_by_starts_at" ON "maintenances" ("starts_at");
-- #[toasty::breakpoint]
CREATE INDEX "index_maintenances_by_ends_at" ON "maintenances" ("ends_at");
-- #[toasty::breakpoint]
CREATE TABLE "admin_users" (
    "id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    "github_id" BIGINT NOT NULL,
    "login" TEXT NOT NULL,
    "name" TEXT,
    "avatar_url" TEXT,
    "created_at" TEXT NOT NULL,
    "last_login_at" TEXT NOT NULL
);
-- #[toasty::breakpoint]
CREATE UNIQUE INDEX "index_admin_users_by_github_id" ON "admin_users" ("github_id");
-- #[toasty::breakpoint]
CREATE UNIQUE INDEX "index_admin_users_by_login" ON "admin_users" ("login");
-- #[toasty::breakpoint]
CREATE TABLE "check_results" (
    "id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    "monitor_id" BIGINT NOT NULL,
    "scheduled_for" TEXT NOT NULL,
    "checked_at" TEXT NOT NULL,
    "health" TEXT NOT NULL,
    "state_after" TEXT NOT NULL,
    "latency_ms" BIGINT,
    "status_code" INTEGER,
    "error_kind" TEXT,
    "error" TEXT,
    "region" TEXT NOT NULL
);
-- #[toasty::breakpoint]
CREATE INDEX "index_check_results_by_checked_at" ON "check_results" ("checked_at");
-- #[toasty::breakpoint]
CREATE TABLE "monitor_channels" (
    "monitor_id" BIGINT NOT NULL,
    "channel_id" BIGINT NOT NULL,
    PRIMARY KEY ("monitor_id", "channel_id")
);
-- #[toasty::breakpoint]
CREATE INDEX "index_monitor_channels_by_channel_id" ON "monitor_channels" ("channel_id");
-- #[toasty::breakpoint]
CREATE TABLE "maintenance_monitors" (
    "maintenance_id" BIGINT NOT NULL,
    "monitor_id" BIGINT NOT NULL,
    PRIMARY KEY ("maintenance_id", "monitor_id")
);
-- #[toasty::breakpoint]
CREATE INDEX "index_maintenance_monitors_by_monitor_id" ON "maintenance_monitors" ("monitor_id");
-- #[toasty::breakpoint]
CREATE TABLE "page_sections" (
    "id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    "page_id" BIGINT NOT NULL,
    "name" TEXT NOT NULL,
    "position" INTEGER NOT NULL
);
-- #[toasty::breakpoint]
CREATE INDEX "index_page_sections_by_page_id" ON "page_sections" ("page_id");
-- #[toasty::breakpoint]
CREATE TABLE "notification_channels" (
    "id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    "name" TEXT NOT NULL,
    "kind" TEXT NOT NULL,
    "url" TEXT NOT NULL,
    "secret" TEXT,
    "default_on" BOOLEAN NOT NULL,
    "created_at" TEXT NOT NULL
);
-- #[toasty::breakpoint]
CREATE TABLE "status_pages" (
    "id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    "slug" TEXT NOT NULL,
    "title" TEXT NOT NULL,
    "description" TEXT,
    "accent" TEXT,
    "theme" TEXT NOT NULL,
    "published" BOOLEAN NOT NULL,
    "website_url" TEXT,
    "website_label" TEXT,
    "logo" TEXT,
    "favicon" TEXT,
    "created_at" TEXT NOT NULL,
    "updated_at" TEXT NOT NULL
);
-- #[toasty::breakpoint]
CREATE UNIQUE INDEX "index_status_pages_by_slug" ON "status_pages" ("slug");
-- #[toasty::breakpoint]
CREATE TABLE "incidents" (
    "id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    "title" TEXT NOT NULL,
    "impact" TEXT NOT NULL,
    "status" TEXT NOT NULL,
    "kind" TEXT NOT NULL,
    "monitor_id" BIGINT,
    "started_at" TEXT NOT NULL,
    "resolved_at" TEXT
);
-- #[toasty::breakpoint]
CREATE INDEX "index_incidents_by_monitor_id" ON "incidents" ("monitor_id");
-- #[toasty::breakpoint]
CREATE INDEX "index_incidents_by_started_at" ON "incidents" ("started_at");
-- #[toasty::breakpoint]
CREATE TABLE "monitor_daily" (
    "monitor_id" BIGINT NOT NULL,
    "day" TEXT NOT NULL,
    "total" BIGINT NOT NULL,
    "up" BIGINT NOT NULL,
    "degraded" BIGINT NOT NULL,
    "pending" BIGINT NOT NULL,
    "down" BIGINT NOT NULL,
    "maintenance" BIGINT NOT NULL,
    "latency_sum_ms" BIGINT NOT NULL,
    "latency_count" BIGINT NOT NULL,
    "latency_max_ms" BIGINT NOT NULL,
    PRIMARY KEY ("monitor_id", "day")
);
