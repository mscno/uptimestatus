//! A status page's monthly uptime reports: the list of months, each month
//! as a web page, and the same report as PDF.
//!
//! Public reports are open to anyone when the page is published; private
//! ones (and drafts) only to signed-in admins and report links (`?key=`),
//! which may be limited to one month and expire.

use std::{sync::Arc, time::Duration};

use jiff::Timestamp;
use topcoat::{
    Result,
    context::Cx,
    router::{
        StatusCode,
        content::Html,
        error::not_found,
        header::{self, HeaderValue},
        page, path_param, query_params,
        response::{IntoResponse as _, Response},
        route,
    },
    view::{View, ViewExt as _, component, view},
};
use uptime_domain::Month;

use super::{Slug, StatusView, Tab, base, frame, load_draft};
use crate::{
    auth::{app, current_admin},
    reports::{
        Access, Report, hash_link_key, months,
        pdf::{self, Logo},
    },
};

/// The longest a PDF may take to typeset.
const RENDER_TIMEOUT: Duration = Duration::from_secs(30);

path_param!(file);

/// `?key=`: a report link's key.
#[query_params]
pub(crate) struct KeyQuery {
    key: Option<String>,
}

/// Who is asking, if they may see this page's reports at all.
async fn access(cx: &Cx, view: &StatusView) -> Result<Option<Access>> {
    if current_admin(cx).await?.is_some() {
        return Ok(Some(Access::Admin));
    }
    let key = query_params::<KeyQuery>(cx)
        .ok()
        .and_then(|q| q.key.clone())
        .filter(|k| !k.trim().is_empty());
    if let Some(key) = key {
        let link = app(cx)
            .store
            .report_link_by_hash(&hash_link_key(&key))
            .await?
            .filter(|link| link.page_id == view.page_id);
        return Ok(link.map(Access::Link));
    }
    Ok((view.published && view.reports).then_some(Access::Public))
}

/// `?key=…` to carry a link's key along, or nothing.
fn key_suffix(cx: &Cx, access: &Access) -> String {
    match access {
        Access::Link(_) => query_params::<KeyQuery>(cx)
            .ok()
            .and_then(|q| q.key.clone())
            .map(|key| format!("?key={}", url_escape(key.trim())))
            .unwrap_or_default(),
        Access::Public | Access::Admin => String::new(),
    }
}

/// Keys are base64url; anything else is dropped rather than escaped.
fn url_escape(key: &str) -> String {
    key.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect()
}

fn cache_control(access: &Access) -> &'static str {
    match access {
        Access::Public => "public, max-age=300",
        Access::Admin | Access::Link(_) => "private, no-store",
    }
}

#[page("/s/{slug}/reports")]
pub(crate) async fn reports_page(cx: &Cx) -> Result<impl View> {
    let view = load_draft(cx, path_param::<Slug>(cx)).await?;
    let now = Timestamp::now();
    let access = access(cx, &view)
        .await?
        .filter(|a| a.lists_months(now))
        .ok_or_else(not_found)?;
    let state = app(cx);
    let page = state
        .store
        .page(view.page_id)
        .await?
        .ok_or_else(not_found)?;
    let months = months(&state.store, &page, now).await?;
    let base = base(cx, &view.slug);
    let key = key_suffix(cx, &access);
    let rows: Vec<(String, String, String, bool)> = months
        .iter()
        .map(|month| {
            (
                month.label(),
                format!("{base}/reports/{month}{key}"),
                format!("{base}/reports/{month}.pdf{key}"),
                month.contains(now),
            )
        })
        .collect();
    let title = format!("Reports · {}", view.title);
    Ok(view! {
        ((header::CACHE_CONTROL, HeaderValue::from_static(cache_control(&access))))
        frame(view: &view, base: &base, tab: Tab::Reports, title: &title,
            <div class="history">
                <section class="month-card">
                    <h3>"Monthly uptime reports"</h3>
                    if rows.is_empty() {
                        <p class="empty">"No history yet."</p>
                    }
                    <ul class="report-list">
                        for (label, html, pdf, to_date) in &rows {
                            <li>
                                <a href=(html.as_str())>(label.as_str())</a>
                                if *to_date { <span class="small muted">" (month to date)"</span> }
                                <a class="small" href=(pdf.as_str())>"PDF"</a>
                            </li>
                        }
                    </ul>
                </section>
            </div>
        )
    })
}

/// `2026-09` (the web page) or `2026-09.pdf`.
#[route(GET "/s/{slug}/reports/{file}")]
pub(crate) async fn month_report(cx: &Cx) -> Result<Response> {
    let view = load_draft(cx, path_param::<Slug>(cx)).await?;
    let file = path_param::<File>(cx).to_owned();
    let (month, as_pdf) = match file.strip_suffix(".pdf") {
        Some(month) => (month, true),
        None => (file.as_str(), false),
    };
    let month: Month = month.parse().map_err(|_| not_found())?;
    let now = Timestamp::now();
    if month > Month::of(now) {
        return Err(not_found().into());
    }
    let access = access(cx, &view)
        .await?
        .filter(|a| a.opens(month, now))
        .ok_or_else(not_found)?;
    let state = app(cx);
    let page = state
        .store
        .page(view.page_id)
        .await?
        .ok_or_else(not_found)?;
    let settings = state.store.report_settings(page.id).await?;
    let report = Report::build(&state.store, &page, &settings, month, now).await?;
    if as_pdf {
        return pdf_response(cx, &view, &access, &report, page.logo.as_deref()).await;
    }
    let base = base(cx, &view.slug);
    let key = key_suffix(cx, &access);
    let pdf = format!("{base}/reports/{month}.pdf{key}");
    let list = access
        .lists_months(now)
        .then(|| format!("{base}/reports{key}"));
    let title = format!("{} · {}", report.label, view.title);
    let html = view! { cx =>
        frame(view: &view, base: &base, tab: Tab::Reports, title: &title,
            report_body(report: &report, pdf: &pdf, list: list.as_deref())
        )
    }
    .single()
    .await?
    .render(cx);
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, cache_control(&access))],
        Html(html),
    )
        .into_response(cx)
}

async fn pdf_response(
    cx: &Cx,
    view: &StatusView,
    access: &Access,
    report: &Report,
    logo: Option<&str>,
) -> Result<Response> {
    let state = app(cx);
    let logo = match (logo, &state.media) {
        (Some(name), Some(media)) => media
            .read(name)
            .await
            .and_then(|bytes| Logo::supported(name, bytes)),
        _ => None,
    };
    let fingerprint = report.fingerprint(logo.as_ref().map(|l| l.bytes.as_slice()));
    let pdf = match state.reports.get(&fingerprint) {
        Some(pdf) => pdf,
        None => {
            let report = report.clone();
            let rendering =
                tokio::task::spawn_blocking(move || pdf::render(&report, logo.as_ref()));
            let pdf = tokio::time::timeout(RENDER_TIMEOUT, rendering)
                .await
                .map_err(|_| std::io::Error::other("typesetting the report timed out"))?
                .map_err(std::io::Error::other)?
                .map_err(std::io::Error::other)?;
            let pdf = Arc::new(pdf);
            state.reports.put(fingerprint, pdf.clone());
            pdf
        }
    };
    let disposition = format!(
        "inline; filename=\"{}-uptime-{}.pdf\"",
        view.slug, report.month
    );
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/pdf".to_owned()),
            (header::CACHE_CONTROL, cache_control(access).to_owned()),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        pdf.as_ref().clone(),
    )
        .into_response(cx)
}

fn bar_class(level: &str) -> &'static str {
    match level {
        "up" => "bar-up",
        "warn" => "bar-warn",
        "down" => "bar-down",
        "future" => "bar-future",
        _ => "bar-none",
    }
}

#[component]
async fn report_body(report: &Report, pdf: &str, list: Option<&str>) -> Result<impl View> {
    let verdict = |met: Option<bool>| match met {
        Some(true) => ("pill pill-up", "Met"),
        Some(false) => ("pill pill-down", "Missed"),
        None => ("", ""),
    };
    let (overall_pill, overall_verdict) = verdict(report.overall_met);
    let components: Vec<_> = report
        .sections
        .iter()
        .map(|section| {
            let rows: Vec<_> = section
                .components
                .iter()
                .map(|c| {
                    let (pill, label) = verdict(c.met);
                    let bars: Vec<&'static str> = c.bars.iter().map(|b| bar_class(b)).collect();
                    (c, pill, label, bars)
                })
                .collect();
            (section.name.clone(), rows)
        })
        .collect();
    Ok(view! {
        <div class="history report">
            <nav class="report-actions">
                if let Some(list) = list { <a href=(list.to_owned())>"← All reports"</a> }
                <span class="spacer"></span>
                <a class="btn" href=(pdf.to_owned())>"Download PDF"</a>
            </nav>
            <section class="month-card">
                <h3>"Uptime report · " (report.label.as_str())
                    if report.to_date { <span class="small muted">" (month to date)"</span> }
                </h3>
                <div class="report-summary">
                    <div><span class="label">"Overall uptime"</span><strong>(report.overall.clone().unwrap_or_else(|| "—".into()))</strong></div>
                    <div>
                        <span class="label">"Uptime target"</span>
                        <strong>(report.target.clone().unwrap_or_else(|| "None set".into()))</strong>
                        if !overall_verdict.is_empty() { " " <span class=(overall_pill)>(overall_verdict)</span> }
                    </div>
                    <div><span class="label">"Incidents"</span><strong>(report.incidents.len().to_string())</strong></div>
                </div>
                if report.excused > 0 {
                    <p class="small muted">(format!("{} failed checks were excluded from uptime by an administrator this month (recorded in the audit trail).", report.excused))</p>
                }
            </section>
            <section class="month-card">
                <h3>"Components"</h3>
                for (section, rows) in &components {
                    <h4 class="history-day">(section.as_str())</h4>
                    for (component, pill, label, bars) in rows {
                        <article class="report-component">
                            <header>
                                <strong>(component.name.as_str())</strong>
                                <span class="mono">(component.uptime.clone().unwrap_or_else(|| "—".into()))</span>
                                if !label.is_empty() { <span class=(*pill)>(*label)</span> }
                            </header>
                            <div class="bars report-bars" role="img" aria-label=(format!("{} by day", component.name))>
                                for bar in bars { <span class=(*bar)></span> }
                            </div>
                            <p class="small muted">"Downtime " (component.downtime.as_str()) " · maintenance " (component.maintenance.as_str())</p>
                        </article>
                    }
                }
            </section>
            <section class="month-card">
                <h3>"Incidents"</h3>
                if report.incidents.is_empty() {
                    <p class="empty">"No incidents this month."</p>
                }
                for incident in &report.incidents {
                    <article class="history-item">
                        <header>
                            <strong class="history-title">(incident.title.as_str())</strong>
                            <span class="small muted">(incident.impact.as_str())</span>
                        </header>
                        <p class="small muted">(incident.started.as_str()) " · " (incident.duration.as_str())
                            if !incident.components.is_empty() { " · " (incident.components.join(", ")) }
                        </p>
                    </article>
                }
            </section>
            if !report.maintenance.is_empty() {
                <section class="month-card">
                    <h3>"Maintenance"</h3>
                    for window in &report.maintenance {
                        <p><strong>(window.title.as_str())</strong> <span class="small muted">(window.starts.as_str()) " → " (window.ends.as_str())</span></p>
                    }
                </section>
            }
            <section class="month-card">
                <h3>"How uptime is measured"</h3>
                for line in &report.methodology {
                    <p class="small muted">(line.as_str())</p>
                }
                <p class="small muted">"Generated " (report.generated.as_str())</p>
            </section>
        </div>
    })
}
