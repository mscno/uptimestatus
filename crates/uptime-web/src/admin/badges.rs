//! README badge snippets for a page and its components, with previews.

use topcoat::{
    Result,
    view::{View, component, view},
};
use uptime_domain::{ComponentSpec, MonitorKey};
use uptime_store::{DomainStatus, Page};

use crate::AppState;

/// One badge: what it shows, its preview (on this host) and copyable snippets.
pub(crate) struct Badge {
    what: String,
    preview: String,
    markdown: String,
    html: String,
}

/// Where visitors reach a page: its verified custom domain, else `/s/{slug}`
/// on the app. Also returns the page's path prefix there.
pub(super) async fn public_base(state: &AppState, page: &Page) -> Result<String> {
    let domain = state
        .store
        .domains_for_page(page.id)
        .await?
        .into_iter()
        .find(|domain| domain.status == DomainStatus::Verified);
    Ok(match domain {
        Some(domain) => format!("https://{}", domain.hostname),
        None => format!(
            "{}/s/{}",
            state.hosts.app_url().as_str().trim_end_matches('/'),
            page.spec.slug
        ),
    })
}

fn badge(base: &str, local: &str, path: &str, what: String, alt: &str) -> Badge {
    let url = format!("{base}/{path}");
    let alt = alt.replace(['[', ']', '"', '<', '>'], "");
    Badge {
        what,
        preview: format!("{local}/{path}"),
        markdown: format!("[![{alt}]({url})]({base})"),
        html: format!(r#"<a href="{base}"><img src="{url}" alt="{alt}"></a>"#),
    }
}

/// The badges for one component of `page`.
fn component_badges(base: &str, local: &str, component: &ComponentSpec, name: &str) -> Vec<Badge> {
    let key = component.key();
    let mut badges = vec![
        badge(
            base,
            local,
            &format!("badge/{key}/status.svg"),
            format!("{name}: status"),
            &format!("{name} status"),
        ),
        badge(
            base,
            local,
            &format!("badge/{key}/uptime.svg?window=30d"),
            format!("{name}: uptime (30 days; or 24h, 7d, 90d)"),
            &format!("{name} uptime"),
        ),
    ];
    if component.monitor_key().is_some() {
        badges.push(badge(
            base,
            local,
            &format!("badge/{key}/response.svg"),
            format!("{name}: median response time (24 hours)"),
            &format!("{name} response time"),
        ));
    }
    badges
}

/// Every badge of a page: its status, then each component's.
pub(crate) async fn page_badges(
    state: &AppState,
    page: &Page,
    name_of: impl Fn(&ComponentSpec) -> String,
) -> Result<Vec<Badge>> {
    let base = public_base(state, page).await?;
    let local = format!("/s/{}", page.spec.slug);
    let mut badges = vec![badge(
        &base,
        &local,
        "badge.svg",
        format!("{}: overall status", page.spec.title),
        &format!("{} status", page.spec.title),
    )];
    for component in page.spec.components() {
        badges.extend(component_badges(
            &base,
            &local,
            component,
            &name_of(component),
        ));
    }
    Ok(badges)
}

/// A monitor's badges on each page that shows it.
pub(crate) async fn monitor_badges(
    state: &AppState,
    monitor: &MonitorKey,
    name: &str,
) -> Result<Vec<Badge>> {
    let mut badges = Vec::new();
    for summary in state.store.list_pages().await? {
        let Some(page) = state.store.page(summary.id).await? else {
            continue;
        };
        let Some(component) = page
            .spec
            .components()
            .find(|c| c.monitor_key() == Some(monitor))
        else {
            continue;
        };
        let base = public_base(state, &page).await?;
        let local = format!("/s/{}", page.spec.slug);
        let name = component.label().unwrap_or(name).to_owned();
        for mut badge in component_badges(&base, &local, component, &name) {
            badge.what = format!("{} · {}", page.spec.title, badge.what);
            badges.push(badge);
        }
    }
    Ok(badges)
}

/// Badges with previews and copy buttons.
#[component]
pub(crate) async fn badges_section(
    badges: &[Badge],
    /// Shown when there are none.
    empty: &str,
) -> Result<impl View> {
    Ok(view! {
        <div class="card" id="badges">
            <h2>"Badges"</h2>
            if badges.is_empty() {
                <p class="muted small">(empty)</p>
            } else {
                <p class="small muted">"For READMEs and docs. Badges follow the page: drafts show them only to signed-in admins. Add "
                    <code>"?style=flat-square"</code> " or " <code>"for-the-badge"</code> ", or " <code>"&label=…"</code> "."</p>
                <ul class="badges">
                    for badge in badges {
                        <li>
                            <div class="row">
                                <img src=(badge.preview.as_str()) alt="" loading="lazy">
                                <span class="small">(badge.what.as_str())</span>
                            </div>
                            <div class="row small">
                                <code class="snippet">(badge.markdown.as_str())</code>
                                <sb-copy-button value=(badge.markdown.as_str()) label="Copy Markdown"></sb-copy-button>
                                <sb-copy-button value=(badge.html.as_str()) label="Copy HTML"></sb-copy-button>
                            </div>
                        </li>
                    }
                </ul>
            }
        </div>
    })
}
