-- Toasty's SQLite schema generator does not yet express foreign keys. These
-- triggers preserve the delete behavior of the PostgreSQL migrations. Each
-- trigger handles descendants directly, without recursive_triggers.
CREATE TRIGGER "monitors_delete_children" AFTER DELETE ON "monitors" BEGIN
    DELETE FROM "monitor_runtime" WHERE "monitor_id" = OLD."id";
    DELETE FROM "check_results" WHERE "monitor_id" = OLD."id";
    DELETE FROM "monitor_daily" WHERE "monitor_id" = OLD."id";
    DELETE FROM "page_components" WHERE "monitor_id" = OLD."id";
    DELETE FROM "monitor_channels" WHERE "monitor_id" = OLD."id";
    DELETE FROM "maintenance_monitors" WHERE "monitor_id" = OLD."id";
    DELETE FROM "incident_monitors" WHERE "monitor_id" = OLD."id";
    UPDATE "incidents" SET "monitor_id" = NULL WHERE "monitor_id" = OLD."id";
    UPDATE "notification_outbox" SET "monitor_id" = NULL WHERE "monitor_id" = OLD."id";
END;
-- #[toasty::breakpoint]
CREATE TRIGGER "status_pages_delete_children" AFTER DELETE ON "status_pages" BEGIN
    DELETE FROM "page_components" WHERE "section_id" IN
        (SELECT "id" FROM "page_sections" WHERE "page_id" = OLD."id");
    DELETE FROM "page_sections" WHERE "page_id" = OLD."id";
    DELETE FROM "custom_domains" WHERE "page_id" = OLD."id";
END;
-- #[toasty::breakpoint]
CREATE TRIGGER "page_sections_delete_children" AFTER DELETE ON "page_sections" BEGIN
    DELETE FROM "page_components" WHERE "section_id" = OLD."id";
END;
-- #[toasty::breakpoint]
CREATE TRIGGER "notification_channels_delete_children" AFTER DELETE ON "notification_channels" BEGIN
    DELETE FROM "monitor_channels" WHERE "channel_id" = OLD."id";
    DELETE FROM "notification_outbox" WHERE "channel_id" = OLD."id";
END;
-- #[toasty::breakpoint]
CREATE TRIGGER "maintenances_delete_children" AFTER DELETE ON "maintenances" BEGIN
    DELETE FROM "maintenance_monitors" WHERE "maintenance_id" = OLD."id";
END;
-- #[toasty::breakpoint]
CREATE TRIGGER "incidents_delete_children" AFTER DELETE ON "incidents" BEGIN
    DELETE FROM "incident_updates" WHERE "incident_id" = OLD."id";
    DELETE FROM "incident_monitors" WHERE "incident_id" = OLD."id";
END;
-- #[toasty::breakpoint]
CREATE TRIGGER "admin_users_delete_children" AFTER DELETE ON "admin_users" BEGIN
    DELETE FROM "sessions" WHERE "user_id" = OLD."id";
END;
-- #[toasty::breakpoint]
CREATE INDEX "check_results_by_monitor_recent" ON "check_results" ("monitor_id", "checked_at" DESC);
-- #[toasty::breakpoint]
CREATE INDEX "notification_outbox_due" ON "notification_outbox" ("next_attempt_at", "id")
    WHERE "sent_at" IS NULL AND "failed_at" IS NULL;
-- #[toasty::breakpoint]
CREATE UNIQUE INDEX "incidents_one_open_auto_per_monitor" ON "incidents" ("monitor_id")
    WHERE "kind" = 'auto' AND "resolved_at" IS NULL;
-- #[toasty::breakpoint]
-- SQLite and Turso do not enable foreign_keys on every Toasty pool connection.
-- Reject orphan inserts here; store updates keep these keys immutable.
CREATE TRIGGER "monitor_runtime_parent" BEFORE INSERT ON "monitor_runtime" BEGIN
    SELECT RAISE(ABORT, 'unknown monitor') WHERE NOT EXISTS
        (SELECT 1 FROM "monitors" WHERE "id" = NEW."monitor_id");
END;
-- #[toasty::breakpoint]
CREATE TRIGGER "check_results_parent" BEFORE INSERT ON "check_results" BEGIN
    SELECT RAISE(ABORT, 'unknown monitor') WHERE NOT EXISTS
        (SELECT 1 FROM "monitors" WHERE "id" = NEW."monitor_id");
END;
-- #[toasty::breakpoint]
CREATE TRIGGER "monitor_daily_parent" BEFORE INSERT ON "monitor_daily" BEGIN
    SELECT RAISE(ABORT, 'unknown monitor') WHERE NOT EXISTS
        (SELECT 1 FROM "monitors" WHERE "id" = NEW."monitor_id");
END;
-- #[toasty::breakpoint]
CREATE TRIGGER "sessions_parent" BEFORE INSERT ON "sessions" BEGIN
    SELECT RAISE(ABORT, 'unknown admin user') WHERE NOT EXISTS
        (SELECT 1 FROM "admin_users" WHERE "id" = NEW."user_id");
END;
-- #[toasty::breakpoint]
CREATE TRIGGER "page_sections_parent" BEFORE INSERT ON "page_sections" BEGIN
    SELECT RAISE(ABORT, 'unknown status page') WHERE NOT EXISTS
        (SELECT 1 FROM "status_pages" WHERE "id" = NEW."page_id");
END;
-- #[toasty::breakpoint]
CREATE TRIGGER "page_components_parent" BEFORE INSERT ON "page_components" BEGIN
    SELECT RAISE(ABORT, 'unknown page section') WHERE NOT EXISTS
        (SELECT 1 FROM "page_sections" WHERE "id" = NEW."section_id");
    SELECT RAISE(ABORT, 'unknown monitor') WHERE NOT EXISTS
        (SELECT 1 FROM "monitors" WHERE "id" = NEW."monitor_id");
END;
-- #[toasty::breakpoint]
CREATE TRIGGER "custom_domains_parent" BEFORE INSERT ON "custom_domains" BEGIN
    SELECT RAISE(ABORT, 'unknown status page') WHERE NOT EXISTS
        (SELECT 1 FROM "status_pages" WHERE "id" = NEW."page_id");
END;
-- #[toasty::breakpoint]
CREATE TRIGGER "monitor_channels_parent" BEFORE INSERT ON "monitor_channels" BEGIN
    SELECT RAISE(ABORT, 'unknown monitor') WHERE NOT EXISTS
        (SELECT 1 FROM "monitors" WHERE "id" = NEW."monitor_id");
    SELECT RAISE(ABORT, 'unknown notification channel') WHERE NOT EXISTS
        (SELECT 1 FROM "notification_channels" WHERE "id" = NEW."channel_id");
END;
-- #[toasty::breakpoint]
CREATE TRIGGER "notification_outbox_parent" BEFORE INSERT ON "notification_outbox" BEGIN
    SELECT RAISE(ABORT, 'unknown notification channel') WHERE NOT EXISTS
        (SELECT 1 FROM "notification_channels" WHERE "id" = NEW."channel_id");
    SELECT RAISE(ABORT, 'unknown monitor') WHERE NEW."monitor_id" IS NOT NULL AND NOT EXISTS
        (SELECT 1 FROM "monitors" WHERE "id" = NEW."monitor_id");
END;
-- #[toasty::breakpoint]
CREATE TRIGGER "maintenance_monitors_parent" BEFORE INSERT ON "maintenance_monitors" BEGIN
    SELECT RAISE(ABORT, 'unknown maintenance') WHERE NOT EXISTS
        (SELECT 1 FROM "maintenances" WHERE "id" = NEW."maintenance_id");
    SELECT RAISE(ABORT, 'unknown monitor') WHERE NOT EXISTS
        (SELECT 1 FROM "monitors" WHERE "id" = NEW."monitor_id");
END;
-- #[toasty::breakpoint]
CREATE TRIGGER "incident_updates_parent" BEFORE INSERT ON "incident_updates" BEGIN
    SELECT RAISE(ABORT, 'unknown incident') WHERE NOT EXISTS
        (SELECT 1 FROM "incidents" WHERE "id" = NEW."incident_id");
END;
-- #[toasty::breakpoint]
CREATE TRIGGER "incident_monitors_parent" BEFORE INSERT ON "incident_monitors" BEGIN
    SELECT RAISE(ABORT, 'unknown incident') WHERE NOT EXISTS
        (SELECT 1 FROM "incidents" WHERE "id" = NEW."incident_id");
    SELECT RAISE(ABORT, 'unknown monitor') WHERE NOT EXISTS
        (SELECT 1 FROM "monitors" WHERE "id" = NEW."monitor_id");
END;
-- #[toasty::breakpoint]
CREATE TRIGGER "incidents_parent" BEFORE INSERT ON "incidents" BEGIN
    SELECT RAISE(ABORT, 'unknown monitor') WHERE NEW."monitor_id" IS NOT NULL AND NOT EXISTS
        (SELECT 1 FROM "monitors" WHERE "id" = NEW."monitor_id");
END;
