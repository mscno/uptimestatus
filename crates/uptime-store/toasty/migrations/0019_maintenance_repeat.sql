ALTER TABLE "maintenances" ADD COLUMN "repeat_until" TIMESTAMPTZ(6);
ALTER TABLE "maintenances" ADD COLUMN "repeat" TEXT;
