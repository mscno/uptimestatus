-- Hand-written: an incident owns its updates and monitor links. Automatic
-- incidents keep their history when the monitor is deleted.
ALTER TABLE "incident_updates"
    ADD CONSTRAINT "incident_updates_incident_fk"
    FOREIGN KEY ("incident_id") REFERENCES "incidents" ("id") ON DELETE CASCADE;

ALTER TABLE "incident_monitors"
    ADD CONSTRAINT "incident_monitors_incident_fk"
    FOREIGN KEY ("incident_id") REFERENCES "incidents" ("id") ON DELETE CASCADE;

ALTER TABLE "incident_monitors"
    ADD CONSTRAINT "incident_monitors_monitor_fk"
    FOREIGN KEY ("monitor_id") REFERENCES "monitors" ("id") ON DELETE CASCADE;

ALTER TABLE "incidents"
    ADD CONSTRAINT "incidents_monitor_fk"
    FOREIGN KEY ("monitor_id") REFERENCES "monitors" ("id") ON DELETE SET NULL;

-- At most one open automatic incident per monitor.
CREATE UNIQUE INDEX "incidents_one_open_auto_per_monitor"
    ON "incidents" ("monitor_id")
    WHERE "kind" = 'auto' AND "resolved_at" IS NULL;
