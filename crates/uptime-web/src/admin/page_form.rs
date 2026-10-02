//! The status page form: raw input ⇄ [`PageSpec`].

use serde::Deserialize;
use uptime_domain::{
    Accent, Look, PageSlug, PageSpec, Theme, Website, format_layout, parse_layout,
};

use super::form::FormErrors;

/// `application/x-www-form-urlencoded` body of the page form.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PageForm {
    pub slug: String,
    pub title: String,
    pub description: String,
    pub accent: String,
    pub theme: String,
    /// `pixel` (the 8-bit look) or `clean`.
    pub look: String,
    pub published: Option<String>,
    /// The product site linked from the page header, and its link text.
    pub website_url: String,
    pub website_label: String,
    /// Sections and components in the plain-text layout format.
    pub layout: String,
}

impl PageForm {
    /// A new, published page with an example layout comment.
    pub fn new_page() -> Self {
        Self {
            theme: "auto".into(),
            look: "pixel".into(),
            published: Some("on".into()),
            layout: "## Services\n".into(),
            ..Self::default()
        }
    }

    /// A copy of `spec` for the "new page" form: the next free slug after
    /// `taken`, "(copy)" in the title, and unpublished. Custom domains are not copied.
    pub fn duplicate(spec: &PageSpec, taken: &[String]) -> Self {
        Self {
            slug: super::copy_of(
                spec.slug.as_str(),
                taken,
                uptime_domain::MonitorKey::MAX_LEN,
            ),
            title: format!("{} (copy)", spec.title),
            published: None,
            ..Self::from_spec(spec)
        }
    }

    pub fn from_spec(spec: &PageSpec) -> Self {
        Self {
            slug: spec.slug.to_string(),
            title: spec.title.clone(),
            description: spec.description.clone().unwrap_or_default(),
            accent: spec
                .accent
                .as_ref()
                .map(|a| a.as_str().to_owned())
                .unwrap_or_default(),
            theme: spec.theme.as_str().into(),
            look: spec.look.as_str().into(),
            published: spec.published.then(|| "on".into()),
            website_url: spec
                .website
                .as_ref()
                .map(|w| w.url.to_string())
                .unwrap_or_default(),
            website_label: spec
                .website
                .as_ref()
                .and_then(|w| w.label.clone())
                .unwrap_or_default(),
            layout: format_layout(&spec.sections),
        }
    }

    pub fn to_spec(&self) -> Result<PageSpec, FormErrors> {
        let mut errors = FormErrors::default();
        let slug = self.slug.trim().parse::<PageSlug>().map_err(|_| {
            errors.insert(
                "slug",
                "Use 1-64 lowercase letters, digits and dashes (e.g. platform).",
            );
        });
        let title = self.title.trim();
        if title.is_empty() {
            errors.insert("title", "Give the page a title.");
        }
        let accent = match self.accent.trim() {
            "" => Ok(None),
            text => text.parse::<Accent>().map(Some).map_err(|_| {
                errors.insert("accent", "Use a hex color like #4f46e5.");
            }),
        };
        let theme = self.theme.trim().parse::<Theme>().map_err(|_| {
            errors.insert("theme", "Choose auto, light or dark.");
        });
        let look = self.look.trim().parse::<Look>().map_err(|_| {
            errors.insert("look", "Choose the 8-bit or the clean look.");
        });
        let sections = parse_layout(&self.layout).map_err(|error| {
            errors.insert("layout", format!("Line {}: {}.", error.line, error.message));
        });
        let description = Some(self.description.trim())
            .filter(|d| !d.is_empty())
            .map(str::to_owned);
        let label = Some(self.website_label.trim())
            .filter(|l| !l.is_empty())
            .map(str::to_owned);
        let website = match self.website_url.trim() {
            "" => {
                if label.is_some() {
                    errors.insert("website_url", "Add the URL the link text points to.");
                }
                Ok(None)
            }
            text => match text.parse::<url::Url>() {
                Ok(url) if matches!(url.scheme(), "http" | "https") && url.host().is_some() => {
                    Ok(Some(Website { url, label }))
                }
                _ => {
                    errors.insert("website_url", "Enter a full URL, e.g. https://example.com.");
                    Err(())
                }
            },
        };

        match (slug, accent, theme, look, sections, website) {
            (Ok(slug), Ok(accent), Ok(theme), Ok(look), Ok(sections), Ok(website))
                if errors.is_empty() =>
            {
                Ok(PageSpec {
                    slug,
                    title: title.to_owned(),
                    description,
                    accent,
                    theme,
                    look,
                    published: self.published.is_some(),
                    website,
                    sections,
                })
            }
            _ => Err(errors),
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn with_site(url: &str, label: &str) -> PageForm {
        PageForm {
            slug: "platform".into(),
            title: "Platform".into(),
            theme: "auto".into(),
            website_url: url.into(),
            website_label: label.into(),
            ..PageForm::default()
        }
    }

    #[test]
    fn website_links_are_optional_and_validated() {
        assert_eq!(with_site("", "").to_spec().unwrap().website, None);
        let site = with_site(" https://privatenpm.com ", "Return to PrivateNPM")
            .to_spec()
            .unwrap()
            .website
            .unwrap();
        assert_eq!(site.url.as_str(), "https://privatenpm.com/");
        assert_eq!(site.label.as_deref(), Some("Return to PrivateNPM"));
        assert!(
            with_site("privatenpm.com", "")
                .to_spec()
                .unwrap_err()
                .get("website_url")
                .is_some()
        );
        assert!(
            with_site("", "Back home")
                .to_spec()
                .unwrap_err()
                .get("website_url")
                .is_some()
        );
        let round_trip =
            PageForm::from_spec(&with_site("https://x.example.com/", "X").to_spec().unwrap());
        assert_eq!(round_trip.website_url, "https://x.example.com/");
        assert_eq!(round_trip.website_label, "X");
    }

    fn valid() -> PageForm {
        PageForm {
            slug: "platform".into(),
            title: "Platform Status".into(),
            description: "  Core services  ".into(),
            accent: "#4F46E5".into(),
            theme: "dark".into(),
            published: Some("on".into()),
            layout: "## Core\napi | API\nweb\n".into(),
            ..PageForm::default()
        }
    }

    #[test]
    fn builds_a_page_spec() {
        let spec = valid().to_spec().unwrap();
        assert_eq!(spec.slug.as_str(), "platform");
        assert_eq!(spec.description.as_deref(), Some("Core services"));
        assert_eq!(spec.accent.unwrap().as_str(), "#4f46e5");
        assert_eq!(spec.theme, Theme::Dark);
        assert!(spec.published);
        assert_eq!(spec.sections.len(), 1);
        assert_eq!(spec.sections[0].components[0].label.as_deref(), Some("API"));
    }

    #[test]
    fn the_look_defaults_to_pixel_and_can_be_clean() {
        assert_eq!(valid().to_spec().unwrap().look, Look::Pixel);
        let clean = PageForm {
            look: "clean".into(),
            ..valid()
        };
        let spec = clean.to_spec().unwrap();
        assert_eq!(spec.look, Look::Clean);
        assert_eq!(PageForm::from_spec(&spec).look, "clean");
        assert_eq!(PageForm::new_page().look, "pixel");
    }

    #[test]
    fn an_unknown_look_is_reported() {
        let form = PageForm {
            look: "neon".into(),
            ..valid()
        };
        assert_eq!(
            form.to_spec().unwrap_err().fields().collect::<Vec<_>>(),
            ["look"]
        );
    }

    #[test]
    fn optional_fields_may_be_blank() {
        let form = PageForm {
            description: String::new(),
            accent: String::new(),
            theme: String::new(),
            published: None,
            ..valid()
        };
        let spec = form.to_spec().unwrap();
        assert_eq!(
            (spec.description, spec.accent, spec.theme, spec.published),
            (None, None, Theme::Auto, false)
        );
    }

    #[test]
    fn reports_every_problem() {
        let form = PageForm {
            slug: "Not A Slug".into(),
            title: " ".into(),
            accent: "blue".into(),
            theme: "neon".into(),
            layout: "api\n".into(),
            ..valid()
        };
        let errors = form.to_spec().unwrap_err();
        assert_eq!(
            errors.fields().collect::<Vec<_>>(),
            ["accent", "layout", "slug", "theme", "title"]
        );
        assert_eq!(
            errors.get("layout"),
            Some("Line 1: start with a section heading like `## Services`.")
        );
    }

    #[test]
    fn round_trips_through_from_spec() {
        let spec = valid().to_spec().unwrap();
        assert_eq!(PageForm::from_spec(&spec).to_spec().unwrap(), spec);
    }
}
