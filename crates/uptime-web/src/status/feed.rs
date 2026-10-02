//! `/s/{slug}/feed.atom`: recent incidents and maintenance as an Atom feed,
//! for feed readers and chat integrations. Like the page, it carries only
//! what visitors may see.

use std::fmt::Write as _;

use jiff::Timestamp;
use topcoat::{
    Result,
    context::Cx,
    router::{
        HeaderValue,
        error::not_found,
        header, path_param,
        request::headers,
        response::{IntoResponse as _, Response},
        route,
    },
};

use super::{IncidentView, MaintenanceView, Slug, StatusView, base, load};
use crate::host::normalize_host;

/// One feed entry.
struct Entry {
    id: String,
    title: String,
    link: String,
    updated: Timestamp,
    published: Timestamp,
    /// Plain text, one paragraph per line.
    body: String,
}

/// Recent incidents (with every update) and maintenance windows.
#[route(GET "/s/{slug}/feed.atom")]
pub(crate) async fn status_feed(cx: &Cx) -> Result<Response> {
    let view = load(cx, path_param::<Slug>(cx)).await?;
    if !view.published {
        return Err(not_found().into());
    }
    let host = headers(cx)
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(normalize_host)
        .ok_or_else(not_found)?;
    let scheme = if cfg!(debug_assertions) && host.starts_with("localhost") {
        "http"
    } else {
        "https"
    };
    let origin = format!("{scheme}://{host}");
    let base = base(cx, &view.slug);
    let xml = render(&view, &origin, &base);
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/atom+xml; charset=utf-8"),
            ),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=60"),
            ),
        ],
        xml,
    )
        .into_response(cx)
}

fn render(view: &StatusView, origin: &str, base: &str) -> String {
    let home = format!("{origin}{}", if base.is_empty() { "/" } else { base });
    let mut entries: Vec<Entry> = view
        .incidents
        .iter()
        .map(|i| incident_entry(i, origin, base))
        .chain(
            view.maintenances
                .iter()
                .map(|m| maintenance_entry(m, &home)),
        )
        .collect();
    entries.sort_by_key(|e| std::cmp::Reverse(e.updated));
    let updated = entries.first().map_or(view.updated_at, |e| e.updated);
    let mut out = String::from(r#"<?xml version="1.0" encoding="utf-8"?>"#);
    out.push_str(r#"<feed xmlns="http://www.w3.org/2005/Atom">"#);
    let _ = write!(
        out,
        r#"<id>{home}</id><title>{title}</title><updated>{updated}</updated><link rel="alternate" href="{home}"/><link rel="self" href="{feed}"/>"#,
        home = escape(&home),
        title = escape(&format!("{} status", view.title)),
        updated = updated,
        feed = escape(&format!("{}/feed.atom", home.trim_end_matches('/'))),
    );
    for e in &entries {
        let _ = write!(
            out,
            r#"<entry><id>{id}</id><title>{title}</title><link rel="alternate" href="{link}"/><published>{published}</published><updated>{updated}</updated><content type="html">{content}</content></entry>"#,
            id = escape(&e.id),
            title = escape(&e.title),
            link = escape(&e.link),
            published = e.published,
            updated = e.updated,
            content = escape(&html_paragraphs(&e.body)),
        );
    }
    out.push_str("</feed>");
    out
}

fn incident_entry(i: &IncidentView, origin: &str, base: &str) -> Entry {
    let link = format!("{origin}{base}/incidents/{}", i.id);
    let updated = i
        .updates
        .iter()
        .map(|(_, _, at)| *at)
        .chain(i.resolved_at)
        .max()
        .unwrap_or(i.started_at);
    let body = i
        .updates
        .iter()
        .map(|(status, text, at)| format!("{} ({at}): {text}", status.label()))
        .collect::<Vec<_>>()
        .join("\n");
    Entry {
        id: link.clone(),
        title: format!("[{}] {}", i.status.label(), i.title),
        link,
        updated,
        published: i.started_at,
        body,
    }
}

fn maintenance_entry(m: &MaintenanceView, home: &str) -> Entry {
    let mut body = format!("From {} to {}.", m.starts_at, m.ends_at);
    if let Some(description) = &m.description {
        body.push('\n');
        body.push_str(description);
    }
    Entry {
        id: format!(
            "{}#maintenance-{}",
            home.trim_end_matches('/'),
            m.starts_at.as_second()
        ),
        title: format!("Maintenance: {}", m.title),
        link: format!("{}/maintenance", home.trim_end_matches('/')),
        updated: m.starts_at,
        published: m.starts_at,
        body,
    }
}

/// Text lines as escaped HTML paragraphs (the feed's `html` content).
fn html_paragraphs(text: &str) -> String {
    text.lines()
        .map(|line| format!("<p>{}</p>", escape(line)))
        .collect()
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c if c.is_control() && !matches!(c, '\n' | '\t') => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_neutralises_markup_and_control_characters() {
        assert_eq!(
            escape("<a href=\"x\">&\u{0}"),
            "&lt;a href=&quot;x&quot;&gt;&amp;"
        );
    }
}
