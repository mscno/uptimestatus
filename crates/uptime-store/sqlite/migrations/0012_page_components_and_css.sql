ALTER TABLE "status_pages" ADD COLUMN "custom_css" TEXT;
-- #[toasty::breakpoint]
CREATE TABLE "page_manual_components" (
    "id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    "section_id" BIGINT NOT NULL,
    "key" TEXT NOT NULL,
    "label" TEXT,
    "description" TEXT,
    "position" INTEGER NOT NULL
);
-- #[toasty::breakpoint]
CREATE INDEX "index_page_manual_components_by_section_id" ON "page_manual_components" ("section_id");
-- #[toasty::breakpoint]
CREATE TABLE "component_status_events" (
    "id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    "page_id" BIGINT NOT NULL,
    "component_key" TEXT NOT NULL,
    "at" TEXT NOT NULL,
    "status" TEXT,
    "previous" TEXT,
    "note" TEXT,
    "actor" TEXT NOT NULL
);
-- #[toasty::breakpoint]
CREATE INDEX "index_component_status_events_by_page_id" ON "component_status_events" ("page_id");
-- #[toasty::breakpoint]
-- Hand-written: a section owns its manual components and a page owns its
-- component status history (SQLite has no ALTER TABLE ... ADD CONSTRAINT).
CREATE TRIGGER "page_sections_delete_manual_components" AFTER DELETE ON "page_sections" BEGIN
    DELETE FROM "page_manual_components" WHERE "section_id" = OLD."id";
END;
-- #[toasty::breakpoint]
CREATE TRIGGER "status_pages_delete_component_history" AFTER DELETE ON "status_pages" BEGIN
    DELETE FROM "page_manual_components" WHERE "section_id" IN
        (SELECT "id" FROM "page_sections" WHERE "page_id" = OLD."id");
    DELETE FROM "component_status_events" WHERE "page_id" = OLD."id";
END;
-- #[toasty::breakpoint]
CREATE TRIGGER "page_manual_components_parent" BEFORE INSERT ON "page_manual_components" BEGIN
    SELECT RAISE(ABORT, 'unknown page section') WHERE NOT EXISTS
        (SELECT 1 FROM "page_sections" WHERE "id" = NEW."section_id");
END;
-- #[toasty::breakpoint]
CREATE TRIGGER "component_status_events_parent" BEFORE INSERT ON "component_status_events" BEGIN
    SELECT RAISE(ABORT, 'unknown status page') WHERE NOT EXISTS
        (SELECT 1 FROM "status_pages" WHERE "id" = NEW."page_id");
END;
-- #[toasty::breakpoint]
CREATE INDEX "index_component_status_events_by_component_and_time"
    ON "component_status_events" ("page_id", "component_key", "at");
