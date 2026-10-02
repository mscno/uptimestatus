CREATE TABLE "api_tokens" (
    "id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    "name" TEXT NOT NULL,
    "token_hash" TEXT NOT NULL,
    "scope" TEXT NOT NULL,
    "created_by" TEXT NOT NULL,
    "created_at" TEXT NOT NULL,
    "last_used_at" TEXT
);
-- #[toasty::breakpoint]
CREATE UNIQUE INDEX "index_api_tokens_by_token_hash" ON "api_tokens" ("token_hash");
