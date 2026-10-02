//! A fresh, fully migrated Postgres database for every test.
//!
//! The first test that runs after a migration change builds a template database
//! (`uptimestatus_tpl_<fingerprint>`) by applying every migration. Each test then
//! clones it with `CREATE DATABASE … TEMPLATE …`, which takes milliseconds, and
//! drops its clone when the [`TestDb`] goes out of scope (clones skip the
//! synchronous WAL flush on commit: they are throwaway). A test that panics
//! keeps its database for inspection (`mise run db:test:clean` removes leftovers).
//!
//! The server comes from `PG_ADMIN_URL`, defaulting to the local development
//! server on port 5433.

#![allow(clippy::expect_used, clippy::print_stderr)] // test support: fail loudly

pub mod dns;
pub mod logs;

use tokio_postgres::{Client, NoTls};
use uptime_store::{ConnectOptions, MIGRATIONS, Store};
use url::Url;

const DEFAULT_ADMIN_URL: &str = "postgres://postgres:postgres@localhost:5433/postgres";
const TEST_DB_PREFIX: &str = "uptimestatus_test_";
const TEMPLATE_PREFIX: &str = "uptimestatus_tpl_";

/// A database that exists for the lifetime of one test.
#[derive(Debug)]
pub struct TestDb {
    name: String,
    url: String,
    admin_url: String,
    store: Option<Store>,
}

impl TestDb {
    /// Creates a new migrated database and connects a [`Store`] to it.
    pub async fn new() -> Self {
        let admin_url =
            std::env::var("PG_ADMIN_URL").unwrap_or_else(|_| DEFAULT_ADMIN_URL.to_owned());
        let admin = connect(&admin_url).await;
        let template = ensure_template(&admin, &admin_url).await;

        let name = format!("{TEST_DB_PREFIX}{}", uuid::Uuid::new_v4().simple());
        admin
            .batch_execute(&format!(
                r#"CREATE DATABASE "{name}" TEMPLATE "{template}""#
            ))
            .await
            .expect("create test database from template");
        // The database is thrown away after the test, so commits need not wait
        // for the WAL flush. With hundreds of tests committing in parallel to
        // one server (often in a VM), those waits queue up and make unrelated
        // tests time out.
        admin
            .batch_execute(&format!(
                r#"ALTER DATABASE "{name}" SET synchronous_commit = off"#
            ))
            .await
            .expect("relax durability of the test database");

        let url = database_url(&admin_url, &name);
        let store = Store::connect(&url, &ConnectOptions::default())
            .await
            .expect("connect to test database");
        Self {
            name,
            url,
            admin_url,
            store: Some(store),
        }
    }

    pub fn store(&self) -> &Store {
        self.store.as_ref().expect("store is present until drop")
    }

    /// Connection URL of this test's database.
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        drop(self.store.take());
        if std::thread::panicking() {
            eprintln!(
                "test failed; keeping database `{}` for inspection",
                self.name
            );
            return;
        }
        let (admin_url, name) = (self.admin_url.clone(), self.name.clone());
        // Drop runs inside the test's runtime, so do the async cleanup on a
        // separate thread with its own runtime.
        let cleanup = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("cleanup runtime");
            runtime.block_on(async {
                let admin = connect(&admin_url).await;
                admin
                    .batch_execute(&format!(r#"DROP DATABASE IF EXISTS "{name}" WITH (FORCE)"#))
                    .await
            })
        });
        if let Ok(Err(error)) = cleanup.join() {
            eprintln!("could not drop test database `{}`: {error}", self.name);
        }
    }
}

async fn connect(url: &str) -> Client {
    let (client, connection) = tokio_postgres::connect(url, NoTls)
        .await
        .unwrap_or_else(|e| panic!("cannot reach the test Postgres server at {url}: {e}"));
    tokio::spawn(connection);
    client
}

/// Returns the name of a template database with every current migration
/// applied, building it once per migration fingerprint.
async fn ensure_template(admin: &Client, admin_url: &str) -> String {
    let fingerprint = migrations_fingerprint();
    let template = format!("{TEMPLATE_PREFIX}{fingerprint:016x}");
    // Serialize template builds across concurrent test processes.
    #[allow(clippy::cast_possible_wrap)] // any 64-bit value is a valid lock key
    let lock_key = fingerprint as i64;
    admin
        .execute("SELECT pg_advisory_lock($1)", &[&lock_key])
        .await
        .expect("take template lock");

    let exists = admin
        .query_opt("SELECT 1 FROM pg_database WHERE datname = $1", &[&template])
        .await
        .expect("look up template")
        .is_some();
    if !exists {
        build_template(admin, admin_url, &template).await;
    }

    admin
        .execute("SELECT pg_advisory_unlock($1)", &[&lock_key])
        .await
        .expect("release template lock");
    template
}

async fn build_template(admin: &Client, admin_url: &str, template: &str) {
    let building = format!("{template}_building");
    // Each statement on its own: a multi-statement batch is an implicit
    // transaction, and CREATE/DROP DATABASE cannot run inside one.
    for sql in [
        format!(r#"DROP DATABASE IF EXISTS "{building}" WITH (FORCE)"#),
        format!(r#"CREATE DATABASE "{building}""#),
    ] {
        admin
            .batch_execute(&sql)
            .await
            .expect("create template database");
    }

    {
        let store = Store::connect(
            &database_url(admin_url, &building),
            &ConnectOptions::default(),
        )
        .await
        .expect("connect to template database");
        store.migrate().await.expect("migrate template database");
    }

    // Publish atomically: nobody may be connected while renaming or cloning.
    admin
        .execute(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = $1 AND pid <> pg_backend_pid()",
            &[&building],
        )
        .await
        .expect("disconnect from template database");
    for sql in [
        format!(r#"ALTER DATABASE "{building}" RENAME TO "{template}""#),
        format!(r#"ALTER DATABASE "{template}" WITH IS_TEMPLATE true ALLOW_CONNECTIONS false"#),
    ] {
        admin
            .batch_execute(&sql)
            .await
            .expect("publish template database");
    }
}

/// Stable hash of every embedded migration (FNV-1a), so the template is rebuilt
/// exactly when migrations change.
fn migrations_fingerprint() -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    MIGRATIONS
        .migrations()
        .iter()
        .flat_map(|m| {
            m.id()
                .to_le_bytes()
                .into_iter()
                .chain(m.name().bytes())
                .chain(m.sql().bytes())
        })
        .fold(OFFSET, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(PRIME)
        })
}

fn database_url(admin_url: &str, database: &str) -> String {
    let mut url = Url::parse(admin_url).expect("admin URL is a valid URL");
    url.set_path(database);
    url.into()
}
