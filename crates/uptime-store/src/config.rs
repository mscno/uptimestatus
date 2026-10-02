//! Everything an admin configured, as one [`ConfigFile`] (for `export`).

use uptime_domain::ConfigFile;

use crate::{Result, Store, StoreError};

impl Store {
    /// Every monitor and status page, by key and slug.
    pub async fn export_config(&self) -> Result<ConfigFile> {
        let mut monitors: Vec<_> = self
            .list_monitors()
            .await?
            .into_iter()
            .map(|m| m.spec)
            .collect();
        monitors.sort_by(|a, b| a.key.cmp(&b.key));
        let mut pages = Vec::new();
        for summary in self.list_pages().await? {
            let page = self
                .page(summary.id)
                .await?
                .ok_or(StoreError::PageNotFound(summary.id))?;
            pages.push(page.spec);
        }
        pages.sort_by(|a, b| a.slug.cmp(&b.slug));
        Ok(ConfigFile { monitors, pages })
    }
}
