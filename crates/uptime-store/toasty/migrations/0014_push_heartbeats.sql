ALTER TABLE "monitor_runtime" ADD COLUMN "last_push_up" BOOLEAN;
ALTER TABLE "monitor_runtime" ADD COLUMN "last_push_at" TIMESTAMPTZ(6);
ALTER TABLE "monitor_runtime" ADD COLUMN "last_push_ms" BIGINT;
ALTER TABLE "monitor_runtime" ADD COLUMN "last_push_message" TEXT;
