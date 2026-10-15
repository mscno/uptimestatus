CREATE TABLE "audit_events" (
    "id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    "at" TEXT NOT NULL,
    "actor" TEXT NOT NULL,
    "action" TEXT NOT NULL,
    "target_kind" TEXT,
    "target_id" TEXT,
    "summary" TEXT NOT NULL,
    "details" TEXT
);
-- #[toasty::breakpoint]
CREATE INDEX "index_audit_events_by_actor" ON "audit_events" ("actor");
-- #[toasty::breakpoint]
CREATE INDEX "index_audit_events_by_action" ON "audit_events" ("action");
-- #[toasty::breakpoint]
CREATE INDEX "index_audit_events_by_target_id" ON "audit_events" ("target_id");
