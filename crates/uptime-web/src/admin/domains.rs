//! Custom domains for a status page: add, verify (DNS), remove.
//!
//! Every action answers with a Datastar patch of the `#domains` section.

use jiff::Timestamp;
use serde::Deserialize;
use topcoat::{
    Result,
    context::Cx,
    datastar::PatchElements,
    router::{content::Form, error::not_found, path_param, route},
    view::{View, ViewExt as _, component, view},
};
use uptime_domain::{Edge, Hostname};
use uptime_store::{CustomDomain, DomainStatus, StoreError};

use super::Id;
use crate::{auth::app, auth::require_admin, fmt};

path_param!(domain: i64, error = not_found);

/// A message shown above the domain list.
#[derive(Clone, Debug, Default)]
pub(crate) struct Notice {
    error: bool,
    text: String,
}

impl Notice {
    fn error(text: impl Into<String>) -> Self {
        Self {
            error: true,
            text: text.into(),
        }
    }

    fn info(text: impl Into<String>) -> Self {
        Self {
            error: false,
            text: text.into(),
        }
    }
}

fn status_pill(domain: &CustomDomain) -> (&'static str, &'static str) {
    match domain.status {
        DomainStatus::Pending => ("pill pill-pending", "Pending"),
        DomainStatus::Verified => ("pill pill-up", "Verified"),
        DomainStatus::Failed => ("pill pill-down", "Not pointing here"),
    }
}

fn detail(domain: &CustomDomain) -> String {
    match (domain.status, domain.cert_ok_at, &domain.last_error) {
        (DomainStatus::Pending, _, _) => "Waiting for the first DNS check.".into(),
        (DomainStatus::Failed, _, Some(error)) => error.clone(),
        (DomainStatus::Failed, _, None) => "DNS does not point at the edge.".into(),
        (DomainStatus::Verified, Some(_), _) => "Serving over HTTPS.".into(),
        (DomainStatus::Verified, None, Some(error)) => format!("Certificate: {error}"),
        (DomainStatus::Verified, None, None) => {
            "DNS is right; HTTPS works once a certificate is added for this hostname.".into()
        }
    }
}

/// The custom-domain section of the page editor.
#[component]
pub(crate) async fn domains_section(
    page_id: i64,
    domains: &[CustomDomain],
    edge: &Edge,
    #[default] notice: Option<Notice>,
    #[default] value: &str,
) -> Result<impl View> {
    let base = format!("/admin/pages/{page_id}/domains");
    let add = format!("@post('{base}', {{contentType: 'form'}})");
    let now = Timestamp::now();
    Ok(view! {
        <section id="domains" class="card stack">
            <div>
                <h2>"Custom domains"</h2>
                <p class="muted small">"Serve this page at the root of your own hostname, e.g. status.yourcompany.com. Requests for a verified hostname show this page; nothing else is reachable on it."</p>
            </div>
            if let Some(notice) = &notice {
                <p class=(if notice.error { "flash flash-error" } else { "flash" }) role="status">(notice.text.as_str())</p>
            }
            if !domains.is_empty() {
                <div class="table-wrap">
                    <table class="list">
                        <thead><tr><th>"Hostname"</th><th>"Status"</th><th>"Details"</th><th>"Checked"</th><th></th></tr></thead>
                        <tbody>
                            for domain in domains {
                                <tr id=(format!("domain-{}", domain.id))>
                                    <td class="mono">
                                        if domain.status == DomainStatus::Verified {
                                            <a href=(format!("https://{}/", domain.hostname))>(domain.hostname.as_str())</a>
                                        } else {
                                            (domain.hostname.as_str())
                                        }
                                    </td>
                                    <td><span class=(status_pill(domain).0)>(status_pill(domain).1)</span></td>
                                    <td class="small">(detail(domain))</td>
                                    <td class="small muted">(domain.last_checked_at.map_or("never".into(), |at| fmt::relative(at, now)))</td>
                                    <td class="actions-cell">
                                        <button class="btn" type="button"
                                            data-indicator=(format!("_verifying{}", domain.id))
                                            data-attr:aria-busy=(format!("$_verifying{}", domain.id))
                                            data-on:click=(format!("@post('{base}/{}/verify')", domain.id))>"Verify"</button>
                                        <button class="btn btn-link" type="button"
                                            data-on:click=(format!("confirm('Remove {}?') && @post('{base}/{}/delete')", domain.hostname, domain.id))>"Remove"</button>
                                    </td>
                                </tr>
                            }
                        </tbody>
                    </table>
                </div>
            }
            <form class="row" data-on:submit__prevent=(add)>
                <input type="text" name="hostname" value=(value) placeholder="status.yourcompany.com" aria-label="Hostname" autocomplete="off">
                <button class="btn btn-primary" type="submit">"Add domain"</button>
            </form>
            <details class="small">
                <summary>"How to point DNS"</summary>
                <p>"Add a CNAME record for your hostname:"</p>
                <pre class="dns">(format!("status.yourcompany.com.  CNAME  {}.", edge.host))</pre>
                if edge.addresses.is_empty() {
                    <p class="muted">"Apex domains (without a subdomain) cannot use a CNAME; use a subdomain."</p>
                } else {
                    <p>"For an apex domain, which cannot have a CNAME, add these records instead:"</p>
                    <pre class="dns">
                        for address in &edge.addresses {
                            (format!("yourcompany.com.  {}  {address}\n", if address.is_ipv4() { "A   " } else { "AAAA" }))
                        }
                    </pre>
                }
                <p class="muted">"Then press Verify (unverified domains are also rechecked every few minutes). HTTPS needs a certificate for the hostname, which whoever runs this server issues (through the proxy or platform in front of it). A CNAME to a name under "
                    (edge.host.as_str())
                    " verifies too."</p>
            </details>
        </section>
    })
}

fn page_id(cx: &Cx) -> Result<i64> {
    Ok(*path_param::<Id>(cx)?)
}

/// Re-renders the section for `page_id`.
async fn patch(
    cx: &Cx,
    page_id: i64,
    notice: Option<Notice>,
    value: &str,
) -> Result<PatchElements> {
    let state = app(cx);
    let domains = state.store.domains_for_page(page_id).await?;
    let edge = state.edge.clone();
    let html = view! { cx =>
        domains_section(page_id: page_id, domains: &domains, edge: &edge, notice: notice, value: value)
    }
    .single()
    .await?
    .render(cx);
    Ok(PatchElements::new(html))
}

/// The domain named in the path, if it belongs to the page in the path.
async fn owned_domain(cx: &Cx) -> Result<CustomDomain> {
    let page_id = page_id(cx)?;
    let id = *path_param::<Domain>(cx)?;
    match app(cx).store.domain(id).await? {
        Some(domain) if domain.page_id == page_id => Ok(domain),
        _ => Err(not_found().into()),
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct DomainForm {
    #[serde(default)]
    hostname: String,
}

#[route(POST "/admin/pages/{id}/domains")]
pub(crate) async fn add_domain(cx: &Cx, Form(input): Form<DomainForm>) -> Result<PatchElements> {
    let admin = require_admin(cx).await?;
    let page_id = page_id(cx)?;
    let state = app(cx);
    let hostname: Hostname = match input.hostname.parse() {
        Ok(hostname) => hostname,
        Err(error) => {
            return patch(
                cx,
                page_id,
                Some(Notice::error(error.to_string())),
                &input.hostname,
            )
            .await;
        }
    };
    let reserved = [state.hosts.app_host(), state.hosts.edge_host()];
    if reserved.contains(&hostname.as_str()) {
        let notice = Notice::error(format!("`{hostname}` is reserved for uptimestatus itself."));
        return patch(cx, page_id, Some(notice), &input.hostname).await;
    }
    match state
        .store
        .add_domain(page_id, &hostname, Timestamp::now())
        .await
    {
        Ok(_) => {
            tracing::info!(page = page_id, domain = %hostname, by = %admin.login, "custom domain added");
            let notice = Notice::info(format!(
                "Added {hostname}. Point its DNS at {}, then press Verify.",
                state.edge.host
            ));
            patch(cx, page_id, Some(notice), "").await
        }
        Err(error @ StoreError::DuplicateDomain(_)) => {
            patch(
                cx,
                page_id,
                Some(Notice::error(error.to_string())),
                &input.hostname,
            )
            .await
        }
        Err(StoreError::PageNotFound(_)) => Err(not_found().into()),
        Err(error) => Err(error.into()),
    }
}

#[route(POST "/admin/pages/{id}/domains/{domain}/verify")]
pub(crate) async fn verify_domain(cx: &Cx) -> Result<PatchElements> {
    let admin = require_admin(cx).await?;
    let domain = owned_domain(cx).await?;
    let Some(verifier) = app(cx).verifier.clone() else {
        let notice = Notice::error("Domain verification is not running on this server.");
        return patch(cx, domain.page_id, Some(notice), "").await;
    };
    let checked = verifier.verify(domain.id).await?;
    tracing::info!(domain = %checked.hostname, status = %checked.status, by = %admin.login, "custom domain verified on request");
    let notice =
        match checked.status {
            DomainStatus::Verified => Notice::info(format!(
                "{} points at the edge and is now served.",
                checked.hostname
            )),
            _ => Notice::error(checked.last_error.clone().unwrap_or_else(|| {
                format!("{} is not pointing at the edge yet.", checked.hostname)
            })),
        };
    patch(cx, domain.page_id, Some(notice), "").await
}

#[route(POST "/admin/pages/{id}/domains/{domain}/delete")]
pub(crate) async fn remove_domain(cx: &Cx) -> Result<PatchElements> {
    let admin = require_admin(cx).await?;
    let domain = owned_domain(cx).await?;
    let state = app(cx);
    state.store.remove_domain(domain.id).await?;
    if domain.status == DomainStatus::Verified {
        state.domains.reload(&state.store).await?;
        state.domains_changed();
    }
    tracing::info!(domain = %domain.hostname, by = %admin.login, "custom domain removed");
    patch(
        cx,
        domain.page_id,
        Some(Notice::info(format!("Removed {}.", domain.hostname))),
        "",
    )
    .await
}
