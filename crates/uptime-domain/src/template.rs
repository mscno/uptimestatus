//! Incident templates: wording admins reuse when declaring or updating an
//! incident. Placeholders (`{monitor}`, `{monitors}`, `{page}`,
//! `{started_at}`) are expanded when the incident is published, so a
//! template picked now and published later still names the right things.

use serde::{Deserialize, Serialize};

use crate::{Impact, IncidentStatus};

/// The longest template name.
pub const MAX_TEMPLATE_NAME: usize = 80;

/// A saved incident template (`[[incident_template]]` in TOML).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IncidentTemplate {
    /// Unique, shown in pickers.
    pub name: String,
    /// The incident's title when declaring.
    pub title: String,
    #[serde(default)]
    pub impact: Impact,
    #[serde(default)]
    pub status: IncidentStatus,
    /// The update's message.
    pub body: String,
}

/// Why a template is invalid: the field and a sentence for the admin.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{field}: {message}")]
pub struct TemplateError {
    pub field: &'static str,
    pub message: String,
}

impl IncidentTemplate {
    /// Trims the text and checks that the template can be used.
    pub fn normalized(mut self) -> Result<Self, TemplateError> {
        self.name = self.name.trim().to_owned();
        self.title = self.title.trim().to_owned();
        self.body = self.body.trim().to_owned();
        let error = |field, message: &str| {
            Err(TemplateError {
                field,
                message: message.to_owned(),
            })
        };
        if self.name.is_empty() {
            return error("name", "Give the template a name.");
        }
        if self.name.chars().count() > MAX_TEMPLATE_NAME {
            return error("name", "Keep the name to 80 characters.");
        }
        if self.title.is_empty() {
            return error("title", "Give incidents from this template a title.");
        }
        if self.body.is_empty() {
            return error("body", "Write the message visitors will read.");
        }
        Ok(self)
    }
}

/// What placeholders stand for when an incident is published.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Placeholders {
    /// The first affected monitor's name.
    pub monitor: String,
    /// Every affected monitor's name, comma-separated.
    pub monitors: String,
    /// The status page(s) showing the incident, comma-separated.
    pub page: String,
    /// When the incident started, e.g. "Oct 15, 09:30 UTC".
    pub started_at: String,
}

/// Replaces the known placeholders in `text`; anything else in braces is
/// left as written.
pub fn expand(text: &str, values: &Placeholders) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open..];
        let replaced = [
            ("{monitor}", &values.monitor),
            ("{monitors}", &values.monitors),
            ("{page}", &values.page),
            ("{started_at}", &values.started_at),
        ]
        .into_iter()
        .find(|(name, _)| after.starts_with(name));
        match replaced {
            Some((name, value)) => {
                out.push_str(value);
                rest = &after[name.len()..];
            }
            None => {
                out.push('{');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn values() -> Placeholders {
        Placeholders {
            monitor: "API".into(),
            monitors: "API, Database".into(),
            page: "Acme Status".into(),
            started_at: "Oct 15, 09:30 UTC".into(),
        }
    }

    #[test]
    fn known_placeholders_are_expanded() {
        assert_eq!(
            expand(
                "{monitor} is down since {started_at}; affects {monitors} on {page}.",
                &values()
            ),
            "API is down since Oct 15, 09:30 UTC; affects API, Database on Acme Status."
        );
    }

    #[test]
    fn other_braces_are_left_alone() {
        assert_eq!(
            expand(r#"{"code": 503} {unknown} {monitor"#, &values()),
            r#"{"code": 503} {unknown} {monitor"#
        );
        assert_eq!(expand("{{monitor}}", &values()), "{API}");
        assert_eq!(expand("", &values()), "");
    }

    #[test]
    fn templates_are_trimmed_and_checked() {
        let template = IncidentTemplate {
            name: "  Degraded API ".into(),
            title: " {monitor} is slow ".into(),
            impact: Impact::Minor,
            status: IncidentStatus::Investigating,
            body: " We are looking into it. ".into(),
        };
        let normalized = template.clone().normalized().unwrap();
        assert_eq!(normalized.name, "Degraded API");
        assert_eq!(normalized.title, "{monitor} is slow");
        assert_eq!(normalized.body, "We are looking into it.");
        for (field, broken) in [
            (
                "name",
                IncidentTemplate {
                    name: " ".into(),
                    ..template.clone()
                },
            ),
            (
                "name",
                IncidentTemplate {
                    name: "x".repeat(81),
                    ..template.clone()
                },
            ),
            (
                "title",
                IncidentTemplate {
                    title: String::new(),
                    ..template.clone()
                },
            ),
            (
                "body",
                IncidentTemplate {
                    body: "\n".into(),
                    ..template.clone()
                },
            ),
        ] {
            assert_eq!(broken.normalized().unwrap_err().field, field);
        }
    }

    #[test]
    fn templates_read_from_toml_with_defaults() {
        let template: IncidentTemplate = toml::from_str(
            r#"
            name = "Investigating"
            title = "{monitor} outage"
            body = "We are investigating."
            "#,
        )
        .unwrap();
        assert_eq!(template.impact, Impact::Minor);
        assert_eq!(template.status, IncidentStatus::Investigating);
    }
}
