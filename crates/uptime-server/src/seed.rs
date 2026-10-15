//! `uptimestatus seed <file>`: create the monitors, status pages and incident
//! templates in a TOML file ([`ConfigFile`]) that do not exist yet. `export`
//! writes the same format.

use std::path::Path;

use anyhow::Context as _;
use jiff::Timestamp;
use uptime_domain::ConfigFile;
use uptime_runtime::schedule;
use uptime_store::{Store, StoreError};

/// Reads and parses a configuration file.
pub fn load(path: &Path) -> anyhow::Result<ConfigFile> {
    let toml =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    ConfigFile::from_toml(&toml).with_context(|| format!("parsing {}", path.display()))
}

/// What [`apply`] did, by monitor key, page slug and template name.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SeedReport {
    pub created: Vec<String>,
    pub skipped: Vec<String>,
    pub pages_created: Vec<String>,
    pub pages_skipped: Vec<String>,
    pub templates_created: Vec<String>,
    pub templates_skipped: Vec<String>,
}

/// Validates everything, then creates the monitors, pages and incident
/// templates whose key, slug or name do not exist yet (existing ones are left
/// alone).
pub async fn apply(store: &Store, file: &ConfigFile, now: Timestamp) -> anyhow::Result<SeedReport> {
    for spec in &file.monitors {
        spec.policy
            .validate()
            .with_context(|| format!("monitor `{}`", spec.key))?;
        spec.check
            .validate()
            .with_context(|| format!("monitor `{}`", spec.key))?;
    }
    let mut report = SeedReport::default();
    for spec in &file.monitors {
        let first_run = schedule::first_run_at(now, &spec.key, spec.policy.interval);
        match store.create_monitor(spec, first_run).await {
            Ok(_) => report.created.push(spec.key.to_string()),
            Err(StoreError::DuplicateKey(key)) => report.skipped.push(key.to_string()),
            Err(error) => {
                return Err(error).with_context(|| format!("creating monitor `{}`", spec.key));
            }
        }
    }
    for page in &file.pages {
        match store.create_page(page).await {
            Ok(_) => report.pages_created.push(page.slug.to_string()),
            Err(StoreError::DuplicateSlug(slug)) => report.pages_skipped.push(slug.to_string()),
            Err(error) => {
                return Err(error).with_context(|| format!("creating page `{}`", page.slug));
            }
        }
    }
    for template in &file.incident_templates {
        match store.create_template(template).await {
            Ok(created) => report.templates_created.push(created.spec.name),
            Err(StoreError::DuplicateTemplate(name)) => report.templates_skipped.push(name),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("creating incident template `{}`", template.name));
            }
        }
    }
    Ok(report)
}
