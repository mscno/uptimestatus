//! Process wiring: connect, build state, serve both listeners until shutdown.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::Context as _;
use tokio::{net::TcpListener, sync::Notify};
use tokio_util::sync::CancellationToken;
use uptime_domain::Edge;
use uptime_notify::Sender;
use uptime_probe::{AddressPolicy, DomainInspector, Prober, ProberConfig};
use uptime_runtime::{
    Cluster, DomainVerifier, EventBus, Handlers, Janitor, Notifier, NotifierConfig, Reload,
    Scheduler, SchedulerConfig, VerifierConfig, VerifyDomain,
};
use uptime_store::bus::BusMessage;
use uptime_store::{Backend, ConnectOptions, Store};
use uptime_web::{
    AppState, DomainCache, Hosts,
    auth::{AuthSettings, GithubOAuth, Key},
};

use crate::{cli::Roles, config::Config};

/// Connects to the database described by `config`.
pub async fn connect(config: &Config) -> anyhow::Result<Store> {
    if config.database.require_existing {
        anyhow::ensure!(
            config.database.backend == Backend::Turso,
            "database.require_existing requires a local Turso database"
        );
        let path = config
            .database_url
            .expose()
            .strip_prefix("turso:")
            .filter(|path| path.starts_with('/') && !path.starts_with("//"))
            .context("database.require_existing requires an absolute local turso: path")?;
        let path = std::path::Path::new(path);
        let metadata = std::fs::metadata(path).context("required database is missing")?;
        anyhow::ensure!(
            metadata.is_file() && metadata.len() >= 4096,
            "required database is empty or invalid"
        );
        let marker = std::fs::read(uptime_transfer::turso::marker_path(path))
            .context("required database has no completed import report")?;
        let report: uptime_transfer::snapshot::Report = serde_json::from_slice(&marker)?;
        anyhow::ensure!(
            report.format == 1
                && report.schema == uptime_transfer::snapshot::schema_hash()
                && report.tables.len() == uptime_transfer::schema::TABLES.len(),
            "required database import report is incompatible"
        );
    }
    let options = ConnectOptions {
        max_connections: config.database.max_connections,
        ..Default::default()
    };
    Store::connect_backend(
        config.database_url.expose(),
        config.database.backend,
        config
            .database
            .auth_token
            .as_ref()
            .map(crate::config::Secret::expose),
        &options,
    )
    .await
    .context("connecting to the database")
}

/// Builds the prober shared by the scheduler and "Test now".
pub fn prober(config: &Config) -> anyhow::Result<Prober> {
    let allow_private = config.checks.allow_private_targets;
    if allow_private {
        tracing::warn!(
            "probes may reach private and internal addresses (checks.allow_private_targets)"
        );
    }
    Ok(Prober::new(ProberConfig {
        policy: if allow_private {
            AddressPolicy::ALLOW_ALL
        } else {
            AddressPolicy::PUBLIC_ONLY
        },
        user_agent: format!(
            "uptimestatus/{} (+{})",
            env!("CARGO_PKG_VERSION"),
            config.app_url
        ),
        ..ProberConfig::default()
    })?)
}

/// Builds the admin sign-in settings from `config.auth`.
pub fn auth_settings(config: &Config) -> anyhow::Result<AuthSettings> {
    let auth = &config.auth;
    let key = match auth.cookie_key()? {
        Some(key) => key,
        None => {
            tracing::warn!(
                "auth.cookie_key is not set; using a per-process key (sign-ins break across instances)"
            );
            Key::generate()
        }
    };
    let allowlist = auth.allowlist();
    if allowlist.is_empty() {
        tracing::warn!("auth.admins is empty; nobody can sign in to the console");
    }
    let mut settings = AuthSettings::new(allowlist, key)
        .with_session_limits(auth.session_idle, auth.session_max_age)
        .with_dev_login(auth.dev_login.clone());
    match (&auth.github_client_id, &auth.github_client_secret) {
        (Some(id), Some(secret)) => {
            settings = settings.with_github(GithubOAuth::new(id, secret.expose()))
        }
        (None, None) => {
            tracing::warn!("GitHub sign-in is not configured (auth.github_client_id/secret)")
        }
        _ => anyhow::bail!("set both auth.github_client_id and auth.github_client_secret"),
    }
    Ok(settings)
}

/// Builds the alert sender: webhooks obey the probes' address policy.
pub fn sender(config: &Config, prober: &Arc<Prober>) -> anyhow::Result<Sender> {
    let user_agent = format!("uptimestatus/{}", env!("CARGO_PKG_VERSION"));
    Ok(Sender::guarded(prober, &user_agent)?.with_app_url(config.app_url.clone()))
}

/// Background services the web state hands work to.
#[derive(Default)]
pub struct Services {
    pub scheduler: Option<Arc<Notify>>,
    pub domains: DomainCache,
    pub verifier: Option<Arc<dyn VerifyDomain>>,
}

/// Builds the shared web state.
pub fn app_state(
    config: &Config,
    store: Store,
    events: EventBus,
    prober: Arc<Prober>,
    services: Services,
) -> anyhow::Result<AppState> {
    let mut state = AppState::new(store, Hosts::new(config.app_url.clone(), &config.edge_host))
        .with_sender(sender(config, &prober)?)
        .with_domains(services.domains)
        .with_edge(edge(config))
        .with_auth(auth_settings(config)?)
        .with_events(events)
        .with_prober(prober);
    if let Some(waker) = services.scheduler {
        state = state.with_scheduler(waker);
    }
    if let Some(verifier) = services.verifier {
        state = state.with_verifier(verifier);
    }
    if let Some(path) = &config.storage.path {
        let media = uptime_web::media::MediaStore::open(path)
            .with_context(|| format!("opening the storage directory {}", path.display()))?;
        state = state.with_media(media);
    } else {
        tracing::info!("image uploads disabled (storage.path is not set)");
    }
    Ok(state)
}

/// Where custom domains must point.
pub fn edge(config: &Config) -> Edge {
    Edge {
        host: config.edge_host.clone(),
        addresses: config.domains.edge_ips.clone(),
    }
}

/// The verified custom domains, loaded before the first request so the edge's
/// routing works immediately after a restart.
pub async fn load_domains(store: &Store) -> anyhow::Result<DomainCache> {
    let domains = DomainCache::default();
    let count = domains
        .reload(store)
        .await
        .context("loading custom domains")?;
    tracing::info!(count, "custom domains loaded");
    Ok(domains)
}

/// Builds the custom-domain verifier described by `config.domains`, publishing
/// into `domains`.
pub fn domain_verifier(
    config: &Config,
    store: Store,
    domains: &DomainCache,
) -> anyhow::Result<DomainVerifier<DomainInspector>> {
    let settings = &config.domains;
    let inspector = DomainInspector::new()?;
    let verifier_config = VerifierConfig {
        edge: edge(config),
        every: settings.verify_every,
        recheck_after: settings.recheck_after,
        prewarm_tls: settings.prewarm_tls,
    };
    Ok(DomainVerifier::new(store, Arc::new(inspector), verifier_config).with_sink(domains.sink()))
}

/// Builds the check scheduler described by `config.checks`.
pub fn scheduler(
    config: &Config,
    store: Store,
    prober: Arc<Prober>,
    events: EventBus,
) -> Scheduler<Prober> {
    let checks = &config.checks;
    let scheduler_config = SchedulerConfig {
        claimer: std::env::var("HOSTNAME").unwrap_or_else(|_| "local".to_owned()),
        region: checks.region.clone().unwrap_or_else(|| "local".to_owned()),
        concurrency: checks.concurrency,
        poll_interval: checks.poll_interval,
        heartbeat_url: checks.heartbeat_url.clone(),
        ..SchedulerConfig::default()
    };
    Scheduler::new(store, prober, events, scheduler_config)
}

/// How often old check results and expired sessions are pruned.
const JANITOR_EVERY: Duration = Duration::from_secs(60 * 60);

/// How often an instance reloads verified domains even without a bus message.
const DOMAIN_RELOAD_EVERY: Duration = Duration::from_secs(5 * 60);

/// This instance's name on the bus: the container or host name (`HOSTNAME`,
/// for readable logs) and a random suffix, so two processes on one host (a web
/// and a worker, say) never share a name and ignore each other's messages.
fn instance_id() -> String {
    let mut bytes = [0u8; 4];
    let _ = getrandom::fill(&mut bytes);
    let suffix = u32::from_be_bytes(bytes);
    match std::env::var("HOSTNAME") {
        Ok(host) if !host.is_empty() => format!("{host}-{suffix:08x}"),
        _ => format!("local-{suffix:08x}"),
    }
}

/// Starts what `roles` asks for and serves until `shutdown` is cancelled (or a
/// server fails, which also stops the rest).
///
/// - `web`: the public listener (and the internal one).
/// - `worker`: scheduler, janitor, alert sender and domain verification loop;
///   a worker-only instance serves just the internal listener (health).
///
/// Every instance joins the Postgres bus: check events reach dashboards on
/// other instances, edits wake remote schedulers, and domain changes reload
/// every instance's host routing.
pub async fn run(
    config: &Config,
    store: Store,
    roles: Roles,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    if store.backend() != Backend::Postgres && roles != Roles::ALL {
        anyhow::bail!("SQLite and Turso require one instance with both web and worker roles");
    }
    let public = if roles.web {
        Some(bind(SocketAddr::new(config.http.host, config.http.port)).await?)
    } else {
        None
    };
    let internal = bind(SocketAddr::new(config.http.host, config.http.internal_port)).await?;
    let instance = instance_id();
    tracing::info!(%instance, %roles, "starting");
    let cluster = Cluster::new(store.clone(), &instance);
    let events = EventBus::default().with_forwarder(cluster.forwarder());
    let prober = Arc::new(prober(config)?);
    // Reading the system's DNS settings can take seconds on some platforms:
    // do it in the background so startup (and checks of IP addresses) never wait.
    tokio::spawn({
        let prober = prober.clone();
        async move { prober.warm_up().await }
    });

    let mut background = Vec::new();
    let domains = load_domains(&store).await?;
    let reload: Reload = {
        let (domains, store) = (domains.clone(), store.clone());
        Arc::new(move || {
            let (domains, store) = (domains.clone(), store.clone());
            Box::pin(async move {
                if let Err(error) = domains.reload(&store).await {
                    tracing::warn!(error = %uptime_domain::Report(&error), "reloading custom domains failed");
                }
            })
        })
    };
    let verifier = if config.domains.verify {
        let publish = cluster.clone();
        let local = domains.sink();
        let verifier = domain_verifier(config, store.clone(), &domains)?.with_sink(Arc::new(
            move |verified| {
                local(verified);
                publish.publish(BusMessage::DomainsChanged);
            },
        ));
        if roles.worker {
            background.push(tokio::spawn(verifier.clone().run(shutdown.clone())));
        }
        Some(Arc::new(verifier) as Arc<dyn VerifyDomain>)
    } else {
        tracing::info!("custom-domain verification disabled (domains.verify = false)");
        None
    };
    let waker = if roles.worker && config.checks.enabled {
        let scheduler = scheduler(config, store.clone(), prober.clone(), events.clone());
        let waker = scheduler.waker();
        background.push(tokio::spawn(scheduler.run(shutdown.clone())));
        let janitor = Janitor::new(store.clone(), config.checks.retention, JANITOR_EVERY);
        background.push(tokio::spawn(janitor.run(shutdown.clone())));
        let notifier = Notifier::new(
            store.clone(),
            Arc::new(sender(config, &prober)?),
            NotifierConfig::default(),
        )
        .with_events(events.clone());
        background.push(tokio::spawn(notifier.run(shutdown.clone())));
        Some(waker)
    } else {
        if roles.worker {
            tracing::info!("scheduler disabled (checks.enabled = false)");
        }
        None
    };
    if roles.web {
        let (reload, shutdown) = (reload.clone(), shutdown.clone());
        background.push(tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    () = tokio::time::sleep(DOMAIN_RELOAD_EVERY) => reload().await,
                }
            }
        }));
    }
    let handlers = Handlers {
        events: roles.web.then(|| events.clone()),
        wake: waker.clone(),
        domains_changed: Some(reload),
    };
    if store.backend() == Backend::Postgres {
        background.push(tokio::spawn(cluster.clone().listen(
            config.database_url.expose().to_owned(),
            handlers,
            shutdown.clone(),
        )));
    }

    let services = Services {
        scheduler: waker,
        domains,
        verifier,
    };
    let state = app_state(config, store, events, prober, services)?.with_cluster(cluster);
    let served = match public {
        Some(public) => serve(state, public, internal, shutdown.clone()).await,
        None => serve_internal(state, internal, shutdown.clone()).await,
    };
    shutdown.cancel();
    for task in background {
        task.await.context("background task failed")?;
    }
    served
}

/// A worker-only instance: health and readiness for the platform.
async fn serve_internal(
    state: AppState,
    internal: TcpListener,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    tracing::info!(internal = %internal.local_addr()?, "serving (internal only)");
    axum::serve(internal, uptime_web::internal_router(state))
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
        .context("HTTP server failed")
}

/// Serves the public and internal routers on already-bound listeners.
pub async fn serve(
    state: AppState,
    public: TcpListener,
    internal: TcpListener,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    tracing::info!(public = %public.local_addr()?, internal = %internal.local_addr()?, "serving");
    let public_app = uptime_web::public_router(state.clone())
        .into_make_service_with_connect_info::<SocketAddr>();
    let internal_app = uptime_web::internal_router(state);
    let public =
        axum::serve(public, public_app).with_graceful_shutdown(shutdown.clone().cancelled_owned());
    let internal =
        axum::serve(internal, internal_app).with_graceful_shutdown(shutdown.cancelled_owned());
    tokio::try_join!(public, internal).context("HTTP server failed")?;
    tracing::info!("server stopped");
    Ok(())
}

async fn bind(addr: SocketAddr) -> anyhow::Result<TcpListener> {
    TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))
}

/// Cancels `token` on Ctrl+C or SIGTERM (what container platforms send on deploy).
pub fn cancel_on_signal(token: CancellationToken) {
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut terminate = match tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::terminate(),
            ) {
                Ok(signal) => signal,
                Err(error) => {
                    tracing::error!(error = %uptime_domain::Report(&error), "cannot listen for SIGTERM");
                    let _ = ctrl_c.await;
                    token.cancel();
                    return;
                }
            };
            tokio::select! {
                _ = ctrl_c => {}
                _ = terminate.recv() => {}
            }
        }
        #[cfg(not(unix))]
        let _ = ctrl_c.await;
        tracing::info!("shutdown signal received");
        token.cancel();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instances_on_one_host_get_different_names() {
        let names: std::collections::HashSet<_> = (0..50).map(|_| instance_id()).collect();
        assert_eq!(names.len(), 50);
    }
}
