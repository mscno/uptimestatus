CREATE TABLE "page_email_settings" (
    "page_id" BIGINT NOT NULL,
    "subscriptions" BOOLEAN NOT NULL,
    "notify_automatic" BOOLEAN NOT NULL,
    "sender_name" TEXT,
    "sender_address" TEXT,
    "reply_to" TEXT,
    "domain_id" TEXT,
    "domain_name" TEXT,
    "domain_status" TEXT,
    "domain_records" TEXT,
    "domain_checked_at" TEXT,
    PRIMARY KEY ("page_id")
);
-- #[toasty::breakpoint]
CREATE TABLE "subscribers" (
    "id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    "page_id" BIGINT NOT NULL,
    "kind" TEXT NOT NULL,
    "email" TEXT NOT NULL,
    "email_index" TEXT NOT NULL,
    "webhook_url" TEXT,
    "url_index" TEXT,
    "webhook_secret" TEXT,
    "secret_shown" BOOLEAN NOT NULL,
    "components" TEXT,
    "status" TEXT NOT NULL,
    "link_salt" TEXT NOT NULL,
    "created_at" TEXT NOT NULL,
    "confirmed_at" TEXT,
    "ended_at" TEXT,
    "failures" INTEGER NOT NULL,
    "failing_since" TEXT,
    "last_delivery_at" TEXT,
    "last_error" TEXT
);
-- #[toasty::breakpoint]
CREATE INDEX "index_subscribers_by_page_id" ON "subscribers" ("page_id");
-- #[toasty::breakpoint]
CREATE INDEX "index_subscribers_by_email_index" ON "subscribers" ("email_index");
-- #[toasty::breakpoint]
CREATE INDEX "index_subscribers_by_url_index" ON "subscribers" ("url_index");
-- #[toasty::breakpoint]
CREATE TABLE "subscriber_outbox" (
    "id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    "subscriber_id" BIGINT NOT NULL,
    "kind" TEXT NOT NULL,
    "event_key" TEXT NOT NULL,
    "payload" TEXT,
    "attempts" INTEGER NOT NULL,
    "next_attempt_at" TEXT NOT NULL,
    "created_at" TEXT NOT NULL,
    "sent_at" TEXT,
    "failed_at" TEXT,
    "last_error" TEXT
);
-- #[toasty::breakpoint]
CREATE INDEX "index_subscriber_outbox_by_subscriber_id" ON "subscriber_outbox" ("subscriber_id");
-- #[toasty::breakpoint]
CREATE INDEX "index_subscriber_outbox_by_next_attempt_at" ON "subscriber_outbox" ("next_attempt_at");
-- #[toasty::breakpoint]
-- Hand-written (see 0001_integrity): email settings and subscribers go with
-- their page, queued messages with their subscriber.
CREATE TRIGGER "status_pages_delete_subscribers" AFTER DELETE ON "status_pages" BEGIN
    DELETE FROM "subscriber_outbox" WHERE "subscriber_id" IN
        (SELECT "id" FROM "subscribers" WHERE "page_id" = OLD."id");
    DELETE FROM "subscribers" WHERE "page_id" = OLD."id";
    DELETE FROM "page_email_settings" WHERE "page_id" = OLD."id";
END;
-- #[toasty::breakpoint]
CREATE TRIGGER "subscribers_delete_children" AFTER DELETE ON "subscribers" BEGIN
    DELETE FROM "subscriber_outbox" WHERE "subscriber_id" = OLD."id";
END;
-- #[toasty::breakpoint]
CREATE TRIGGER "subscribers_parent" BEFORE INSERT ON "subscribers" BEGIN
    SELECT RAISE(ABORT, 'unknown page') WHERE NOT EXISTS
        (SELECT 1 FROM "status_pages" WHERE "id" = NEW."page_id");
END;
-- #[toasty::breakpoint]
CREATE TRIGGER "page_email_settings_parent" BEFORE INSERT ON "page_email_settings" BEGIN
    SELECT RAISE(ABORT, 'unknown page') WHERE NOT EXISTS
        (SELECT 1 FROM "status_pages" WHERE "id" = NEW."page_id");
END;
-- #[toasty::breakpoint]
CREATE TRIGGER "subscriber_outbox_parent" BEFORE INSERT ON "subscriber_outbox" BEGIN
    SELECT RAISE(ABORT, 'unknown subscriber') WHERE NOT EXISTS
        (SELECT 1 FROM "subscribers" WHERE "id" = NEW."subscriber_id");
END;
-- #[toasty::breakpoint]
CREATE INDEX "subscriber_outbox_due" ON "subscriber_outbox" ("next_attempt_at", "id")
    WHERE "sent_at" IS NULL AND "failed_at" IS NULL;
-- #[toasty::breakpoint]
CREATE INDEX "subscriber_outbox_by_event" ON "subscriber_outbox" ("subscriber_id", "event_key");
