-- Hand-written: a window owns its monitor links; deleting a monitor removes it
-- from every window.
ALTER TABLE "maintenance_monitors"
    ADD CONSTRAINT "maintenance_monitors_maintenance_fk"
    FOREIGN KEY ("maintenance_id") REFERENCES "maintenances" ("id") ON DELETE CASCADE;

ALTER TABLE "maintenance_monitors"
    ADD CONSTRAINT "maintenance_monitors_monitor_fk"
    FOREIGN KEY ("monitor_id") REFERENCES "monitors" ("id") ON DELETE CASCADE;
