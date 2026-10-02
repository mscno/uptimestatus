-- Toasty's JSON wrapper needs one column type for every runtime driver.
-- Text-backed JSON works on PostgreSQL, SQLite and Turso; the push-token
-- lookup casts check to jsonb when running on PostgreSQL.
ALTER TABLE "monitors" ALTER COLUMN "check" TYPE TEXT USING "check"::text;
ALTER TABLE "notification_outbox" ALTER COLUMN "payload" TYPE TEXT USING "payload"::text;
