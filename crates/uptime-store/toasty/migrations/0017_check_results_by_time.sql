-- Hand-written: the janitor deletes results by age across all monitors; with
-- 90 days of history that must not scan the table.
CREATE INDEX "check_results_checked_at" ON "check_results" ("checked_at");
