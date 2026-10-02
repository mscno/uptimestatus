-- Hand-written: links and queued deliveries go away with their channel or
-- monitor; the claim query scans only undelivered rows.
ALTER TABLE "monitor_channels"
    ADD CONSTRAINT "monitor_channels_monitor_fk"
    FOREIGN KEY ("monitor_id") REFERENCES "monitors" ("id") ON DELETE CASCADE;

ALTER TABLE "monitor_channels"
    ADD CONSTRAINT "monitor_channels_channel_fk"
    FOREIGN KEY ("channel_id") REFERENCES "notification_channels" ("id") ON DELETE CASCADE;

ALTER TABLE "notification_outbox"
    ADD CONSTRAINT "notification_outbox_channel_fk"
    FOREIGN KEY ("channel_id") REFERENCES "notification_channels" ("id") ON DELETE CASCADE;

ALTER TABLE "notification_outbox"
    ADD CONSTRAINT "notification_outbox_monitor_fk"
    FOREIGN KEY ("monitor_id") REFERENCES "monitors" ("id") ON DELETE SET NULL;

CREATE INDEX "notification_outbox_due"
    ON "notification_outbox" ("next_attempt_at", "id")
    WHERE "sent_at" IS NULL AND "failed_at" IS NULL;
