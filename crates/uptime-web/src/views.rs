//! Shared page chrome and small components.

use topcoat::{
    Result,
    context::Cx,
    cookie::{Cookies as _, cookies},
    view::{Child, Unescaped, View, component, view},
};
use uptime_domain::{Health, MonitorState};
use uptime_store::AdminUser;

use crate::assets::{self, CSS_HREF, DATASTAR_SRC, IMPORT_MAP};

/// Starbase components the admin console uses.
pub(crate) const ADMIN_MODULES: &[&str] = &[
    "alert",
    "button",
    "code-editor",
    "copy-button",
    "modal",
    "sparkline",
    "theme-switch",
    "toggle",
    "tooltip",
];

/// The cookie `sb-theme-switch` stores the chosen theme in.
const THEME_COOKIE: &str = "sb-theme";

/// The Starbase theme to render: a page's own setting (`light`/`dark`), else
/// the visitor's choice from the theme switch, else none ("auto": follow the
/// system, dark by default).
fn starbase_theme(cx: &Cx, page_theme: &str) -> Option<&'static str> {
    match page_theme {
        "light" => Some("daylight"),
        "dark" => Some("deep-space"),
        _ => match cookies(cx).get(THEME_COOKIE).as_ref().map(|c| c.value()) {
            Some("daylight") => Some("daylight"),
            Some("deep-space") => Some("deep-space"),
            _ => None,
        },
    }
}

/// A complete HTML document.
///
/// `modules` are Starbase components to load (see [`ADMIN_MODULES`]); they
/// import `datastar` through the import map, i.e. the page's own bundle.
#[component]
pub(crate) async fn document(
    cx: &Cx,
    title: &str,
    #[default] theme: &str,
    #[default] accent: &str,
    /// A status page's look (`clean`); empty for the default 8-bit look.
    #[default]
    look: &str,
    /// Favicon URL; our logo when empty.
    #[default]
    icon: &str,
    #[default] modules: &[&str],
    /// An Atom feed to advertise to feed readers.
    #[default]
    feed: &str,
    #[default] child: Child<'_>,
) -> Result<impl View> {
    let theme = starbase_theme(cx, theme);
    let accent = (!accent.is_empty()).then(|| {
        format!("--accent: {accent}; --sb-brand: {accent}; --sb-brand-light: {accent}; --sb-frame-color: {accent}")
    });
    let clean = look == "clean";
    let modules: Vec<String> = modules.iter().map(|m| assets::component(m)).collect();
    Ok(view! {
        <!DOCTYPE html>
        <html lang="en" class="sb-cloak" data-sb-theme=(theme) data-sb-style=(clean.then_some("smooth"))
            data-look=(clean.then_some("clean")) style=(accent)>
            <head>
                <meta charset="utf-8">
                <meta name="viewport" content="width=device-width, initial-scale=1">
                <title>(title) " · uptimestatus"</title>
                if icon.is_empty() {
                    <link rel="icon" type="image/svg+xml" href=(assets::url("logo.svg"))>
                } else {
                    <link rel="icon" href=(icon)>
                }
                if !feed.is_empty() {
                    <link rel="alternate" type="application/atom+xml" title=(title) href=(feed)>
                }
                <link rel="preload" as="font" type="font/woff2" crossorigin="" href="/static/starbase/fonts/jetbrains-mono.woff2">
                <link rel="stylesheet" href=(CSS_HREF.as_str())>
                <script type="importmap">(Unescaped::new_unchecked(IMPORT_MAP.as_str()))</script>
                <script type="module" src=(DATASTAR_SRC.as_str())></script>
                for module in &modules {
                    <script type="module" src=(module.as_str())></script>
                }
            </head>
            <body>(child)</body>
        </html>
    })
}

/// The admin console frame: top bar with navigation and the signed-in user.
#[component]
pub(crate) async fn admin_shell(
    title: &str,
    admin: &AdminUser,
    #[default] section: &str,
    #[default] child: Child<'_>,
) -> Result<impl View> {
    let display = admin.name.clone().unwrap_or_else(|| admin.login.clone());
    Ok(view! {
        document(title: title, modules: ADMIN_MODULES,
            <header class="topbar">
                <div class="topbar-inner">
                    <a class="brand" href="/admin"><img src=(assets::url("logo.svg")) alt="">"uptimestatus"</a>
                    <nav class="nav">
                        <a href="/admin" aria-current=((section == "monitors").then_some("page"))>"Monitors"</a>
                        <a href="/admin/pages" aria-current=((section == "pages").then_some("page"))>"Status pages"</a>
                        <a href="/admin/incidents" aria-current=((section == "incidents").then_some("page"))>"Incidents"</a>
                        <a href="/admin/maintenance" aria-current=((section == "maintenance").then_some("page"))>"Maintenance"</a>
                        <a href="/admin/alerts" aria-current=((section == "alerts").then_some("page"))>"Alerts"</a>
                        <a href="/admin/api" aria-current=((section == "api").then_some("page"))>"API"</a>
                    </nav>
                    <span class="spacer"></span>
                    <div class="user">
                        <sb-theme-switch variant="menu" compact="" attribute="data-sb-theme"
                            themes=r#"["auto","deep-space","daylight"]"# labels=r#"["Auto","Dark","Light"]"#></sb-theme-switch>
                        <span title=(admin.login.as_str())>(display)</span>
                        <form method="post" action="/auth/logout">
                            <button class="btn btn-link" type="submit">"Sign out"</button>
                        </form>
                    </div>
                </div>
            </header>
            <main class="shell">(child)</main>
        )
    })
}

/// A monitor state as a colored pill with a text label.
#[component]
pub(crate) async fn state_pill(state: MonitorState) -> Result<impl View> {
    Ok(view! {
        <span class=(format!("pill pill-{}", state.as_str()))>(state_label(state))</span>
    })
}

/// A single check's health as a pill.
#[component]
pub(crate) async fn health_pill(health: Health) -> Result<impl View> {
    Ok(view! {
        <span class=(format!("pill pill-{}", health.as_str()))>(health_label(health))</span>
    })
}

pub(crate) fn state_label(state: MonitorState) -> &'static str {
    match state {
        MonitorState::Unknown => "Unknown",
        MonitorState::Up => "Up",
        MonitorState::Degraded => "Degraded",
        MonitorState::Pending => "Pending",
        MonitorState::Down => "Down",
        MonitorState::Maintenance => "Maintenance",
        MonitorState::Paused => "Paused",
    }
}

pub(crate) fn health_label(health: Health) -> &'static str {
    match health {
        Health::Up => "Up",
        Health::Degraded => "Degraded",
        Health::Down => "Down",
    }
}

/// A relative time ("just now", "5m ago"). Hovering shows the exact time in
/// the visitor's locale and time zone (via Datastar), or in UTC without it.
/// Live pages re-render it, which keeps the text current.
#[component]
pub(crate) async fn ago(at: jiff::Timestamp, now: jiff::Timestamp) -> Result<impl View> {
    let iso = at.to_string();
    let utc = at.strftime("%Y-%m-%d %H:%M:%S UTC").to_string();
    let local = format!(
        "new Date('{iso}').toLocaleString(undefined, {{year: 'numeric', month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit', second: '2-digit', timeZoneName: 'short'}})"
    );
    Ok(view! {
        <time class="ago" datetime=(iso) title=(utc) data-attr:title=(local)>(crate::fmt::relative(at, now))</time>
    })
}

/// A moment spelled in the visitor's locale and time zone ("Aug 5, 2026,
/// 12:18 AM GMT+2", via Datastar), or in UTC without it.
#[component]
pub(crate) async fn local_time(at: jiff::Timestamp) -> Result<impl View> {
    let iso = at.to_string();
    let local = format!(
        "new Date('{iso}').toLocaleString(undefined, {{year: 'numeric', month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit', timeZoneName: 'short'}})"
    );
    Ok(view! {
        <time datetime=(iso) data-text=(local)>(crate::fmt::when_dated(at))</time>
    })
}

/// A "Delete" button that asks first, in a modal, then posts to `action`.
#[component]
pub(crate) async fn delete_button(action: &str, heading: &str, body: &str) -> Result<impl View> {
    Ok(view! {
        <button class="btn btn-danger" type="button" data-on:click="el.nextElementSibling.show()">"Delete"</button>
        <sb-modal heading=(heading)>
            <p>(body)</p>
            <button slot="footer" class="btn" type="button" data-sb-close="cancel">"Cancel"</button>
            <form slot="footer" method="post" action=(action)>
                <button class="btn btn-danger" type="submit">"Delete"</button>
            </form>
        </sb-modal>
    })
}
