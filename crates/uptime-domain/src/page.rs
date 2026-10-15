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

/// A component's key on its page: the monitor key for monitor components,
/// an admin-chosen key (same rules) for manual ones. Status overrides and
/// manual status history refer to components by this key.
pub type ComponentKey = MonitorKey;

/// One row on a page: a monitor, or a manual component whose status admins set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawComponent", into = "RawComponent")]
pub enum ComponentSpec {
    Monitor {
        monitor: MonitorKey,
        /// Shown instead of the monitor's name.
        label: Option<String>,
    },
    Manual {
        key: ComponentKey,
        /// Shown instead of the key.
        label: Option<String>,
        description: Option<String>,
    },
}

impl ComponentSpec {
    pub fn of_monitor(monitor: MonitorKey, label: Option<String>) -> Self {
        Self::Monitor { monitor, label }
    }

    pub fn manual(key: ComponentKey, label: Option<String>, description: Option<String>) -> Self {
        Self::Manual {
            key,
            label,
            description,
        }
    }

    /// The component's key on its page.
    pub fn key(&self) -> &ComponentKey {
        match self {
            Self::Monitor { monitor, .. } => monitor,
            Self::Manual { key, .. } => key,
        }
    }

    /// The monitor behind a monitor component.
    pub fn monitor_key(&self) -> Option<&MonitorKey> {
        match self {
            Self::Monitor { monitor, .. } => Some(monitor),
            Self::Manual { .. } => None,
        }
    }

    pub fn label(&self) -> Option<&str> {
        match self {
            Self::Monitor { label, .. } | Self::Manual { label, .. } => label.as_deref(),
        }
    }

    pub fn description(&self) -> Option<&str> {
        match self {
            Self::Monitor { .. } => None,
            Self::Manual { description, .. } => description.as_deref(),
        }
    }

    pub fn is_manual(&self) -> bool {
        matches!(self, Self::Manual { .. })
    }
}

/// The data shape of a component: `monitor` or `manual` (a key), with an
/// optional `label`; manual components may add a `description`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawComponent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    monitor: Option<MonitorKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    manual: Option<ComponentKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

impl TryFrom<RawComponent> for ComponentSpec {
    type Error = String;

    fn try_from(raw: RawComponent) -> Result<Self, Self::Error> {
        match (raw.monitor, raw.manual) {
            (Some(monitor), None) => {
                if raw.description.is_some() {
                    return Err("only manual components have a description".into());
                }
                Ok(Self::Monitor {
                    monitor,
                    label: raw.label,
                })
            }
            (None, Some(key)) => Ok(Self::Manual {
                key,
                label: raw.label,
                description: raw.description,
            }),
            (Some(_), Some(_)) => {
                Err("a component is either `monitor` or `manual`, not both".into())
            }
            (None, None) => {
                Err("a component needs `monitor` (a monitor key) or `manual` (a key)".into())
            }
        }
    }
}

impl From<ComponentSpec> for RawComponent {
    fn from(component: ComponentSpec) -> Self {
        match component {
            ComponentSpec::Monitor { monitor, label } => Self {
                monitor: Some(monitor),
                manual: None,
                label,
                description: None,
            },
            ComponentSpec::Manual {
                key,
                label,
                description,
            } => Self {
                monitor: None,
                manual: Some(key),
                label,
                description,
            },
        }
    }
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
    /// Extra CSS served after the page's own styles (see [`validate_custom_css`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_css: Option<String>,
    #[serde(default, rename = "section")]
    pub sections: Vec<SectionSpec>,
}

/// Why a page definition is invalid: the field and a sentence for the admin.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{field}: {message}")]
pub struct PageError {
    pub field: &'static str,
    pub message: String,
}

/// The most custom CSS a page may carry.
pub const MAX_CUSTOM_CSS: usize = 64 * 1024;

/// Checks custom CSS before it is saved. It is served as its own stylesheet,
/// so it cannot break out of the page's markup; these rules keep it to
/// styling: no closing `</style`, no script URLs or IE expressions, no
/// bindings, and `@import` only of `https:` stylesheets.
pub fn validate_custom_css(css: &str) -> Result<(), String> {
    if css.len() > MAX_CUSTOM_CSS {
        return Err(format!(
            "Custom CSS is limited to {} KiB.",
            MAX_CUSTOM_CSS / 1024
        ));
    }
    let lower = css.to_ascii_lowercase();
    for (needle, why) in [
        ("</style", "`</style` is not allowed."),
        ("javascript:", "`javascript:` URLs are not allowed."),
        ("expression(", "CSS `expression(` is not allowed."),
        ("-moz-binding", "`-moz-binding` is not allowed."),
        ("behavior:", "`behavior:` is not allowed."),
    ] {
        if lower.contains(needle) {
            return Err(why.to_owned());
        }
    }
    let mut rest = lower.as_str();
    while let Some(at) = rest.find("@import") {
        rest = &rest[at + "@import".len()..];
        let target = rest.trim_start();
        let target = target.strip_prefix("url(").unwrap_or(target).trim_start();
        let target = target.trim_start_matches(['"', '\'']);
        if !target.starts_with("https://") {
            return Err("`@import` may only load `https://` stylesheets.".into());
        }
    }
    Ok(())
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
        self.components().filter_map(ComponentSpec::monitor_key)
    }

    /// Every component, in layout order.
    pub fn components(&self) -> impl Iterator<Item = &ComponentSpec> {
        self.sections
            .iter()
            .flat_map(|section| section.components.iter())
    }

    /// The component with this key, if any.
    pub fn component(&self, key: &str) -> Option<&ComponentSpec> {
        self.components().find(|c| c.key().as_str() == key)
    }

    /// Rules beyond the field formats: custom CSS, and manual component
    /// keys that are unique and distinct from the page's monitor keys.
    pub fn validate(&self) -> Result<(), PageError> {
        if let Some(css) = &self.custom_css {
            validate_custom_css(css).map_err(|message| PageError {
                field: "custom_css",
                message,
            })?;
        }
        let monitors: std::collections::HashSet<&str> =
            self.monitor_keys().map(MonitorKey::as_str).collect();
        let mut manual = std::collections::HashSet::new();
        for component in self.components().filter(|c| c.is_manual()) {
            let key = component.key().as_str();
            if monitors.contains(key) {
                return Err(PageError {
                    field: "layout",
                    message: format!(
                        "The manual component `{key}` has the same key as a monitor on this page."
                    ),
                });
            }
            if !manual.insert(key) {
                return Err(PageError {
                    field: "layout",
                    message: format!("The manual component key `{key}` is used twice."),
                });
            }
        }
        Ok(())
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
/// optionally followed by `| Label`, or a manual component written
/// `manual:key | Label | Description` (label and description optional).
/// Lines starting with `//` are comments.
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
        let mut parts = line.split('|').map(str::trim);
        let key = parts.next().unwrap_or_default();
        let mut text = || {
            parts
                .next()
                .filter(|part| !part.is_empty())
                .map(str::to_owned)
        };
        let (label, description) = (text(), text());
        let component = match key.strip_prefix("manual:") {
            Some(manual) => {
                let manual = manual.trim();
                let key = manual
                    .parse::<ComponentKey>()
                    .map_err(|_| error(format!("`{manual}` is not a valid component key")))?;
                ComponentSpec::manual(key, label, description)
            }
            None => {
                if description.is_some() {
                    return Err(error(
                        "only manual components (`manual:key`) have a description".into(),
                    ));
                }
                let monitor = key
                    .parse::<MonitorKey>()
                    .map_err(|_| error(format!("`{key}` is not a valid monitor key")))?;
                ComponentSpec::of_monitor(monitor, label)
            }
        };
        section.components.push(component);
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
                match component {
                    ComponentSpec::Monitor { monitor, label } => {
                        block.push_str(monitor.as_str());
                        if let Some(label) = label {
                            block.push_str(" | ");
                            block.push_str(label);
                        }
                    }
                    ComponentSpec::Manual {
                        key,
                        label,
                        description,
                    } => {
                        block.push_str("manual:");
                        block.push_str(key.as_str());
                        if label.is_some() || description.is_some() {
                            block.push_str(" | ");
                            block.push_str(label.as_deref().unwrap_or(key.as_str()));
                        }
                        if let Some(description) = description {
                            block.push_str(" | ");
                            block.push_str(description);
                        }
                    }
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

    /// The worse of two statuses.
    #[must_use]
    pub fn worse(self, other: Self) -> Self {
        if other.severity() > self.severity() {
            other
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
        ComponentSpec::of_monitor(key.parse().unwrap(), label.map(str::to_owned))
    }

    fn manual(key: &str, label: Option<&str>, description: Option<&str>) -> ComponentSpec {
        ComponentSpec::manual(
            key.parse().unwrap(),
            label.map(str::to_owned),
            description.map(str::to_owned),
        )
    }

    #[test]
    fn manual_components_are_written_with_a_prefix_and_round_trip() {
        let layout = "## Edge\nmanual:cdn | CDN | Static assets worldwide\nmanual:dns\nmanual:email | Email\napi | API\n";
        let sections = parse_layout(layout).unwrap();
        assert_eq!(
            sections[0].components,
            [
                manual("cdn", Some("CDN"), Some("Static assets worldwide")),
                manual("dns", None, None),
                manual("email", Some("Email"), None),
                component("api", Some("API")),
            ]
        );
        assert_eq!(format_layout(&sections), layout);
        assert_eq!(
            parse_layout("## Edge\nmanual:Not Valid").unwrap_err(),
            LayoutError {
                line: 2,
                message: "`Not Valid` is not a valid component key".into()
            }
        );
        assert_eq!(
            parse_layout("## Edge\napi | API | words")
                .unwrap_err()
                .message,
            "only manual components (`manual:key`) have a description"
        );
        // A description without a label keeps the key as the label.
        let unlabeled = vec![SectionSpec {
            name: "Edge".into(),
            components: vec![manual("cdn", None, Some("Assets"))],
        }];
        assert_eq!(
            format_layout(&unlabeled),
            "## Edge\nmanual:cdn | cdn | Assets\n"
        );
    }

    #[test]
    fn components_read_and_write_as_monitor_or_manual_data() {
        let monitor = serde_json::json!({ "monitor": "api", "label": "API" });
        let cdn = serde_json::json!({ "manual": "cdn", "label": "CDN", "description": "Assets" });
        assert_eq!(
            serde_json::from_value::<ComponentSpec>(monitor.clone()).unwrap(),
            component("api", Some("API"))
        );
        assert_eq!(
            serde_json::from_value::<ComponentSpec>(cdn.clone()).unwrap(),
            manual("cdn", Some("CDN"), Some("Assets"))
        );
        assert_eq!(
            serde_json::to_value(manual("cdn", Some("CDN"), Some("Assets"))).unwrap(),
            cdn
        );
        assert_eq!(
            serde_json::to_value(component("api", Some("API"))).unwrap(),
            monitor
        );
        for bad in [
            serde_json::json!({ "label": "Nothing" }),
            serde_json::json!({ "monitor": "api", "manual": "cdn" }),
            serde_json::json!({ "monitor": "api", "description": "x" }),
            serde_json::json!({ "manual": "cdn", "colour": "red" }),
        ] {
            assert!(
                serde_json::from_value::<ComponentSpec>(bad.clone()).is_err(),
                "{bad}"
            );
        }
    }

    fn page_with(components: Vec<ComponentSpec>, css: Option<&str>) -> PageSpec {
        PageSpec {
            slug: "platform".parse().unwrap(),
            title: "Platform".into(),
            description: None,
            accent: None,
            theme: Theme::Auto,
            look: Look::Pixel,
            published: true,
            website: None,
            custom_css: css.map(str::to_owned),
            sections: vec![SectionSpec {
                name: "Core".into(),
                components,
            }],
        }
    }

    #[test]
    fn manual_keys_must_be_unique_and_differ_from_monitor_keys() {
        let ok = page_with(
            vec![component("api", None), manual("cdn", None, None)],
            None,
        );
        assert_eq!(ok.validate(), Ok(()));
        assert_eq!(
            ok.monitor_keys()
                .map(MonitorKey::as_str)
                .collect::<Vec<_>>(),
            ["api"]
        );
        assert!(ok.component("cdn").unwrap().is_manual());
        let clash = page_with(
            vec![component("api", None), manual("api", None, None)],
            None,
        );
        assert_eq!(clash.validate().unwrap_err().field, "layout");
        let twice = page_with(
            vec![manual("cdn", None, None), manual("cdn", None, None)],
            None,
        );
        assert!(twice.validate().unwrap_err().message.contains("used twice"));
    }

    #[rstest]
    #[case(".status-header { background: #000; }", true)]
    #[case(
        "@import url(\"https://fonts.googleapis.com/css2?family=Inter\");",
        true
    )]
    #[case("@import 'https://example.com/x.css';", true)]
    #[case("@import url(http://example.com/x.css);", false)]
    #[case("@import '//example.com/x.css';", false)]
    #[case("a { background: url(JavaScript:alert(1)) }", false)]
    #[case("a { width: expression(alert(1)) }", false)]
    #[case("</STYLE><script>", false)]
    #[case("a { -moz-binding: url(x) }", false)]
    #[case("a { behavior: url(x.htc) }", false)]
    fn custom_css_is_limited_to_styling(#[case] css: &str, #[case] allowed: bool) {
        assert_eq!(validate_custom_css(css).is_ok(), allowed, "{css}");
        assert_eq!(
            page_with(vec![], Some(css)).validate().is_ok(),
            allowed,
            "{css}"
        );
    }

    #[test]
    fn custom_css_has_a_size_limit() {
        assert!(validate_custom_css(&"a{}".repeat(MAX_CUSTOM_CSS / 3)).is_ok());
        assert!(validate_custom_css(&"x".repeat(MAX_CUSTOM_CSS + 1)).is_err());
    }

    #[test]
    fn the_worse_status_wins() {
        assert_eq!(
            PageStatus::Operational.worse(PageStatus::Degraded),
            PageStatus::Degraded
        );
        assert_eq!(
            PageStatus::MajorOutage.worse(PageStatus::Maintenance),
            PageStatus::MajorOutage
        );
        assert_eq!(
            PageStatus::Unknown.worse(PageStatus::Operational),
            PageStatus::Operational
        );
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
        assert_eq!(spec.sections[0].components[0].key().as_str(), "api");
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
