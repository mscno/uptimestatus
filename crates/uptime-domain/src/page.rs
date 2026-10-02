//! Status page definitions and the rules for presenting them.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{MonitorKey, MonitorState, Tally};

/// URL slug of a status page (`/s/{slug}`): same rules as monitor keys.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PageSlug(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("page slug must be 1-64 chars of a-z, 0-9 and '-', not starting or ending with '-': `{0}`")]
pub struct PageSlugError(pub String);

impl PageSlug {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for PageSlug {
    type Err = PageSlugError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<MonitorKey>()
            .map(|key| Self(key.to_string()))
            .map_err(|_| PageSlugError(s.to_owned()))
    }
}

impl TryFrom<String> for PageSlug {
    type Error = PageSlugError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<PageSlug> for String {
    fn from(value: PageSlug) -> Self {
        value.0
    }
}

impl fmt::Display for PageSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Color scheme of a status page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    /// Follow the visitor's system setting.
    #[default]
    Auto,
    Light,
    Dark,
}

impl Theme {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
}

impl FromStr for Theme {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" | "" => Ok(Self::Auto),
            "light" => Ok(Self::Light),
            "dark" => Ok(Self::Dark),
            other => Err(format!("unknown theme `{other}`")),
        }
    }
}

/// The visual style of a status page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Look {
    /// The 8-bit look: pixel display font, starfield backdrop, grid header.
    #[default]
    Pixel,
    /// Plain: system fonts, no backdrop, softer corners.
    Clean,
}

impl Look {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pixel => "pixel",
            Self::Clean => "clean",
        }
    }

    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl FromStr for Look {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pixel" | "" => Ok(Self::Pixel),
            "clean" => Ok(Self::Clean),
            other => Err(format!("unknown look `{other}`")),
        }
    }
}

/// A `#rgb` or `#rrggbb` color, stored lowercase.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Accent(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("accent must be a hex color like #4f46e5: `{0}`")]
pub struct AccentError(pub String);

impl Accent {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for Accent {
    type Err = AccentError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        let valid = trimmed.strip_prefix('#').is_some_and(|hex| {
            matches!(hex.len(), 3 | 6) && hex.bytes().all(|b| b.is_ascii_hexdigit())
        });
        if valid {
            Ok(Self(trimmed.to_ascii_lowercase()))
        } else {
            Err(AccentError(s.to_owned()))
        }
    }
}

impl TryFrom<String> for Accent {
    type Error = AccentError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<Accent> for String {
    fn from(value: Accent) -> Self {
        value.0
    }
}

/// One monitor shown on a page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentSpec {
    pub monitor: MonitorKey,
    /// Shown instead of the monitor's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// A titled group of components.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SectionSpec {
    pub name: String,
    #[serde(default)]
    pub components: Vec<ComponentSpec>,
}

/// Everything an admin defines about a status page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageSpec {
    pub slug: PageSlug,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent: Option<Accent>,
    #[serde(default)]
    pub theme: Theme,
    /// The page's visual style (the 8-bit look, or plain).
    #[serde(default, skip_serializing_if = "Look::is_default")]
    pub look: Look,
    #[serde(default = "PageSpec::default_published")]
    pub published: bool,
    /// The site this status page belongs to, linked from the page header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub website: Option<Website>,
    #[serde(default, rename = "section")]
    pub sections: Vec<SectionSpec>,
}

/// A "back to the product" link on a status page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Website {
    pub url: url::Url,
    /// Link text; defaults to "Back to <host>".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl Website {
    /// The label, or "Back to example.com" from the URL's host (without `www.`).
    pub fn link_text(&self) -> String {
        if let Some(label) = self
            .label
            .as_deref()
            .map(str::trim)
            .filter(|l| !l.is_empty())
        {
            return label.to_owned();
        }
        let host = self.url.host_str().unwrap_or_default();
        format!("Back to {}", host.strip_prefix("www.").unwrap_or(host))
    }
}

impl PageSpec {
    const fn default_published() -> bool {
        true
    }

    /// Every monitor key referenced by the page, in layout order.
    pub fn monitor_keys(&self) -> impl Iterator<Item = &MonitorKey> {
        self.sections
            .iter()
            .flat_map(|section| section.components.iter().map(|c| &c.monitor))
    }
}

/// A problem in a page layout, with its 1-based line.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("line {line}: {message}")]
pub struct LayoutError {
    pub line: usize,
    pub message: String,
}

/// Parses the plain-text layout used by the page editor:
///
/// ```text
/// ## Core
/// api-health | API
/// web-app
/// ## Data
/// postgres-primary | Database
/// ```
///
/// `## Name` starts a section; each other non-blank line is a monitor key,
/// optionally followed by `| Label`. Lines starting with `//` are comments.
pub fn parse_layout(text: &str) -> Result<Vec<SectionSpec>, LayoutError> {
    let mut sections: Vec<SectionSpec> = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        let error = |message: String| LayoutError {
            line: index + 1,
            message,
        };
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        if line.starts_with('#') {
            let name = line.trim_start_matches('#').trim();
            if name.is_empty() {
                return Err(error("section name is empty".into()));
            }
            sections.push(SectionSpec {
                name: name.to_owned(),
                components: Vec::new(),
            });
            continue;
        }
        let Some(section) = sections.last_mut() else {
            return Err(error(
                "start with a section heading like `## Services`".into(),
            ));
        };
        let (key, label) = match line.split_once('|') {
            Some((key, label)) => (
                key.trim(),
                Some(label.trim()).filter(|label| !label.is_empty()),
            ),
            None => (line, None),
        };
        let monitor = key
            .parse::<MonitorKey>()
            .map_err(|_| error(format!("`{key}` is not a valid monitor key")))?;
        section.components.push(ComponentSpec {
            monitor,
            label: label.map(str::to_owned),
        });
    }
    Ok(sections)
}

/// Writes sections back in the layout format [`parse_layout`] reads.
pub fn format_layout(sections: &[SectionSpec]) -> String {
    sections
        .iter()
        .map(|section| {
            let mut block = format!("## {}\n", section.name);
            for component in &section.components {
                block.push_str(component.monitor.as_str());
                if let Some(label) = &component.label {
                    block.push_str(" | ");
                    block.push_str(label);
                }
                block.push('\n');
            }
            block
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The headline of a status page, from its components' states.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageStatus {
    Operational,
    Degraded,
    PartialOutage,
    MajorOutage,
    Maintenance,
    Unknown,
}

impl PageStatus {
    /// How bad, for combining statuses (higher is worse).
    fn severity(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Operational => 1,
            Self::Maintenance => 2,
            Self::Degraded => 3,
            Self::PartialOutage => 4,
            Self::MajorOutage => 5,
        }
    }

    /// The worse of the two: what components show, raised by what an open
    /// incident says (minor → degraded, major → partial, critical → major outage).
    #[must_use]
    pub fn with_incident(self, impact: crate::Impact) -> Self {
        let declared = match impact {
            crate::Impact::None => return self,
            crate::Impact::Minor => Self::Degraded,
            crate::Impact::Major => Self::PartialOutage,
            crate::Impact::Critical => Self::MajorOutage,
        };
        if declared.severity() > self.severity() {
            declared
        } else {
            self
        }
    }

    pub fn headline(self) -> &'static str {
        match self {
            Self::Operational => "All systems operational",
            Self::Degraded => "Degraded performance",
            Self::PartialOutage => "Partial outage",
            Self::MajorOutage => "Major outage",
            Self::Maintenance => "Scheduled maintenance",
            Self::Unknown => "Status unknown",
        }
    }
}

/// Worst first: any DOWN is an outage (major when more than half are down);
/// then degraded/pending; then maintenance; all UP is operational. Paused
/// components are ignored; nothing to judge is `Unknown`.
pub fn page_status(states: &[MonitorState]) -> PageStatus {
    let judged: Vec<MonitorState> = states
        .iter()
        .copied()
        .filter(|s| !matches!(s, MonitorState::Paused | MonitorState::Unknown))
        .collect();
    if judged.is_empty() {
        return PageStatus::Unknown;
    }
    let count = |wanted: &[MonitorState]| judged.iter().filter(|s| wanted.contains(s)).count();
    let down = count(&[MonitorState::Down]);
    let in_service = judged.len() - count(&[MonitorState::Maintenance]);
    if down > 0 {
        return if down * 2 > in_service {
            PageStatus::MajorOutage
        } else {
            PageStatus::PartialOutage
        };
    }
    if count(&[MonitorState::Degraded, MonitorState::Pending]) > 0 {
        PageStatus::Degraded
    } else if count(&[MonitorState::Maintenance]) > 0 {
        PageStatus::Maintenance
    } else {
        PageStatus::Operational
    }
}

/// How one day's bar is colored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BarLevel {
    /// ≥ 99.9% up.
    Up,
    /// ≥ 99% up.
    Warn,
    /// < 99% up.
    Down,
    /// No checks that day.
    None,
}

pub fn bar_level(tally: Option<&Tally>) -> BarLevel {
    match tally.and_then(Tally::uptime) {
        None => BarLevel::None,
        Some(ratio) if ratio >= 0.999 => BarLevel::Up,
        Some(ratio) if ratio >= 0.99 => BarLevel::Warn,
        Some(_) => BarLevel::Down,
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    use super::*;

    #[test]
    fn website_links_default_to_the_host() {
        let site = Website {
            url: "https://www.privatenpm.com/pricing".parse().unwrap(),
            label: None,
        };
        assert_eq!(site.link_text(), "Back to privatenpm.com");
        let labelled = Website {
            label: Some("Return to PrivateNPM".into()),
            ..site.clone()
        };
        assert_eq!(labelled.link_text(), "Return to PrivateNPM");
        let blank = Website {
            label: Some("  ".into()),
            ..site
        };
        assert_eq!(blank.link_text(), "Back to privatenpm.com");
    }

    #[test]
    fn open_incidents_raise_the_headline_but_never_lower_it() {
        use crate::Impact;
        assert_eq!(
            PageStatus::Operational.with_incident(Impact::Major),
            PageStatus::PartialOutage
        );
        assert_eq!(
            PageStatus::Operational.with_incident(Impact::None),
            PageStatus::Operational
        );
        assert_eq!(
            PageStatus::MajorOutage.with_incident(Impact::Minor),
            PageStatus::MajorOutage
        );
        assert_eq!(
            PageStatus::Maintenance.with_incident(Impact::Critical),
            PageStatus::MajorOutage
        );
    }

    fn component(key: &str, label: Option<&str>) -> ComponentSpec {
        ComponentSpec {
            monitor: key.parse().unwrap(),
            label: label.map(str::to_owned),
        }
    }

    #[test]
    fn slugs_follow_the_key_rules() {
        assert_eq!("platform".parse::<PageSlug>().unwrap().as_str(), "platform");
        assert!("Platform".parse::<PageSlug>().is_err());
        assert!("-x".parse::<PageSlug>().is_err());
    }

    #[rstest]
    #[case("#4f46e5", "#4f46e5")]
    #[case("#4F46E5", "#4f46e5")]
    #[case("#abc", "#abc")]
    #[case(" #abc ", "#abc")]
    fn accepts_hex_accents(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(input.parse::<Accent>().unwrap().as_str(), expected);
    }

    #[rstest]
    #[case("4f46e5")]
    #[case("#4f46e")]
    #[case("#ggg")]
    #[case("red")]
    #[case("#abc;x")]
    fn rejects_other_accents(#[case] input: &str) {
        assert!(input.parse::<Accent>().is_err(), "{input}");
    }

    #[test]
    fn parses_sections_components_and_labels() {
        let layout = "## Core\napi-health | API\nweb-app\n\n// comment\n## Data\n  postgres-primary |  Database  \n";
        assert_eq!(
            parse_layout(layout).unwrap(),
            [
                SectionSpec {
                    name: "Core".into(),
                    components: vec![
                        component("api-health", Some("API")),
                        component("web-app", None)
                    ],
                },
                SectionSpec {
                    name: "Data".into(),
                    components: vec![component("postgres-primary", Some("Database"))]
                },
            ]
        );
    }

    #[test]
    fn an_empty_layout_has_no_sections() {
        assert_eq!(parse_layout("  \n\n").unwrap(), []);
    }

    #[test]
    fn components_need_a_section() {
        assert_eq!(
            parse_layout("api\n## Core").unwrap_err(),
            LayoutError {
                line: 1,
                message: "start with a section heading like `## Services`".into()
            }
        );
    }

    #[test]
    fn bad_keys_and_empty_names_are_reported_with_lines() {
        assert_eq!(parse_layout("## Core\nNot A Key").unwrap_err().line, 2);
        assert_eq!(
            parse_layout("## \napi").unwrap_err(),
            LayoutError {
                line: 1,
                message: "section name is empty".into()
            }
        );
    }

    #[test]
    fn layouts_round_trip() {
        let sections = vec![
            SectionSpec {
                name: "Core".into(),
                components: vec![component("api", Some("API")), component("web", None)],
            },
            SectionSpec {
                name: "Empty".into(),
                components: vec![],
            },
        ];
        let text = format_layout(&sections);
        assert_eq!(text, "## Core\napi | API\nweb\n\n## Empty\n");
        assert_eq!(parse_layout(&text).unwrap(), sections);
    }

    #[test]
    fn page_spec_reads_from_toml_style_data() {
        let spec: PageSpec = serde_json::from_value(serde_json::json!({
            "slug": "platform",
            "title": "Platform Status",
            "accent": "#4F46E5",
            "section": [{ "name": "Core", "components": [{ "monitor": "api", "label": "API" }] }]
        }))
        .unwrap();
        assert!(spec.published);
        assert_eq!(spec.theme, Theme::Auto);
        assert_eq!(spec.accent.unwrap().as_str(), "#4f46e5");
        assert_eq!(spec.sections[0].components[0].monitor.as_str(), "api");
    }

    #[test]
    fn the_look_defaults_to_pixel_and_reads_from_data() {
        let base = serde_json::json!({ "slug": "platform", "title": "Platform" });
        let default: PageSpec = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(default.look, Look::Pixel);
        let clean: PageSpec = serde_json::from_value(
            serde_json::json!({ "slug": "platform", "title": "Platform", "look": "clean" }),
        )
        .unwrap();
        assert_eq!(clean.look, Look::Clean);
        assert!(
            serde_json::from_value::<PageSpec>(
                serde_json::json!({ "slug": "p", "title": "P", "look": "neon" })
            )
            .is_err()
        );
    }

    #[test]
    fn only_a_non_default_look_is_written_out() {
        let mut spec: PageSpec =
            serde_json::from_value(serde_json::json!({ "slug": "platform", "title": "Platform" }))
                .unwrap();
        assert!(serde_json::to_value(&spec).unwrap().get("look").is_none());
        spec.look = Look::Clean;
        assert_eq!(serde_json::to_value(&spec).unwrap()["look"], "clean");
    }

    #[rstest]
    #[case("pixel", Some(Look::Pixel))]
    #[case("", Some(Look::Pixel))]
    #[case("clean", Some(Look::Clean))]
    #[case("neon", None)]
    fn looks_parse_from_form_values(#[case] text: &str, #[case] expected: Option<Look>) {
        assert_eq!(text.parse::<Look>().ok(), expected);
    }

    #[rstest]
    #[case(&[], PageStatus::Unknown)]
    #[case(&[MonitorState::Up, MonitorState::Up], PageStatus::Operational)]
    #[case(&[MonitorState::Up, MonitorState::Paused], PageStatus::Operational)]
    #[case(&[MonitorState::Paused], PageStatus::Unknown)]
    #[case(&[MonitorState::Unknown], PageStatus::Unknown)]
    #[case(&[MonitorState::Up, MonitorState::Degraded], PageStatus::Degraded)]
    #[case(&[MonitorState::Up, MonitorState::Pending], PageStatus::Degraded)]
    #[case(&[MonitorState::Up, MonitorState::Maintenance], PageStatus::Maintenance)]
    #[case(&[MonitorState::Up, MonitorState::Up, MonitorState::Down], PageStatus::PartialOutage)]
    #[case(&[MonitorState::Up, MonitorState::Down], PageStatus::PartialOutage)]
    #[case(&[MonitorState::Down, MonitorState::Down, MonitorState::Up], PageStatus::MajorOutage)]
    #[case(&[MonitorState::Down, MonitorState::Maintenance], PageStatus::MajorOutage)]
    fn page_status_reflects_the_worst_component(
        #[case] states: &[MonitorState],
        #[case] expected: PageStatus,
    ) {
        assert_eq!(page_status(states), expected);
    }

    #[rstest]
    #[case(None, BarLevel::None)]
    #[case(Some(Tally { total: 1000, up: 1000, ..Default::default() }), BarLevel::Up)]
    #[case(Some(Tally { total: 1000, up: 999, down: 1, ..Default::default() }), BarLevel::Up)]
    #[case(Some(Tally { total: 1000, up: 998, down: 2, ..Default::default() }), BarLevel::Warn)]
    #[case(Some(Tally { total: 100, up: 99, down: 1, ..Default::default() }), BarLevel::Warn)]
    #[case(Some(Tally { total: 100, up: 98, down: 2, ..Default::default() }), BarLevel::Down)]
    #[case(Some(Tally { total: 5, maintenance: 5, ..Default::default() }), BarLevel::None)]
    fn bars_are_colored_by_uptime(#[case] tally: Option<Tally>, #[case] expected: BarLevel) {
        assert_eq!(bar_level(tally.as_ref()), expected);
    }
}
