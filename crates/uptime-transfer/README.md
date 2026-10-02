# Full database transfers

`uptime-transfer` exports all 19 application tables as a private, streaming
JSONL snapshot with per-table SHA-256 checksums and a final seal. It verifies
schema and values before creating a target and preserves IDs, nulls, dates,
UTC timestamp precision, booleans and JSON semantics. PostgreSQL extension-owned
tables and each backend's own migration/sequence bookkeeping are excluded.

Snapshot rows contain credentials and private configuration. Keep artifacts
outside Git, encrypt off-volume backups, and never print the row contents.
All local Turso commands require the application writer to be stopped; the
tool does not enable experimental multiprocess access.

```sh
# Source credentials come only from the environment (not command arguments).
uptime-transfer export-postgres --output /protected/source.jsonl
uptime-transfer validate --input /protected/source.jsonl
uptime-transfer import-turso --input /protected/source.jsonl \
  --database /data/db/uptimestatus.db --reset-claims

# Offline backup and rollback after the local database has received writes.
uptime-transfer export-turso --database /data/db/uptimestatus.db \
  --output /protected/local.jsonl
uptime-transfer import-postgres --input /protected/local.jsonl --replace
```

`DATABASE_URL` must point to PostgreSQL for the PostgreSQL commands. Connections
reuse the application's certificate-verifying TLS connector. Exports use a
read-only repeatable-read snapshot. Imports into PostgreSQL validate the input
first, lock application tables and replace rows in one transaction; failure
rolls back all application rows. Sequence allocation is reseeded and validated
for both engines. Local restores refuse an existing destination and only write
`<database>.verified.json` after checksums, checkpoint and reopen checks pass.
An interrupted local import has no completion marker and must be discarded
before retrying. `--reset-claims` clears abandoned scheduler claims after all
other transferred values have been verified.

PostgreSQL stores microseconds, while local Turso can store nanoseconds. A
reverse transfer rejects finer timestamps unless
`--truncate-submicroseconds` is supplied. This truncates only that finer
precision, reports the number of affected values, and validates every target
checksum against the explicitly normalized input. No rows are skipped.

`UPTIMESTATUS_DATABASE__REQUIRE_EXISTING=true` makes application startup refuse
a missing/empty local Turso file or missing/incompatible completion report.
Use `serve --migrate` on the Machine with its volume mounted. A separate
volume-less release command cannot migrate the persistent file.
