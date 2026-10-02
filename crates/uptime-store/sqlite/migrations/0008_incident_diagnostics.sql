ALTER TABLE "incident_updates" ADD COLUMN "check_json" TEXT;
-- #[toasty::breakpoint]
ALTER TABLE "check_results" ADD COLUMN "response_body" TEXT;
