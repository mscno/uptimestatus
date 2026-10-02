# Security audit (2026-09-30)

Scope: application code for persisted data, browser and API authorization,
outbound network access, and secret exposure. This is a source and local test
audit. It does not establish the settings of a live proxy, database, volume,
backup system, or GitHub repository.

## Data and trust boundaries

- Admins sign in through GitHub OAuth with state and PKCE. The flow cookie is
  encrypted and short lived. Sessions use `Secure`, `HttpOnly`, `SameSite=Lax`
  cookies; only SHA-256 token hashes are stored. Each request checks the
  allowlist and session expiry.
- JSON API tokens are 32 random bytes; only SHA-256 hashes are stored. Read
  tokens receive reduced monitor data. Write tokens and authenticated admins
  can retrieve full monitor definitions. Admin TOML export includes secrets and
  must be handled as a credential file.
- Monitor checks, including HTTP headers, bodies, URL credentials and push
  tokens, are stored as JSON in the database. Notification webhook URLs and
  signing secrets are also stored in database columns as plaintext. They must
  remain available to workers for probing and delivery. The application does
  **not** provide field encryption for these values.
- Uploaded media is public by design, named by a content hash, and constrained
  by size and file type. Probes and guarded notification sends filter resolved
  private addresses unless the development override is enabled.
- The app expects a TLS terminating proxy. PostgreSQL TLS behavior depends on
  `DATABASE_URL`'s `sslmode`; `verify-full` is the documented choice for hosted
  databases. The internal health listener is not published by `compose.yaml`.

## Confirmed application findings and fixes

| Finding | Verification | Fix |
| --- | --- | --- |
| Read-only API tokens could retrieve secrets in HTTP URL userinfo/path/query, arbitrary headers, request bodies and TCP payloads. | An integration test using a read token reproduced the leak from `GET /api/v1/config`; the response contained every injected value. | Redact all outbound request and match values, URL path/query/userinfo, and diagnostic errors for read tokens on monitor/config/check endpoints. Write access still returns the original data. |
| A failed webhook delivery persisted the full webhook URL, which can itself be a Slack/Discord credential. Receiver response bodies could also be stored in delivery history; reading them without a bound allowed excessive memory use. | A connection-failure test produced `DeliveryError` with the secret URL path. | Persist only an error category or HTTP status. Read at most 4 KB of a rate-limit response to parse its retry delay. |
| Same-site pages could submit cookie-authenticated admin mutations from a different origin. `SameSite=Lax` does not separate origins on the same site. | The router previously performed no Origin or Fetch Metadata check before `/admin` mutations. An integration test now sends a signed-in request from `evil.example.com` and requires `403` with no token created. | Reject explicit foreign `Origin` and cross/same-site Fetch Metadata for unsafe admin and logout requests. |
| Debug formatting of checks, webhook channels, forms and OAuth settings could emit secrets if logged. | The affected structs derived `Debug` over secret-bearing fields. | Replace those implementations with limited, non-secret fields and test the output. |
| Admin and API responses carrying credentials lacked an explicit cache policy. | The router had no cache guard on those routes. | Add `Cache-Control: no-store` to admin, auth and API responses, including errors. |

## Deployment checks still required

Application source cannot verify whether the live database, SQLite volume and
backups are encrypted or who can read them. A database-only or backup-only
compromise exposes monitor and webhook credentials because there is no app-level
field encryption. Restrict database and backup access, enable encryption at rest
in the chosen storage service, and keep credentials out of exported TOML files.
For hosted PostgreSQL, set `sslmode=verify-full` with a trusted CA; do not rely
on TLS downgrade modes. Confirm that the proxy enforces HTTPS and does not
publish the internal listener. Rotate monitor and webhook credentials that were
available to previously distributed read tokens.

This audit did not scan dependencies against a current vulnerability database
or assess the live deployment configuration.
