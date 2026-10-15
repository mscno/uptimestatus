//! Shields-style SVG badges for READMEs: a page's status, and a component's
//! status, uptime and response time. Only components on a page are exposed
//! (drafts only to admins); anything else is a 404, so badges reveal nothing
//! the page itself does not.
//!
//! Text widths come from a Verdana 11px width table (what shields.io uses), so
//! rendering needs no fonts or network.

use std::fmt::Write as _;

use topcoat::{
    Result,
    context::Cx,
    router::{
        StatusCode,
        error::not_found,
        path_param,
        response::{IntoResponse as _, Response},
        route,
    },
};
use uptime_domain::{ComponentStatus, MonitorState, PageStatus};

use super::{ComponentView, Slug, StatusView, cached, load};
use crate::fmt;

path_param!(key);

/// How a badge is drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Style {
    /// Rounded with a soft gradient (the shields default).
    #[default]
    Flat,
    /// Square corners, no gradient.
    FlatSquare,
    /// Taller, uppercase.
    ForTheBadge,
}

impl Style {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "" | "flat" => Some(Self::Flat),
            "flat-square" => Some(Self::FlatSquare),
            "for-the-badge" => Some(Self::ForTheBadge),
            _ => None,
        }
    }
}

/// Colors (the shields palette).
const GREEN: &str = "#4c1";
const YELLOW_GREEN: &str = "#a4a61d";
const YELLOW: &str = "#dfb317";
const ORANGE: &str = "#fe7d37";
const RED: &str = "#e05d44";
const BLUE: &str = "#007ec6";
const GREY: &str = "#9f9f9f";
const LABEL: &str = "#555";

/// Verdana 11px advance widths, in pixels.
fn char_width(c: char) -> f64 {
    match c {
        ' ' => 3.87,
        '!' | 'i' | 'l' | '|' => 3.02,
        '.' | ',' | ':' | ';' => 3.94,
        '\'' => 2.69,
        '"' => 4.6,
        '(' | ')' | '[' | ']' | '{' | '}' | '/' | '\\' | '-' => 4.99,
        '%' => 11.84,
        '&' => 7.6,
        '+' | '<' | '=' | '>' | '#' | '~' => 9.01,
        '_' => 6.99,
        '0'..='9' => 6.99,
        'f' => 3.86,
        'j' => 3.79,
        'r' => 4.69,
        't' => 4.33,
        'm' => 10.7,
        'w' => 8.98,
        'c' => 5.92,
        's' => 5.73,
        'z' => 5.77,
        'k' | 'v' | 'x' | 'y' => 6.51,
        'e' | 'o' => 6.68,
        'a' => 6.81,
        'b' | 'd' | 'g' | 'p' | 'q' => 6.86,
        'h' | 'n' | 'u' => 6.96,
        'I' => 4.61,
        'J' => 5.0,
        'L' => 6.12,
        'F' => 6.32,
        'P' => 6.63,
        'T' | 'Y' => 6.78,
        'E' => 6.95,
        'A' | 'B' | 'S' | 'V' | 'X' | 'Z' => 7.52,
        'K' | 'R' => 7.6,
        'C' => 7.68,
        'U' => 8.05,
        'N' => 8.23,
        'H' => 8.27,
        'D' => 8.48,
        'G' => 8.52,
        'O' | 'Q' => 8.66,
        'M' => 9.27,
        'W' => 10.88,
        _ => 7.0,
    }
}

fn text_width(text: &str) -> f64 {
    text.chars().map(char_width).sum()
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// A two-part badge: `label` on grey, `message` on `color`.
pub(crate) fn render(label: &str, message: &str, color: &str, style: Style) -> String {
    let (label, message) = match style {
        Style::ForTheBadge => (label.to_uppercase(), message.to_uppercase()),
        _ => (label.to_owned(), message.to_owned()),
    };
    // Uppercase badges are spaced out and a little bolder.
    let (height, pad, spacing) = match style {
        Style::ForTheBadge => (28.0, 9.0, 1.25),
        _ => (20.0, 5.0, 0.0),
    };
    let width_of = |text: &str| {
        let chars = text.chars().count() as f64;
        (text_width(text) + spacing * chars).round()
    };
    let label_width = if label.is_empty() {
        0.0
    } else {
        width_of(&label) + 2.0 * pad
    };
    let message_width = width_of(&message) + 2.0 * pad;
    let width = label_width + message_width;
    let title = if label.is_empty() {
        escape(&message)
    } else {
        format!("{}: {}", escape(&label), escape(&message))
    };
    let radius = match style {
        Style::Flat => 3.0,
        _ => 0.0,
    };
    let mut svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" role="img" aria-label="{title}"><title>{title}</title>"#
    );
    if style == Style::Flat {
        svg.push_str(r##"<linearGradient id="s" x2="0" y2="100%"><stop offset="0" stop-color="#bbb" stop-opacity=".1"/><stop offset="1" stop-opacity=".1"/></linearGradient>"##);
    }
    let _ = write!(
        svg,
        r##"<clipPath id="r"><rect width="{width}" height="{height}" rx="{radius}" fill="#fff"/></clipPath><g clip-path="url(#r)"><rect width="{label_width}" height="{height}" fill="{LABEL}"/><rect x="{label_width}" width="{message_width}" height="{height}" fill="{color}"/>"##
    );
    if style == Style::Flat {
        let _ = write!(
            svg,
            r#"<rect width="{width}" height="{height}" fill="url(#s)"/>"#
        );
    }
    svg.push_str("</g>");
    let (font_size, weight, baseline) = match style {
        Style::ForTheBadge => (10, "bold", 18.0),
        _ => (11, "normal", 14.0),
    };
    let _ = write!(
        svg,
        r##"<g fill="#fff" text-anchor="middle" font-family="Verdana,Geneva,DejaVu Sans,sans-serif" font-size="{font_size}" font-weight="{weight}" letter-spacing="{spacing}">"##
    );
    let mut text = |x: f64, content: &str| {
        if style != Style::ForTheBadge {
            // The shadow under the text.
            let _ = write!(
                svg,
                r##"<text x="{x}" y="{}" fill="#010101" fill-opacity=".3">{content}</text>"##,
                baseline + 1.0
            );
        }
        let _ = write!(svg, r#"<text x="{x}" y="{baseline}">{content}</text>"#);
    };
    if !label.is_empty() {
        text(label_width / 2.0, &escape(&label));
    }
    text(label_width + message_width / 2.0, &escape(&message));
    svg.push_str("</g></svg>");
    svg
}

/// A component's status as badge text and color.
fn status_message(component: &ComponentView) -> (&'static str, &'static str) {
    if let Some(status) = component.manual {
        return match status {
            ComponentStatus::Operational => ("operational", GREEN),
            ComponentStatus::Degraded => ("degraded", YELLOW),
            ComponentStatus::PartialOutage => ("partial outage", ORANGE),
            ComponentStatus::MajorOutage => ("major outage", RED),
            ComponentStatus::Maintenance => ("maintenance", BLUE),
        };
    }
    match component.state {
        MonitorState::Up => ("up", GREEN),
        MonitorState::Degraded => ("degraded", YELLOW),
        MonitorState::Pending => ("pending", YELLOW),
        MonitorState::Down => ("down", RED),
        MonitorState::Maintenance => ("maintenance", BLUE),
        MonitorState::Paused => ("paused", GREY),
        MonitorState::Unknown => ("unknown", GREY),
    }
}

fn page_message(status: PageStatus) -> (&'static str, &'static str) {
    match status {
        PageStatus::Operational => ("operational", GREEN),
        PageStatus::Degraded => ("degraded", YELLOW),
        PageStatus::PartialOutage => ("partial outage", ORANGE),
        PageStatus::MajorOutage => ("major outage", RED),
        PageStatus::Maintenance => ("maintenance", BLUE),
        PageStatus::Unknown => ("unknown", GREY),
    }
}

fn uptime_color(ratio: f64) -> &'static str {
    match ratio {
        r if r >= 0.999 => GREEN,
        r if r >= 0.99 => YELLOW_GREEN,
        r if r >= 0.97 => YELLOW,
        r if r >= 0.95 => ORANGE,
        _ => RED,
    }
}

fn latency_color(ms: i64) -> &'static str {
    match ms {
        ms if ms < 300 => GREEN,
        ms if ms < 800 => YELLOW_GREEN,
        ms if ms < 1500 => YELLOW,
        ms if ms < 3000 => ORANGE,
        _ => RED,
    }
}

/// `?style=flat|flat-square|for-the-badge&label=…&window=24h|7d|30d|90d`
#[topcoat::router::query_params]
pub(crate) struct BadgeQuery {
    style: Option<String>,
    label: Option<String>,
    window: Option<String>,
}

/// Longest custom label accepted.
const MAX_LABEL: usize = 64;

/// The style and the label (the query's, else `default`), or a 400.
fn options(cx: &Cx, default: &str) -> std::result::Result<(Style, String), &'static str> {
    let query = topcoat::router::query_params::<BadgeQuery>(cx).ok();
    let style = query
        .as_ref()
        .and_then(|q| q.style.as_deref())
        .unwrap_or_default();
    let style = Style::parse(style).ok_or("style must be flat, flat-square or for-the-badge")?;
    let label = match query.and_then(|q| q.label.clone()) {
        Some(label) => label.chars().take(MAX_LABEL).collect(),
        None => default.to_owned(),
    };
    Ok((style, label))
}

fn bad_request(cx: &Cx, message: &str) -> Result<Response> {
    (
        StatusCode::BAD_REQUEST,
        [(
            topcoat::router::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        message.to_owned(),
    )
        .into_response(cx)
}

fn svg(cx: &Cx, view: &StatusView, svg: String) -> Result<Response> {
    let version = crate::assets::short_hash(svg.as_bytes());
    cached(cx, view, &version, "image/svg+xml; charset=utf-8", 60, svg)
}

fn component<'a>(view: &'a StatusView, key: &str) -> Result<&'a ComponentView> {
    view.sections
        .iter()
        .flat_map(|section| &section.components)
        .find(|component| component.key == key)
        .ok_or_else(|| not_found().into())
}

/// The page's overall status.
#[route(GET "/s/{slug}/badge.svg")]
pub(crate) async fn page_badge(cx: &Cx) -> Result<Response> {
    let view = load(cx, path_param::<Slug>(cx)).await?;
    let (style, label) = match options(cx, "status") {
        Ok(options) => options,
        Err(message) => return bad_request(cx, message),
    };
    let (message, color) = page_message(view.status);
    svg(cx, &view, render(&label, message, color, style))
}

/// A component's current status.
#[route(GET "/s/{slug}/badge/{key}/status.svg")]
pub(crate) async fn status_badge(cx: &Cx) -> Result<Response> {
    let view = load(cx, path_param::<Slug>(cx)).await?;
    let component = component(&view, path_param::<Key>(cx))?;
    let (style, label) = match options(cx, &component.name) {
        Ok(options) => options,
        Err(message) => return bad_request(cx, message),
    };
    let (message, color) = status_message(component);
    svg(cx, &view, render(&label, message, color, style))
}

/// A component's uptime over `?window=` (default 30 days).
#[route(GET "/s/{slug}/badge/{key}/uptime.svg")]
pub(crate) async fn uptime_badge(cx: &Cx) -> Result<Response> {
    let view = load(cx, path_param::<Slug>(cx)).await?;
    let component = component(&view, path_param::<Key>(cx))?;
    let window = topcoat::router::query_params::<BadgeQuery>(cx)
        .ok()
        .and_then(|q| q.window.clone())
        .unwrap_or_else(|| "30d".to_owned());
    let windows = &component.metrics.windows;
    let ratio = match window.as_str() {
        "24h" => windows.h24,
        "7d" => windows.d7,
        "30d" => windows.d30,
        "90d" => windows.d90,
        _ => return bad_request(cx, "window must be 24h, 7d, 30d or 90d"),
    };
    let (style, label) = match options(cx, &format!("uptime {window}")) {
        Ok(options) => options,
        Err(message) => return bad_request(cx, message),
    };
    let (message, color) = ratio.map_or_else(
        || ("no data".to_owned(), GREY),
        |ratio| (fmt::uptime(ratio), uptime_color(ratio)),
    );
    svg(cx, &view, render(&label, &message, color, style))
}

/// A monitor component's median response time over the last 24 hours.
#[route(GET "/s/{slug}/badge/{key}/response.svg")]
pub(crate) async fn response_badge(cx: &Cx) -> Result<Response> {
    let view = load(cx, path_param::<Slug>(cx)).await?;
    let component = component(&view, path_param::<Key>(cx))?;
    if component.monitor_id.is_none() {
        return Err(not_found().into());
    }
    let (style, label) = match options(cx, "response time") {
        Ok(options) => options,
        Err(message) => return bad_request(cx, message),
    };
    let (message, color) = component.metrics.summary.as_ref().map_or_else(
        || ("no data".to_owned(), GREY),
        |summary| (format!("{} ms", summary.p50), latency_color(summary.p50)),
    );
    svg(cx, &view, render(&label, &message, color, style))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    use super::*;

    #[test]
    fn badges_size_themselves_to_their_text() {
        let short = render("api", "up", GREEN, Style::Flat);
        let long = render("api gateway", "major outage", RED, Style::Flat);
        let width = |svg: &str| -> f64 {
            svg.split("width=\"")
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap()
                .parse()
                .unwrap()
        };
        assert!(width(&long) > width(&short) + 50.0);
        assert!(short.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""));
        assert!(short.contains("<title>api: up</title>"));
        assert!(short.contains(GREEN));
    }

    #[test]
    fn text_is_escaped() {
        let svg = render("<script>&\"'", "up", GREEN, Style::Flat);
        assert!(!svg.contains("<script>"));
        assert!(svg.contains("&lt;script&gt;&amp;&quot;&apos;"));
    }

    #[test]
    fn styles_change_shape_and_case() {
        let flat = render("api", "up", GREEN, Style::Flat);
        let square = render("api", "up", GREEN, Style::FlatSquare);
        let big = render("api", "up", GREEN, Style::ForTheBadge);
        assert!(flat.contains("rx=\"3\"") && flat.contains("linearGradient"));
        assert!(square.contains("rx=\"0\"") && !square.contains("linearGradient"));
        assert!(big.contains("height=\"28\"") && big.contains(">API<") && big.contains(">UP<"));
    }

    #[test]
    fn an_empty_label_draws_only_the_message() {
        let svg = render("", "up", GREEN, Style::Flat);
        assert!(svg.contains("<title>up</title>"));
        assert!(svg.contains(r#"<rect width="0""#));
    }

    #[rstest]
    #[case("", Some(Style::Flat))]
    #[case("flat", Some(Style::Flat))]
    #[case("flat-square", Some(Style::FlatSquare))]
    #[case("for-the-badge", Some(Style::ForTheBadge))]
    #[case("plastic", None)]
    fn styles_parse(#[case] text: &str, #[case] expected: Option<Style>) {
        assert_eq!(Style::parse(text), expected);
    }

    #[rstest]
    #[case(1.0, GREEN)]
    #[case(0.995, YELLOW_GREEN)]
    #[case(0.98, YELLOW)]
    #[case(0.96, ORANGE)]
    #[case(0.5, RED)]
    fn uptime_colors_follow_the_bars(#[case] ratio: f64, #[case] color: &str) {
        assert_eq!(uptime_color(ratio), color);
    }

    #[test]
    fn widths_follow_verdana() {
        assert!(text_width("iii") < text_width("mmm"));
        assert!((text_width("99.95%") - 43.74).abs() < 0.01);
    }
}
