ALTER TABLE "status_pages" ADD COLUMN "sla_target" TEXT;
-- #[toasty::breakpoint]
ALTER TABLE "status_pages" ADD COLUMN "reports_private" BOOLEAN;
-- #[toasty::breakpoint]
CREATE TABLE "report_links" (
    "id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    "page_id" BIGINT NOT NULL,
    "month" TEXT,
    "token_hash" TEXT NOT NULL,
    "label" TEXT,
    "expires_at" TEXT,
    "created_by" TEXT NOT NULL,
    "created_at" TEXT NOT NULL
);
-- #[toasty::breakpoint]
CREATE INDEX "index_report_links_by_page_id" ON "report_links" ("page_id");
-- #[toasty::breakpoint]
CREATE UNIQUE INDEX "index_report_links_by_token_hash" ON "report_links" ("token_hash");
-- #[toasty::breakpoint]
CREATE TABLE "scheduled_incident_updates" (
    "id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    "incident_id" BIGINT,
    "publish_at" TEXT NOT NULL,
    "status" TEXT NOT NULL,
    "body" TEXT NOT NULL,
    "title" TEXT,
    "impact" TEXT,
    "monitors" TEXT,
    "actor" TEXT NOT NULL,
    "error" TEXT,
    "created_at" TEXT NOT NULL
);
-- #[toasty::breakpoint]
CREATE INDEX "index_scheduled_incident_updates_by_incident_id" ON "scheduled_incident_updates" ("incident_id");
-- #[toasty::breakpoint]
CREATE INDEX "index_scheduled_incident_updates_by_publish_at" ON "scheduled_incident_updates" ("publish_at");
-- #[toasty::breakpoint]
CREATE TABLE "incident_templates" (
    "id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    "name" TEXT NOT NULL,
    "title" TEXT NOT NULL,
    "impact" TEXT NOT NULL,
    "status" TEXT NOT NULL,
    "body" TEXT NOT NULL,
    "created_at" TEXT NOT NULL,
    "updated_at" TEXT NOT NULL
);
-- #[toasty::breakpoint]
CREATE UNIQUE INDEX "index_incident_templates_by_name" ON "incident_templates" ("name");
