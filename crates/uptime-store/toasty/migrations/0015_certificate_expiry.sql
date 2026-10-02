ALTER TABLE "monitor_runtime" ADD COLUMN "cert_expires_at" TIMESTAMPTZ(6);
ALTER TABLE "monitor_runtime" ADD COLUMN "cert_warned_days" INTEGER;
