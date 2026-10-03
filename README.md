# uptimestatus

A self-hosted uptime monitor and status page app, written in Rust. It's an open-source
hobby project for keeping an eye on a small collection of services. Run it as one binary
or a Docker container, with SQLite, PostgreSQL or Turso for storage.

![PrivateNPM status page, using the pixel theme](docs/screenshot.png)

## Features

- HTTP(S), TCP, DNS and push heartbeat monitors, with configurable timeouts and retries.
  HTTP checks support status ranges, keywords, JSON assertions, custom requests and auth;
  TCP checks can use TLS and send/expect rules.
- Automatic incidents after the original check and all configured retries fail, with at least
  two failed attempts. Monitors retry once by default. Checks continue at the retry interval
  while down, returning to the normal interval on recovery. Recovery resolves the incident;
  the admin timeline keeps errors, statuses and response excerpts, including the original
  attempt and retries.
- Public status pages with current and previous incidents, 90-day uptime bars, response-time
  charts, Atom feeds, custom domains, logos and a choice of pixel or clean styling.
- Slack, Discord and signed webhook alerts, with retries, quiet hours and escalation delays.
- TLS certificate-expiry warnings and one-off or recurring maintenance windows.
- A live admin console with GitHub sign-in, tags, groups, search, bulk actions and "Test now".
  Status pages and the console update over SSE when checks finish.
- A [JSON API](#json-api) and TOML import/export for monitors and pages.

## Quick start

You need a GitHub OAuth app for the admin console (Settings → Developer settings → OAuth
apps; callback URL `<APP_URL>/auth/github/callback`, e.g. `http://localhost:8080/auth/github/callback`).

```sh
cp .env.example .env      # set UPTIMESTATUS_AUTH__ADMINS, GITHUB_CLIENT_ID/SECRET, COOKIE_KEY
docker compose up --build # SQLite on a named volume, http://localhost:8080
```

That is a single instance with SQLite: no database server to run. To use PostgreSQL or Turso
instead, change `DATABASE_URL` (see [Databases](#databases)). Without Docker:

```sh
cargo build --release -p uptime-server      # target/release/uptimestatus
DATABASE_URL=sqlite:./uptimestatus.db UPTIMESTATUS_DATABASE__BACKEND=sqlite \
UPTIMESTATUS_APP_URL=http://localhost:8080 UPTIMESTATUS_EDGE_HOST=edge.localhost \
UPTIMESTATUS_AUTH__ADMINS=you UPTIMESTATUS_AUTH__GITHUB_CLIENT_ID=... UPTIMESTATUS_AUTH__GITHUB_CLIENT_SECRET=... \
  target/release/uptimestatus serve --migrate
```

`serve --migrate` applies pending migrations before starting. For upgrades, you can also
run `uptimestatus migrate` separately, then start `serve`.

## JSON API

Create a token under **API** in the console (read-only or read/write; the secret is shown
once, only its hash is stored) and send it as `Authorization: Bearer upt_…`. Read-only tokens
see credentials (auth secrets, credential-looking headers, push tokens) redacted. Errors are
`{"error": "…"}`.

| Method and path | Scope | |
|---|---|---|
| `GET /api/v1/monitors?tag=&group=&q=` | read | Monitors with their current state (`group` matches the group and everything below) |
| `GET`, `PUT`, `DELETE /api/v1/monitors/{key}` | read / write | `PUT` creates or replaces (idempotent); the body is the monitor without `key` |
| `POST /api/v1/monitors/{key}/pause`, `/resume` | write | |
| `GET /api/v1/monitors/{key}/checks?limit=` | read | Recent results, newest first |
| `GET /api/v1/config` | read | All monitors and pages, in the shape `PUT` accepts |
| `GET`, `PUT`, `DELETE /api/v1/pages/{slug}` | read / write | Status pages (`section` layout as in the TOML) |
| `POST /api/v1/incidents` | write | `{"title", "message", "impact", "status", "monitors": ["key"]}` |
| `POST /api/v1/incidents/{id}/updates` | write | `{"status", "message"}` |

```sh
curl -X PUT https://status.example.com/api/v1/monitors/api \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"name": "API", "check": {"type": "http", "url": "https://api.example.com/health"}, "tags": ["prod"]}'
```

## Configuration

Settings come from, later winning:

1. built-in defaults
2. a TOML file: `uptimestatus.toml` in the working directory, or the path in
   `UPTIMESTATUS_CONFIG` (see [`uptimestatus.example.toml`](uptimestatus.example.toml))
3. `UPTIMESTATUS_*` environment variables; nested keys use `__`, so `[http] port` is
   `UPTIMESTATUS_HTTP__PORT`
4. `DATABASE_URL`, the name platforms conventionally inject

Unknown keys are rejected at startup, so typos fail loudly. Keep secrets (`DATABASE_URL`,
the OAuth secret, the cookie key, the Turso token) in the environment or an uncommitted file;
the configuration logger redacts these secrets. [`.env.example`](.env.example) lists the common ones.

| Variable | Default | |
|---|---|---|
| `DATABASE_URL` | – | Database URL (required): `postgres://…`, `sqlite:/data/uptimestatus.db`, or `libsql://…` / `turso://…` |
| `UPTIMESTATUS_DATABASE__BACKEND` | `postgres` | `postgres`, `sqlite` or `turso`; must match the `DATABASE_URL` scheme |
| `UPTIMESTATUS_DATABASE__AUTH_TOKEN` | – | Turso Cloud auth token; keep it out of the URL |
| `UPTIMESTATUS_DATABASE__MAX_CONNECTIONS` | `8` | PostgreSQL pool size; SQLite and Turso use one connection |
| `UPTIMESTATUS_DATABASE__REQUIRE_EXISTING` | `false` | Local Turso deployments can require an existing file and verified import report before connecting |
| `UPTIMESTATUS_APP_URL` | – | Public admin URL; its host is the app host (required) |
| `UPTIMESTATUS_EDGE_HOST` | – | Hostname custom domains CNAME to; requests for it redirect to the console (required) |
| `UPTIMESTATUS_HTTP__HOST` | `::` | Bind address |
| `UPTIMESTATUS_HTTP__PORT` | `8080` | Public listener |
| `UPTIMESTATUS_HTTP__INTERNAL_PORT` | `9090` | `/healthz` and `/readyz`; never expose publicly |
| `UPTIMESTATUS_LOG__FORMAT` | `json` | `json` or `pretty` |
| `UPTIMESTATUS_CHECKS__ENABLED` | `true` | Run the scheduler in this process |
| `UPTIMESTATUS_CHECKS__CONCURRENCY` | `32` | Checks in flight at once |
| `UPTIMESTATUS_CHECKS__POLL_INTERVAL` | `1s` | How often due checks are claimed |
| `UPTIMESTATUS_CHECKS__RETENTION` | `90days` | How long raw check results and finished alert deliveries are kept (pruned hourly; daily counters are kept) |
| `UPTIMESTATUS_CHECKS__HEARTBEAT_URL` | – | Dead-man switch (e.g. healthchecks.io) |
| `UPTIMESTATUS_CHECKS__ALLOW_PRIVATE_TARGETS` | `false` | Let probes reach private addresses (local development only) |
| `UPTIMESTATUS_CHECKS__REGION` | `local` | Region label recorded with results |
| `UPTIMESTATUS_AUTH__ADMINS` | – | GitHub usernames allowed to sign in, comma-separated |
| `UPTIMESTATUS_AUTH__GITHUB_CLIENT_ID` / `_SECRET` | – | GitHub OAuth app (callback `<APP_URL>/auth/github/callback`) |
| `UPTIMESTATUS_AUTH__COOKIE_KEY` | per process | Base64 of ≥ 64 random bytes (`openssl rand -base64 64`); set it so sessions survive restarts |
| `UPTIMESTATUS_AUTH__SESSION_IDLE` / `_MAX_AGE` | `7days` / `30days` | Session lifetimes |
| `UPTIMESTATUS_AUTH__DEV_LOGIN` | – | Debug builds only: sign in as this user without GitHub |
| `UPTIMESTATUS_DOMAINS__VERIFY` | `true` | Verify custom domains in this process |
| `UPTIMESTATUS_DOMAINS__EDGE_IPS` | – | The app's public IPv4/IPv6 addresses, for apex custom domains, comma-separated |
| `UPTIMESTATUS_DOMAINS__VERIFY_EVERY` | `5m` | Retry interval for unverified domains |
| `UPTIMESTATUS_DOMAINS__RECHECK_AFTER` | `24h` | Re-verify verified domains this often |
| `UPTIMESTATUS_DOMAINS__PREWARM_TLS` | `true` | Check verified domains over HTTPS and show whether a certificate is served; turn off if no proxy issues certificates |
| `UPTIMESTATUS_STORAGE__PATH` | – | Writable directory for uploaded logos and favicons; uploads are off when unset |
| `RUST_LOG` | `info` | `tracing` filter directives |

Durations use humantime syntax (`30s`, `5m`, `90days`).

## Databases

| Backend | `DATABASE_URL` | Persistence | Multiple instances |
|---|---|---|---|
| PostgreSQL | `postgres://user:pass@host:5432/db` (add `?sslmode=verify-full&sslrootcert=system` for hosted databases) | the server | yes |
| SQLite | `sqlite:/data/uptimestatus.db` (created if missing; `sqlite::memory:` for tests) | a file: **mount a persistent volume** | no |
| Turso Cloud | `libsql://your-db.turso.io` plus `UPTIMESTATUS_DATABASE__AUTH_TOKEN` | Turso | no |
| Turso, local file | `turso:/data/uptimestatus.db` | a file: mount a volume | no |

```sh
# PostgreSQL (the default backend)
DATABASE_URL=postgres://uptime:secret@db.internal:5432/uptime

# SQLite
UPTIMESTATUS_DATABASE__BACKEND=sqlite
DATABASE_URL=sqlite:/data/uptimestatus.db

# Turso Cloud
UPTIMESTATUS_DATABASE__BACKEND=turso
DATABASE_URL=libsql://your-database.turso.io
UPTIMESTATUS_DATABASE__AUTH_TOKEN=your-token
```

Run `uptimestatus migrate` (or `serve --migrate`) before serving. The image includes all
drivers and its `/data` directory is prepared for the unprivileged user; mount a volume there
for SQLite files and uploaded images. SQLite and Turso run one combined `web,worker` instance;
PostgreSQL is the backend for separate web and worker instances and multi-instance live
updates (it uses `LISTEN/NOTIFY`). The supported backends are PostgreSQL, SQLite and Turso.

A `Turso Cloud integration` CI job runs on pushes to `master` when the repository secrets
`UPTIMESTATUS_TURSO_TEST_URL` and `UPTIMESTATUS_TURSO_TEST_AUTH_TOKEN` are set (it skips
otherwise). For a manual run, export those two variables and run
`cargo test -p uptime-store --test portable remote_turso_backend -- --ignored --exact`.

## Running modes

`uptimestatus [serve [--migrate] [--roles web,worker]]`, `migrate`, `seed <file>`, `export [-o file]`.

| Command | What it does |
|---|---|
| `serve` (default) | Everything in one process: `--roles web,worker` |
| `serve --roles web` | Only HTTP: console, status pages, push API. No checks are run. |
| `serve --roles worker` | Only the scheduler, alert sender, domain verifier and retention janitor (HTTP health endpoints only) |
| `migrate` | Apply pending migrations and exit (release step) |
| `seed file.toml` | Create the monitors and pages in a TOML file; existing keys and slugs are left alone |
| `export` | Write all monitors and pages as TOML, the format `seed` reads |

One `serve` process is enough for a small setup. If you want to separate the web app from
the checks, PostgreSQL supports `--roles web` and `--roles worker` instances sharing a
database; they coordinate over `LISTEN/NOTIFY`. SQLite and Turso use one instance with both roles.
`UPTIMESTATUS_CHECKS__ENABLED=false` and `UPTIMESTATUS_DOMAINS__VERIFY=false` turn off pieces
of the worker role in a process.

Monitors and pages can be created in the console, through the [JSON API](#json-api), or from
TOML (`seed`/`export`): `[[monitor]]` tables with a `check` (`type = "http"`, `"tcp"`, `"dns"`
or `"push"`) and an optional `policy` (`interval`, `retries`, `retry_interval`, `timeout`,
`invert`, `degraded_after`, `resend_every`), and `[[page]]` tables with `[[page.section]]`s. Pages can be styled with `accent` (a hex
color), `theme` (`auto`, `light`, `dark`) and `look` (`pixel`, the 8-bit default, or `clean`:
plain fonts and no starfield); the same options are in the page editor.
See [`seed/monitors.example.toml`](seed/monitors.example.toml).

## Deploying

- **Docker:** `docker build -t uptimestatus .` (or the `ghcr.io` image published for version
  tags by `.github/workflows/release.yml`), then `docker run --env-file .env -p 8080:8080 -v
  uptime-data:/data uptimestatus serve --migrate`. See [`compose.yaml`](compose.yaml).
- **Behind a proxy:** the app speaks plain HTTP and routes by the `Host` header, so put a
  TLS-terminating proxy in front and forward `Host` unchanged. The console lives at
  `UPTIMESTATUS_APP_URL`'s host; each custom status-page domain is a hostname that must reach
  the proxy with a certificate (Caddy's on-demand TLS, your platform's certificates, ...). Publish only
  port 8080; point health checks at `/healthz` on it or on the internal port.
- **Platforms:** run the released image with the environment above. Set
  `UPTIMESTATUS_HTTP__HOST=::` if the platform's proxy connects over IPv6 (the image already does),
  run `migrate` as the release command, and mount a volume at `/data` if you use SQLite or
  uploads.

## Development

Requirements: Rust (pinned in `rust-toolchain.toml`), [mise](https://mise.jdx.dev), `psql`,
and a Postgres 18 server at `localhost:5433` (`postgres:postgres`). Override any value from
`mise.toml`'s `[env]` in a local `.env`. The tests use PostgreSQL (each test gets a fresh
database cloned from a migrated template); SQLite and Turso are covered by the store's
`portable` tests.

```sh
mise install          # cargo-nextest, cargo-llvm-cov
mise run db:reset     # create + migrate the dev database (uptimestatus_dev)
mise run seed         # create the example monitors in seed/monitors.example.toml
mise run dev          # serve on :8080 (public) and :9090 (internal); the scheduler starts checking
mise run test         # full suite
```

Open <http://localhost:8080> and use **Development sign-in** (debug builds only) to reach the
admin console. `mise tasks` lists everything: `db:create`, `db:drop`, `db:migrate`,
`db:reset`, `db:migration:new <name>`, `db:psql`, `db:test:clean`, `seed [file]`, `dev`,
`test`, `test:doc`, `cov`, `fmt`, `lint`, `ci`.

## Workspace

| Crate | Role |
|---|---|
| `uptime-domain` | Pure domain model: check specs, policies, evaluation, the monitor state machine, uptime math. No I/O. |
| `uptime-store` | PostgreSQL, SQLite and Turso via Toasty: models, embedded migrations, leased check queue, result writes, retention. |
| `uptime-probe` | Runs HTTP, TCP and DNS checks and reports raw observations; refuses private/internal addresses (resolved IPs, IP literals, redirects). Also inspects custom domains (DNS, HTTPS). |
| `uptime-notify` | Renders alerts for Slack, Discord and HMAC-signed webhooks, and delivers them. |
| `uptime-runtime` | The scheduler loop (claim → probe → evaluate → record → publish), the alert sender, domain verification, the cluster bus bridge, the dead-man heartbeat and the retention janitor. |
| `uptime-web` | HTTP surface: host routing, internal endpoints (`/healthz`, `/readyz`), the push API, Topcoat pages (console and status pages), vendored Starbase assets. |
| `uptime-server` | The `uptimestatus` binary: config (figment), telemetry (tracing), CLI (clap), process wiring. |
| `uptime-transfer` | Full database export, validation and restore between PostgreSQL and local Turso; see its [README](crates/uptime-transfer/README.md). |
| `uptime-testkit` | Test support: a migrated database per test, cloned from a template. |

## Migrations

Change the models in `crates/uptime-store/src/models.rs`, then run
`mise run db:migration:new <name>`. Toasty diffs the models against the last snapshot and
writes SQL into `crates/uptime-store/toasty/migrations/`. Things Toasty can't express (foreign
keys, composite indexes) go into hand-written migrations listed in `toasty/history.toml`.
Migrations are embedded in the binary; `uptimestatus migrate` applies them (run it as a
release step before `serve`).

SQLite and Turso use the same embedded SQLite-dialect migration set under
`crates/uptime-store/sqlite/`. Changes to the store schema must update both
migration histories. A new SQLite/Turso database starts at the current schema;
the PostgreSQL history retains its existing upgrade path. To generate the
SQLite/Turso side of a model change, run the migration command with
`UPTIMESTATUS_DATABASE__BACKEND=sqlite`; it uses an in-memory SQLite driver.

## Contributing

Issues and pull requests are welcome. A bug report with the monitor type, relevant
configuration (with secrets removed), and steps to reproduce is especially helpful.
For larger changes, open an issue first so we can discuss the scope.

Before sending a code change, run `mise run lint`, `mise run test` and `mise run test:doc`.
Schema changes need migrations for both PostgreSQL and SQLite/Turso, as described above.

## Credits

This project depends on the work of many other open-source maintainers. In particular:

| Project | What it provides here |
|---|---|
| [Starbase](https://github.com/zweiundeins/starbase), by zwei und eins and its contributors | Rocket web components, design tokens, themes and the starfield. |
| [Datastar](https://github.com/starfederation/datastar), by Star Federation and its contributors | Browser reactivity and live updates over SSE, including the Rocket runtime used by Starbase. |
| [Topcoat](https://github.com/tokio-rs/topcoat) | Server-rendered Rust views, routing, forms, cookies and Datastar integration. |
| [Toasty](https://github.com/tokio-rs/toasty) | Database models, queries, drivers and embedded migrations. |
| [Tokio](https://github.com/tokio-rs/tokio), [Axum](https://github.com/tokio-rs/axum) and [Tower](https://github.com/tower-rs/tower) | Async runtime, HTTP server and middleware. |
| [Reqwest](https://github.com/seanmonstar/reqwest), [Rustls](https://github.com/rustls/rustls) and [Hickory DNS](https://github.com/hickory-dns/hickory-dns) | HTTP requests, TLS and DNS checks. |
| [Jiff](https://github.com/BurntSushi/jiff) and [Serde](https://github.com/serde-rs/serde) | Time handling and serialization. |
| [Figment](https://github.com/SergioBenitez/Figment), [Clap](https://github.com/clap-rs/clap) and [Tracing](https://github.com/tokio-rs/tracing) | Configuration, command-line parsing and logs. |
| [Wiremock](https://github.com/LukeMathWalker/wiremock-rs) and [rcgen](https://github.com/rustls/rcgen) | HTTP fixtures and certificates for integration tests. |

The monitoring policies take inspiration from [Uptime Kuma](https://github.com/louislam/uptime-kuma).
The full dependency list is in [Cargo.toml](Cargo.toml) and the workspace crates' manifests.

## License

uptimestatus is dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at
your option. Contributions are accepted under the same terms. Dependencies keep their own licenses.

Vendored assets include:

- **Starbase** components and styles under MIT: [license](crates/uptime-web/static/starbase/LICENSE),
  [source and modification notes](crates/uptime-web/static/starbase/README.md).
- **Datastar v1.0.4 with Rocket** under MIT: [license](crates/uptime-web/vendor/datastar/LICENSE),
  [vendoring notes](crates/uptime-web/vendor/datastar/README.md).
- **Pixelify Sans** and **JetBrains Mono** under the SIL Open Font License:
  [Pixelify Sans license](crates/uptime-web/static/starbase/fonts/OFL-PixelifySans.txt),
  [JetBrains Mono license](crates/uptime-web/static/starbase/fonts/OFL-JetBrainsMono.txt).
- **Prism**, used by Starbase's code editor, under MIT:
  [license](crates/uptime-web/static/starbase/c/code-editor/vendor/LICENSE-prism.txt).
