ALTER TABLE "monitors" ADD COLUMN "push_token_index" TEXT;
-- #[toasty::breakpoint]
CREATE INDEX "index_monitors_by_push_token_index" ON "monitors" ("push_token_index");
