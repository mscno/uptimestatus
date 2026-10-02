-- Hand-written: Toasty models don't express foreign keys or composite
-- descending indexes, so they live here. Deleting a monitor removes its
-- runtime row, raw results and daily counters.
ALTER TABLE "monitor_runtime"
    ADD CONSTRAINT "monitor_runtime_monitor_fk"
    FOREIGN KEY ("monitor_id") REFERENCES "monitors" ("id") ON DELETE CASCADE;

ALTER TABLE "check_results"
    ADD CONSTRAINT "check_results_monitor_fk"
    FOREIGN KEY ("monitor_id") REFERENCES "monitors" ("id") ON DELETE CASCADE;

ALTER TABLE "monitor_daily"
    ADD CONSTRAINT "monitor_daily_monitor_fk"
    FOREIGN KEY ("monitor_id") REFERENCES "monitors" ("id") ON DELETE CASCADE;

-- Recent history per monitor, newest first.
CREATE INDEX "check_results_by_monitor_recent"
    ON "check_results" ("monitor_id", "checked_at" DESC);
