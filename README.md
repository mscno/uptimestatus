# uptimestatus

A self-hosted uptime monitor and status page app, written in Rust. It's an open-source
hobby project for keeping an eye on a small collection of services. Run it as one binary
or a Docker container, with SQLite, PostgreSQL or Turso for storage.

![PrivateNPM status page, using the pixel theme](docs/screenshot.png)

## Features

- HTTP(S), TCP, DNS, push heartbeat, ICMP ping, database (PostgreSQL, MySQL/MariaDB, Redis),
  gRPC health and MQTT monitors, with configurable timeouts and retries. HTTP checks support
  status ranges, keywords, JSON assertions, custom requests and auth; TCP checks can use TLS and
  send/expect rules. See [Protocol checks](#protocol-checks). Group monitors follow the
  monitors in a folder, with their own alerts, incidents and uptime; see
  [Group monitors](#group-monitors).
- Automatic incidents after the original check and all configured retries fail, with at least
  two failed attempts. Monitors retry once by default. Checks continue at the retry interval
  while down, returning to the normal interval on recovery. Recovery resolves the incident;
  the admin timeline keeps errors, statuses and response excerpts, including the original
  attempt and retries.
- Public status pages with current and previous incidents, 90-day uptime bars, response-time
  charts, Atom feeds, custom domains, logos and a choice of pixel or clean styling. Manual
  components and status overrides, custom CSS and README badges; see
  [Status pages](#status-pages).
- Slack, Discord, email and signed webhook alerts, with retries, quiet hours and escalation
  delays.
- Status page subscriptions by email or webhook, with double opt-in, branded emails sent from
  the page's own domain, and per-component choices; see [Email and subscriptions](#email-and-subscriptions).
- Incident templates with placeholders, and declarations and updates scheduled for later;
  see [Incidents](#incidents).
- Monthly SLA uptime reports per status page, as web pages and branded PDFs, public or
  shared by link; see [Monthly reports](#monthly-reports).
- TLS certificate-expiry warnings and one-off or recurring maintenance windows.
- A live admin console with GitHub sign-in, tags, groups, search, bulk actions and "Test now".
  Status pages and the console update over SSE when checks finish.
- A [JSON API](#json-api) and TOML import/export for monitors, pages and incident templates.

## Quick start

You need a GitHub OAuth app for the admin console (Settings → Developer settings → OAuth
apps; callback URL `<APP_URL>/auth/github/callback`, e.g. `http://localhost:8080/auth/github/callback`).

```sh
cp .env.example .env      # set UPTIMESTATUS_AUTH__ADMINS, GITHUB_CLIENT_ID/SECRET, COOKIE_KEY, SECRETS__KEYS
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
| `GET /api/v1/pages/{slug}/components` | read | Components with their manual status or override, when and by whom it was set, and the note |
| `PUT /api/v1/pages/{slug}/components/{key}/status` | write | `{"status": "degraded", "note": "…"}`: a manual component's status, or a monitor component's override; `{"status": null}` clears an override |
| `POST /api/v1/incidents` | write | `{"title", "message", "impact", "status", "monitors": ["key"]}`; `"template": "name"` fills what is not given |
| `POST /api/v1/incidents/{id}/updates` | write | `{"status", "message"}`; `"template": "name"` fills what is not given |

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
| `UPTIMESTATUS_SECRETS__KEYS` | – | Keys that encrypt secrets at rest, `id:base64,…` (first one active); required in release builds. See [Secrets at rest](#secrets-at-rest) |
| `RUST_LOG` | `info` | `tracing` filter directives |

Durations use humantime syntax (`30s`, `5m`, `90days`).

### Secrets at rest

Monitor checks (URLs, credentials, headers, push tokens, request bodies), alert channel URLs and
signing secrets, and response excerpts kept with failed checks and incidents are encrypted in the
database with XChaCha20-Poly1305. Generate a key with `uptimestatus secrets generate-key` (no
database needed) and set `UPTIMESTATUS_SECRETS__KEYS` to it. Release builds refuse to start
without keys; an instance without keys also refuses a database that holds encrypted values.

- **Back the key up** outside the database (a password manager, your platform's secret store).
  Without it, encrypted values cannot be recovered; database backups alone are not enough.
- **Upgrading** an existing database: set the key and start as usual. Configuration (monitors,
  channels) is sealed before serving; check history is sealed in the background.
- **Rotating:** generate a new key and put it first, keeping the old one after it
  (`new-id:…,old-id:…`), restart, then run `uptimestatus secrets rotate` (or wait for the
  background pass) and remove the old key.
- **Transfers** (`uptimestatus-transfer`) copy ciphertext as-is: the target needs the same keys.
  `export` writes plaintext TOML, so treat its output as a secret.

### Audit trail

Every change made in the console or through the write API is recorded with who made it (an
admin's GitHub login, `api:<token name>`, or `system` for `secrets rotate`), what changed and
when: monitors, pages, domains, images, incidents, maintenance, alert channels, API tokens,
bulk actions, uptime corrections (with their reason), configuration exports and sign-ins.
Changed fields are listed with their old and new values; credentials are shown as `***`, so a
changed password is visible as changed but never stored. Browse and filter it under **Audit**
in the console. The trail is kept forever (retention only prunes check history).

### Email and subscriptions

Email goes through [Lettermint](https://lettermint.co) and is **off** unless a project token is
set; without it the console says so, email channels cannot be chosen, and status pages offer
no subscriptions.

| Variable | |
|---|---|
| `UPTIMESTATUS_EMAIL__LETTERMINT_PROJECT_TOKEN` | Project (sending) token, `lm_…`; turns email on |
| `UPTIMESTATUS_EMAIL__DEFAULT_FROM` | Sender for alerts and for pages without a verified domain, e.g. `uptimestatus <alerts@example.com>` (required with a token; its domain must be verified at Lettermint) |
| `UPTIMESTATUS_EMAIL__LETTERMINT_TEAM_TOKEN` | Optional team token (`read:domains`, `write:domains`) so the console can register and verify pages' sending domains |
| `UPTIMESTATUS_EMAIL__WEBHOOK_SECRET` | Optional signing secret of a Lettermint webhook pointed at `<APP_URL>/webhooks/lettermint` (bounces, complaints, unsubscribes) |

- **Email alert channels** send to up to 10 addresses from the default sender, with the
  same routing (escalation, quiet hours, muted events) as other channels.
- **Each status page** has an **Email** page in the console: whether visitors may subscribe,
  whether automatic incidents are announced (or only declared ones), and its sender name,
  address and Reply-To. With a team token, **Set up sending domain** registers the sender's
  domain at Lettermint and lists its DNS records (DKIM CNAMEs, Return-Path, DMARC) to publish,
  then **Verify**. Without one, add the domain in the Lettermint dashboard and **Send test**:
  once a test is accepted from the page's sender, the page sends from it. Until its domain is
  verified, a page's email comes from the default sender under the page's name, with the
  page's address as Reply-To.
- **Visitors subscribe** from the page footer (`/s/{slug}/subscribe`, also on custom domains)
  by email, or with a webhook confirmed by email, for every component or a chosen few. They
  confirm by email first (nothing else is sent until then), manage their components or leave
  from links in every email, and mail clients can unsubscribe in one click
  (`List-Unsubscribe-Post`). Requests are rate-limited, a hidden field catches simple bots,
  and the page never tells whether an address is already subscribed.
- **Subscribers hear about** incidents when declared, updated and resolved (automatic ones too,
  unless the page opts out; incidents declared as already resolved are not announced),
  maintenance when scheduled, starting and completed, and manual component statuses and
  overrides (clearing an override is not announced). Emails carry the page's title, logo,
  accent and look, link to its custom domain when it has one, and have a plain-text version.
- **Webhook subscribers** receive `POST`s with `X-Uptimestatus-Event`, `X-Uptimestatus-Timestamp`
  and `X-Uptimestatus-Signature: sha256=HMAC-SHA256(secret, "{timestamp}.{body}")` (hex). The
  signing secret is shown once after confirming (and can be replaced from the manage link).
  Bodies look like:

  ```json
  {"id": "42", "type": "incident.created", "created_at": "2026-10-15T09:00:00Z",
   "page": {"slug": "acme", "title": "Acme Status", "url": "https://status.acme.com/"},
   "data": {"id": 7, "title": "API errors", "status": "investigating", "impact": "major",
            "message": "…", "started_at": "…", "resolved_at": null, "at": "…",
            "monitors": ["api"], "components": ["Public API"]}}
  ```

  `type` is one of `incident.created`, `incident.updated`, `incident.resolved`,
  `maintenance.scheduled`, `maintenance.started`, `maintenance.completed` and
  `component.status_changed`. Endpoints must be public `https://` URLs (private addresses are
  refused when sending). Deliveries are retried with backoff; a webhook that fails ten times in
  a row over at least a day is disabled and its owner is emailed.
- **Subscribers** are listed (masked) under each page's **Subscribers**, where admins can stop
  or remove them and export a CSV; all of this is audited. Addresses and webhook URLs are
  encrypted at rest and found through blind indexes. Bounces, complaints and suppressions
  reported by Lettermint's webhook stop all email to that address.

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

`uptimestatus [serve [--migrate] [--roles web,worker]]`, `migrate`, `seed <file>`, `export [-o file]`,
`secrets generate-key`, `secrets rotate`.

| Command | What it does |
|---|---|
| `serve` (default) | Everything in one process: `--roles web,worker` |
| `serve --roles web` | Only HTTP: console, status pages, push API. No checks are run. |
| `serve --roles worker` | Only the scheduler, alert sender, domain verifier and retention janitor (HTTP health endpoints only) |
| `migrate` | Apply pending migrations and exit (release step) |
| `seed file.toml` | Create the monitors and pages in a TOML file; existing keys and slugs are left alone |
| `export` | Write all monitors and pages as TOML, the format `seed` reads (secrets in plaintext) |
| `secrets generate-key` | Print a new key for `UPTIMESTATUS_SECRETS__KEYS` |
| `secrets rotate` | Re-encrypt everything with the first key so older keys can be removed |

One `serve` process is enough for a small setup. If you want to separate the web app from
the checks, PostgreSQL supports `--roles web` and `--roles worker` instances sharing a
database; they coordinate over `LISTEN/NOTIFY`. SQLite and Turso use one instance with both roles.
`UPTIMESTATUS_CHECKS__ENABLED=false` and `UPTIMESTATUS_DOMAINS__VERIFY=false` turn off pieces
of the worker role in a process.

Monitors and pages can be created in the console, through the [JSON API](#json-api), or from
TOML (`seed`/`export`): `[[monitor]]` tables with a `check` (`type = "http"`, `"tcp"`, `"dns"`,
`"push"`, `"ping"`, `"database"`, `"grpc"`, `"mqtt"` or `"group"`) and an optional `policy` (`interval`, `retries`, `retry_interval`, `timeout`,
`invert`, `degraded_after`, `resend_every`), and `[[page]]` tables with `[[page.section]]`s. Pages can be styled with `accent` (a hex
color), `theme` (`auto`, `light`, `dark`) and `look` (`pixel`, the 8-bit default, or `clean`:
plain fonts and no starfield); the same options are in the page editor.
See [`seed/monitors.example.toml`](seed/monitors.example.toml).

### Protocol checks

Every check connects only to addresses the address policy permits (public ones, unless
`UPTIMESTATUS_CHECKS__ALLOW_PRIVATE_TARGETS` is set), and only within the monitor's timeout.
Each `[monitor.check]` below belongs to its own `[[monitor]]`:

```toml
[monitor.check]            # ICMP echo; DOWN when packet loss reaches the threshold
type = "ping"
host = "example.com"
count = 3                  # packets per check (1-10), default 3
packet_timeout = "1s"      # wait per reply, default 1s
loss_threshold_percent = 100  # default: DOWN only when every packet is lost

[monitor.check]            # a query; UP when it succeeds (and the first value contains `expect`)
type = "database"
url = "postgres://monitor:secret@db.example.com:5432/app?sslmode=require"
query = "SELECT count(*) FROM jobs WHERE failed"  # default SELECT 1 (PING for Redis)
expect = "0"
# ignore_tls_errors = true

[monitor.check]            # grpc.health.v1.Health/Check; UP when SERVING
type = "grpc"
url = "https://api.example.com:443"   # http:// for plaintext HTTP/2
service = "api.v1.Users"              # blank asks about the whole server
metadata = [["authorization", "Bearer …"]]

[monitor.check]            # CONNECT (and, with a topic, wait for one message)
type = "mqtt"
host = "broker.example.com"
tls = true                 # port defaults to 1883, or 8883 with TLS
username = "probe"
password = "secret"
topic = "devices/gateway/health"   # retained messages arrive at once
expect = "online"
```

- **Ping** uses an unprivileged ICMP socket (Linux: `net.ipv4.ping_group_range` must include
  the process's group; it also covers ICMPv6), falling back to a raw socket with `CAP_NET_RAW`.
  The image's entrypoint allows unprivileged ICMP when it starts as root in a VM (Fly
  Machines); Docker allows it by default. Without it, ping monitors fail with an explanation.
- **Database** URLs carry credentials and are kept out of logs, errors and read-only API
  responses (which see `scheme://host:port` only); use a read-only account. PostgreSQL takes
  libpq-style `sslmode` (`disable`, `prefer`, `require`, `verify-ca`, `verify-full`;
  certificates are verified against the system trust store unless errors are ignored);
  MySQL/MariaDB takes `?require_ssl=true`; Redis uses `rediss://` for TLS, a password in the
  URL, and a database number as the path. Queries are plain SQL, or a Redis command such as
  `GET health` (quote arguments with spaces).
- **gRPC** speaks HTTP/2, with TLS (ALPN `h2`) for `https://`. NOT_SERVING, or an unknown
  service, is DOWN.
- **MQTT** speaks 3.1.1. A refused CONNECT reports the broker's reason (e.g. bad username or
  password); a topic with no message before the timeout is DOWN.

### Group monitors

A group monitor summarises a folder: every monitor whose group is the path or below it,
including nested group monitors. It probes nothing; it judges its members' current states, so
it alerts, opens and resolves automatic incidents and keeps uptime history like any monitor,
and it can be a single component on a status page.

```toml
[[monitor]]
key = "platform"
name = "Platform"
check = { type = "group", path = "prod", mode = "any", show_members = true }
policy = { retries = 0 }    # members already confirm their own failures
```

- `mode = "any"` (default): DOWN when any member is DOWN, DEGRADED while any member is
  degraded or failing unconfirmed (pending). `mode = "all"`: DOWN only when every member is
  DOWN, DEGRADED while some are.
- Paused and never-checked members do not count, nor do members in maintenance unless all
  are (then the group is in maintenance). With nothing to judge the group is UNKNOWN and
  records no check, so its uptime is unaffected.
- Groups are judged after the other checks claimed at the same moment (deepest folder first),
  and a member changing state makes its groups due at once, so they follow without waiting
  for their own interval.
- Group monitors cannot include each other: a group summarising the folder another group sits
  in, while that group summarises this one's folder, is refused. A group in its own folder
  simply skips itself.
- `show_members` lists the members' names and states under the group on status pages (never
  their targets).

### Status pages

A page's layout lists monitors by key and, for things no check can see (a vendor, a
mailbox, a process), **manual components**: `manual:key | Display name | Description` in the
editor, `{ manual = "cdn", label = "CDN", description = "…" }` in TOML. Manual keys follow the
monitor key rules and must not repeat or match a monitor key on the same page.

Admins set a manual component's status (operational, degraded, partial outage, major outage,
maintenance) with one click from the page editor or **Status pages → Components**, or with
`PUT /api/v1/pages/{slug}/components/{key}/status`. A monitor component can be **overridden**
the same way (say, degraded while checks still pass) until the override is cleared. Every
change keeps who made it and an optional note, is audited, and refreshes open pages at once.

That history is the component's uptime, measured in time rather than checks: degraded counts
as up (like a slow check), a partial outage counts half its time as down, a major outage as
down, and maintenance is left out. A day with degraded or partial-outage time shows at least a
warning bar. An override replaces its share of a monitor's day; the rest still comes from its
checks. The page headline is the worst of its monitors, its manual statuses and any open
incident; a single component in major outage makes the headline a major outage.

**Custom CSS** (page editor, `custom_css` in TOML and the API; up to 64 KiB) is served as
`/s/{slug}/custom.css` after the built-in styles. `javascript:` URLs, `expression(`,
`-moz-binding`, `behavior:`, `</style` and `@import` of anything but `https://` are refused.
Stable hooks to target:

| Hook | What |
|---|---|
| `html[data-look="clean"]` | The clean look (the pixel look has no attribute) |
| `.status-page`, `.status-shell`, `.status-header`, `.status-logo`, `.status-tabs`, `.footer` | Page frame |
| `.banner` | The headline |
| `.notice` | Open incidents and maintenance |
| `.section`, `.component`, `.component-head`, `.component-description` | Sections and components |
| `.component[data-component="api"]` | One component, by key |
| `.component[data-status="down"]` | By status: monitor states (`up`, `degraded`, `pending`, `down`, `maintenance`, `paused`, `unknown`) or manual statuses (`operational`, `partial_outage`, `major_outage`, …) |
| `.bars`, `.bar-up`, `.bar-warn`, `.bar-down`, `.bar-none` | The 90-day bars |
| `--accent`, `--up`, `--degraded`, `--pending`, `--down`, `--maintenance` | Colors, on `:root` |

**README badges** (shields style; copy them from the page editor or a monitor's page):

| Badge | |
|---|---|
| `/s/{slug}/badge.svg` | The page's overall status |
| `/s/{slug}/badge/{key}/status.svg` | A component's status (manual status or override first) |
| `/s/{slug}/badge/{key}/uptime.svg?window=30d` | Uptime over `24h`, `7d`, `30d` (default) or `90d` |
| `/s/{slug}/badge/{key}/response.svg` | A monitor component's median response time, last 24 hours |

All take `?style=flat` (default), `flat-square` or `for-the-badge`, and `?label=` to replace
the left-hand text. They work on custom domains too (`/badge.svg`, `/badge/{key}/…`), are
cached for a minute with ETags, and exist only for components on a page: other keys are a
404, and drafts show badges only to signed-in admins.

### Incidents

**Templates** (**Incidents → Templates**, or `[[incident_template]]` in TOML with `name`,
`title`, `impact`, `status` and `body`) hold wording you reuse. Pick one above the declare
form or an incident's update form to fill it in, or name it as `"template"` in the API.
Placeholders are filled in when the incident is published: `{monitor}` (the first affected
monitor's name), `{monitors}` (all of them), `{page}` (the status pages showing it) and
`{started_at}` (when it started, UTC). Other text in braces is left as written.

**Scheduling:** give a declaration or an update a **Publish at** time (UTC) to publish it
later; choose *Resolved* to schedule the resolution. Scheduled items show in the console
(declarations on the Incidents page, updates on their incident) where they can be edited or
cancelled, and are invisible on status pages until they publish. The worker publishes them
within about 15 seconds of their time, through the same path as a manual update: status
pages refresh, subscribers are notified then (not when it was scheduled), and the audit
trail names the admin who scheduled it. Publishing removes the schedule in the same
transaction, so an item is never published twice, even with several workers. If publishing
fails (say, its monitors were deleted) it waits with the error until edited.

### Monthly reports

Every status page has monthly SLA uptime reports: `/s/{slug}/reports` lists the months with
history, `/s/{slug}/reports/2026-09` shows one (month to date for the current month), and
`/s/{slug}/reports/2026-09.pdf` is the same report as a branded PDF (the page's title, logo,
accent and look). Reports cover each component's uptime, downtime and maintenance time, a bar
per day, the incidents and maintenance windows of the month and an overall figure (the mean of
the components). They count what status pages count: checks, manual statuses and overrides,
with maintenance left out and excused checks counted as up; a report says how many failed
checks an administrator excused that month. Downtime is the observed time in proportion to
the down checks, so for checked components it is an estimate at the check interval.

Under **Status pages → Reports** admins choose who may read them and set an optional
**uptime target** (e.g. `99.9`), which reports mark as met or missed per component:

- **Public** (default): a **Reports** tab on the published page; also on custom domains.
- **Private:** only signed-in admins, and **report links**: `…/reports?key=rpt_…` (every
  month) or `…/reports/2026-09?key=rpt_…` (one month), with an optional expiry. A link is
  shown once; only its hash is stored. Revoke links from the same page; creating and revoking
  them is audited.

PDFs are typeset in-process with [Typst](https://typst.app) and the fonts embedded in the
binary (no network access); rendering takes milliseconds and a few dozen MB. Rendered PDFs
are cached by their content, so corrections (excusing checks, editing incidents) show at once.

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
`portable` tests. Probe tests run gRPC, MQTT and Redis against in-process servers; set
`UPTIMESTATUS_TEST_MYSQL_URL` (e.g. `mysql://root:pw@127.0.0.1:3306/mysql`, a stock server
with its self-signed certificate) and `UPTIMESTATUS_TEST_REDIS_URL` to also probe real ones.

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
| [Typst](https://github.com/typst/typst) | Typesetting the PDF reports. |
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
- **IBM Plex Sans** under the SIL Open Font License, with TrueType copies of Pixelify Sans
  and JetBrains Mono, embedded for PDF reports: [fonts and sources](crates/uptime-web/src/reports/fonts/README.md).
- **Prism**, used by Starbase's code editor, under MIT:
  [license](crates/uptime-web/static/starbase/c/code-editor/vendor/LICENSE-prism.txt).
