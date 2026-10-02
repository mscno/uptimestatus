//! Status pages: definition (sections of monitor components) and the data they show.

use std::collections::{BTreeMap, HashMap};

use jiff::civil::Date;
use uptime_domain::{
    ComponentSpec, MonitorId, MonitorKey, PageSlug, PageSpec, SectionSpec, Tally, Website,
};

use crate::{
    Result, Store, StoreError, convert,
    models::{
        MonitorDailyRecord, MonitorRecord, PageComponentRecord, PageSectionRecord, StatusPageRecord,
    },
};

/// A stored status page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub id: i64,
    pub spec: PageSpec,
    /// Uploaded images: file names in the media directory.
    pub logo: Option<String>,
    pub favicon: Option<String>,
}

/// An image slot on a status page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageImage {
    Logo,
    Favicon,
}

/// A row in the list of pages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageSummary {
    pub id: i64,
    pub slug: PageSlug,
    pub title: String,
    pub published: bool,
    pub components: usize,
}

impl Store {
    /// Creates a page. Every referenced monitor must exist.
    #[tracing::instrument(skip_all, fields(slug = %spec.slug))]
    pub async fn create_page(&self, spec: &PageSpec) -> Result<Page> {
        let mut db = self.db();
        let mut tx = self.transaction(&mut db).await?;
        if StatusPageRecord::filter_by_slug(spec.slug.as_str())
            .first()
            .exec(&mut tx)
            .await?
            .is_some()
        {
            return Err(StoreError::DuplicateSlug(spec.slug.clone()));
        }
        let monitor_ids = resolve_monitors(&mut tx, spec).await?;
        let record = toasty::create!(StatusPageRecord {
            slug: spec.slug.as_str(),
            title: spec.title.as_str(),
            description: spec.description.clone(),
            accent: spec.accent.as_ref().map(|a| a.as_str().to_owned()),
            theme: spec.theme.as_str(),
            look: look_column(spec),
            published: spec.published,
            website_url: spec.website.as_ref().map(|w| w.url.to_string()),
            website_label: spec.website.as_ref().and_then(|w| w.label.clone()),
        })
        .exec(&mut tx)
        .await?;
        insert_layout(&mut tx, record.id, spec, &monitor_ids).await?;
        tx.commit().await?;
        Ok(Page {
            id: record.id,
            spec: spec.clone(),
            logo: None,
            favicon: None,
        })
    }

    /// Replaces a page's definition, including its whole layout.
    #[tracing::instrument(skip_all, fields(page = id, slug = %spec.slug))]
    pub async fn update_page(&self, id: i64, spec: &PageSpec) -> Result<Page> {
        let mut db = self.db();
        let mut tx = self.transaction(&mut db).await?;
        let existing = StatusPageRecord::filter_by_id(id)
            .first()
            .exec(&mut tx)
            .await?
            .ok_or(StoreError::PageNotFound(id))?;
        if existing.slug != spec.slug.as_str()
            && StatusPageRecord::filter_by_slug(spec.slug.as_str())
                .first()
                .exec(&mut tx)
                .await?
                .is_some()
        {
            return Err(StoreError::DuplicateSlug(spec.slug.clone()));
        }
        let monitor_ids = resolve_monitors(&mut tx, spec).await?;
        StatusPageRecord::update_by_id(id)
            .slug(spec.slug.as_str())
            .title(spec.title.as_str())
            .description(spec.description.clone())
            .accent(spec.accent.as_ref().map(|a| a.as_str().to_owned()))
            .theme(spec.theme.as_str())
            .look(look_column(spec))
            .published(spec.published)
            .website_url(spec.website.as_ref().map(|w| w.url.to_string()))
            .website_label(spec.website.as_ref().and_then(|w| w.label.clone()))
            .exec(&mut tx)
            .await?;
        // Components go with their sections (ON DELETE CASCADE).
        toasty::sql::statement(r#"DELETE FROM "page_sections" WHERE "page_id" = $1"#)
            .bind(id)
            .exec(&mut tx)
            .await?;
        insert_layout(&mut tx, id, spec, &monitor_ids).await?;
        tx.commit().await?;
        Ok(Page {
            id,
            spec: spec.clone(),
            logo: existing.logo,
            favicon: existing.favicon,
        })
    }

    /// Sets (or clears) one of a page's images. Returns the previous file.
    pub async fn set_page_image(
        &self,
        id: i64,
        slot: PageImage,
        file: Option<&str>,
    ) -> Result<Option<String>> {
        let mut db = self.db();
        let existing = StatusPageRecord::filter_by_id(id)
            .first()
            .exec(&mut db)
            .await?
            .ok_or(StoreError::PageNotFound(id))?;
        let file = file.map(str::to_owned);
        let update = StatusPageRecord::update_by_id(id);
        match slot {
            PageImage::Logo => update.logo(file).exec(&mut db).await?,
            PageImage::Favicon => update.favicon(file).exec(&mut db).await?,
        }
        Ok(match slot {
            PageImage::Logo => existing.logo,
            PageImage::Favicon => existing.favicon,
        })
    }

    /// Whether any page still uses the image `file` (before deleting it).
    pub async fn image_in_use(&self, file: &str) -> Result<bool> {
        Ok(StatusPageRecord::all()
            .exec(&mut self.db())
            .await?
            .iter()
            .any(|page| {
                page.logo.as_deref() == Some(file) || page.favicon.as_deref() == Some(file)
            }))
    }

    /// Deletes a page and its layout. Returns whether it existed.
    pub async fn delete_page(&self, id: i64) -> Result<bool> {
        let deleted = toasty::sql::statement(r#"DELETE FROM "status_pages" WHERE "id" = $1"#)
            .bind(id)
            .exec(&mut self.db())
            .await?;
        Ok(deleted > 0)
    }

    pub async fn page(&self, id: i64) -> Result<Option<Page>> {
        let record = StatusPageRecord::filter_by_id(id)
            .first()
            .exec(&mut self.db())
            .await?;
        match record {
            Some(record) => Ok(Some(self.load_page(record).await?)),
            None => Ok(None),
        }
    }

    pub async fn page_by_slug(&self, slug: &str) -> Result<Option<Page>> {
        let record = StatusPageRecord::filter_by_slug(slug)
            .first()
            .exec(&mut self.db())
            .await?;
        match record {
            Some(record) => Ok(Some(self.load_page(record).await?)),
            None => Ok(None),
        }
    }

    /// All pages, ordered by slug.
    pub async fn list_pages(&self) -> Result<Vec<PageSummary>> {
        let mut db = self.db();
        let pages = StatusPageRecord::all()
            .order_by(StatusPageRecord::fields().slug().asc())
            .exec(&mut db)
            .await?;
        let counts = toasty::sql::query(
            r#"SELECT s."page_id", COUNT(c."id") FROM "page_sections" s
               JOIN "page_components" c ON c."section_id" = s."id" GROUP BY s."page_id""#,
        )
        .exec(&mut db)
        .await?
        .iter()
        .map(|row| {
            let row = convert::Row::new("list_pages", row)?;
            Ok((row.i64(0)?, row.i64(1)?))
        })
        .collect::<Result<HashMap<i64, i64>>>()?;
        pages
            .into_iter()
            .map(|page| {
                Ok(PageSummary {
                    id: page.id,
                    slug: convert::parse("status_pages.slug", &page.slug)?,
                    title: page.title,
                    published: page.published,
                    components: usize::try_from(counts.get(&page.id).copied().unwrap_or(0))
                        .unwrap_or(0),
                })
            })
            .collect()
    }

    /// Per-day counts for each monitor between `from` and `to` (inclusive, UTC days).
    pub async fn daily_tallies(
        &self,
        monitors: &[MonitorId],
        from: Date,
        to: Date,
    ) -> Result<HashMap<MonitorId, BTreeMap<Date, Tally>>> {
        let ids: Vec<i64> = monitors.iter().map(|id| id.0).collect();
        let fields = MonitorDailyRecord::fields();
        let rows = MonitorDailyRecord::filter(
            fields
                .monitor_id()
                .in_list(ids)
                .and(fields.day().ge(from).and(fields.day().le(to))),
        )
        .exec(&mut self.db())
        .await?;
        let count =
            |value: i64| u64::try_from(value).map_err(|e| StoreError::corrupt("monitor_daily", e));
        let mut tallies: HashMap<MonitorId, BTreeMap<Date, Tally>> = HashMap::new();
        for row in rows {
            let tally = Tally {
                total: count(row.total)?,
                up: count(row.up)?,
                degraded: count(row.degraded)?,
                pending: count(row.pending)?,
                down: count(row.down)?,
                maintenance: count(row.maintenance)?,
            };
            tallies
                .entry(MonitorId(row.monitor_id))
                .or_default()
                .insert(row.day, tally);
        }
        Ok(tallies)
    }

    async fn load_page(&self, record: StatusPageRecord) -> Result<Page> {
        let mut db = self.db();
        let mut sections = PageSectionRecord::filter_by_page_id(record.id)
            .exec(&mut db)
            .await?;
        sections.sort_by_key(|section| section.position);
        let section_ids: Vec<i64> = sections.iter().map(|s| s.id).collect();
        let mut components = PageComponentRecord::filter(
            PageComponentRecord::fields()
                .section_id()
                .in_list(section_ids),
        )
        .exec(&mut db)
        .await?;
        components.sort_by_key(|component| component.position);
        let monitor_ids: Vec<i64> = components.iter().map(|c| c.monitor_id).collect();
        let keys: HashMap<i64, MonitorKey> =
            MonitorRecord::filter(MonitorRecord::fields().id().in_list(monitor_ids))
                .exec(&mut db)
                .await?
                .into_iter()
                .map(|monitor| Ok((monitor.id, convert::parse("monitors.key", &monitor.key)?)))
                .collect::<Result<_>>()?;

        let sections = sections
            .into_iter()
            .map(|section| SectionSpec {
                name: section.name,
                components: components
                    .iter()
                    .filter(|c| c.section_id == section.id)
                    .filter_map(|c| {
                        keys.get(&c.monitor_id).map(|key| ComponentSpec {
                            monitor: key.clone(),
                            label: c.label.clone(),
                        })
                    })
                    .collect(),
            })
            .collect();
        Ok(Page {
            id: record.id,
            spec: PageSpec {
                slug: convert::parse("status_pages.slug", &record.slug)?,
                title: record.title,
                description: record.description,
                accent: record
                    .accent
                    .as_deref()
                    .map(|a| convert::parse("status_pages.accent", a))
                    .transpose()?,
                theme: convert::parse("status_pages.theme", &record.theme)?,
                look: record
                    .look
                    .as_deref()
                    .map(|look| convert::parse("status_pages.look", look))
                    .transpose()?
                    .unwrap_or_default(),
                published: record.published,
                website: record
                    .website_url
                    .as_deref()
                    .map(|url| {
                        Ok::<_, StoreError>(Website {
                            url: convert::parse("status_pages.website_url", url)?,
                            label: record.website_label.clone(),
                        })
                    })
                    .transpose()?,
                sections,
            },
            logo: record.logo,
            favicon: record.favicon,
        })
    }
}

/// Maps each monitor key in the spec to its id, or lists the unknown ones.
async fn resolve_monitors(
    tx: &mut toasty::Transaction<'_>,
    spec: &PageSpec,
) -> Result<HashMap<MonitorKey, i64>> {
    let keys: Vec<MonitorKey> = spec.monitor_keys().cloned().collect();
    resolve_keys(tx, &keys).await
}

/// Monitor ids by key; every key must exist.
pub(crate) async fn resolve_keys(
    tx: &mut toasty::Transaction<'_>,
    keys: &[MonitorKey],
) -> Result<HashMap<MonitorKey, i64>> {
    let wanted: Vec<String> = keys.iter().map(ToString::to_string).collect();
    let found: HashMap<MonitorKey, i64> =
        MonitorRecord::filter(MonitorRecord::fields().key().in_list(wanted))
            .exec(tx)
            .await?
            .into_iter()
            .map(|monitor| {
                Ok((
                    convert::parse::<MonitorKey>("monitors.key", &monitor.key)?,
                    monitor.id,
                ))
            })
            .collect::<Result<_>>()?;
    let mut missing: Vec<MonitorKey> = keys
        .iter()
        .filter(|key| !found.contains_key(*key))
        .cloned()
        .collect();
    missing.dedup();
    if missing.is_empty() {
        Ok(found)
    } else {
        Err(StoreError::UnknownMonitors(missing))
    }
}

async fn insert_layout(
    tx: &mut toasty::Transaction<'_>,
    page_id: i64,
    spec: &PageSpec,
    monitor_ids: &HashMap<MonitorKey, i64>,
) -> Result<()> {
    for (section_position, section) in spec.sections.iter().enumerate() {
        let record = toasty::create!(PageSectionRecord {
            page_id: page_id,
            name: section.name.as_str(),
            position: i32::try_from(section_position).unwrap_or(i32::MAX),
        })
        .exec(tx)
        .await?;
        for (position, component) in section.components.iter().enumerate() {
            toasty::create!(PageComponentRecord {
                section_id: record.id,
                monitor_id: monitor_ids[&component.monitor],
                label: component.label.clone(),
                position: i32::try_from(position).unwrap_or(i32::MAX),
            })
            .exec(tx)
            .await?;
        }
    }
    Ok(())
}

/// `NULL` for the default look, so existing pages need no backfill.
fn look_column(spec: &PageSpec) -> Option<String> {
    (!spec.look.is_default()).then(|| spec.look.as_str().to_owned())
}
