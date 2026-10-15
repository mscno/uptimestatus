//! What monthly SLA reports need from storage: a page's report settings,
//! shareable links to private reports, and month-level facts (when a page's
//! history begins, incidents overlapping a month, excused checks).

use jiff::{Timestamp, civil::Date};
use uptime_domain::{MonitorId, Month, SlaTarget};

use crate::{
    Backend, Incident, Result, Store, StoreError,
    convert::{self, Row},
    models::{
        ComponentStatusEventRecord, IncidentRecord, MonitorDailyRecord, ReportLinkRecord,
        StatusPageRecord,
    },
};

/// How a page's reports are shared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReportSettings {
    /// Private reports open only for admins and through report links;
    /// otherwise they are public with the page.
    pub private: bool,
    /// The uptime the page promises.
    pub sla_target: Option<SlaTarget>,
}

/// A link that opens a page's private reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportLink {
    pub id: i64,
    pub page_id: i64,
    /// `None`: every month.
    pub month: Option<Month>,
    pub label: Option<String>,
    pub expires_at: Option<Timestamp>,
    pub created_by: String,
    pub created_at: Timestamp,
}

impl ReportLink {
    /// Whether the link opens `month`'s report at `now`.
    pub fn opens(&self, month: Month, now: Timestamp) -> bool {
        self.month.is_none_or(|only| only == month) && self.expires_at.is_none_or(|at| now < at)
    }
}

impl Store {
    pub async fn report_settings(&self, page_id: i64) -> Result<ReportSettings> {
        let page = StatusPageRecord::filter_by_id(page_id)
            .first()
            .exec(&mut self.db())
            .await?
            .ok_or(StoreError::PageNotFound(page_id))?;
        Ok(ReportSettings {
            private: page.reports_private.unwrap_or(false),
            sla_target: page
                .sla_target
                .as_deref()
                .map(|target| convert::parse("status_pages.sla_target", target))
                .transpose()?,
        })
    }

    pub async fn set_report_settings(&self, page_id: i64, settings: &ReportSettings) -> Result<()> {
        let mut db = self.db();
        if StatusPageRecord::filter_by_id(page_id)
            .first()
            .exec(&mut db)
            .await?
            .is_none()
        {
            return Err(StoreError::PageNotFound(page_id));
        }
        StatusPageRecord::update_by_id(page_id)
            .reports_private(Some(settings.private))
            .sla_target(
                settings
                    .sla_target
                    .map(|t| t.to_string().trim_end_matches('%').to_owned()),
            )
            .exec(&mut db)
            .await?;
        Ok(())
    }

    /// Stores a link by the hash of its key (the key is shown once).
    pub async fn create_report_link(
        &self,
        page_id: i64,
        month: Option<Month>,
        label: Option<&str>,
        expires_at: Option<Timestamp>,
        created_by: &str,
        token_hash: &str,
    ) -> Result<ReportLink> {
        let mut db = self.db();
        if StatusPageRecord::filter_by_id(page_id)
            .first()
            .exec(&mut db)
            .await?
            .is_none()
        {
            return Err(StoreError::PageNotFound(page_id));
        }
        let label = label.map(str::trim).filter(|l| !l.is_empty());
        let record = toasty::create!(ReportLinkRecord {
            page_id: page_id,
            month: month.map(|m| m.to_string()),
            token_hash: token_hash,
            label: label.map(str::to_owned),
            expires_at: expires_at,
            created_by: created_by,
        })
        .exec(&mut db)
        .await?;
        // Read it back: the database keeps `created_at` at its own precision
        // (microseconds on PostgreSQL), not the nanoseconds set on insert.
        link(
            ReportLinkRecord::filter_by_id(record.id)
                .get(&mut db)
                .await?,
        )
    }

    /// A page's links, newest first.
    pub async fn report_links(&self, page_id: i64) -> Result<Vec<ReportLink>> {
        let mut links = ReportLinkRecord::filter_by_page_id(page_id)
            .exec(&mut self.db())
            .await?
            .into_iter()
            .map(link)
            .collect::<Result<Vec<_>>>()?;
        links.sort_by_key(|l| std::cmp::Reverse((l.created_at, l.id)));
        Ok(links)
    }

    /// The link whose key hashes to `token_hash`.
    pub async fn report_link_by_hash(&self, token_hash: &str) -> Result<Option<ReportLink>> {
        ReportLinkRecord::filter_by_token_hash(token_hash)
            .first()
            .exec(&mut self.db())
            .await?
            .map(link)
            .transpose()
    }

    /// Revokes a page's link. Returns whether it existed.
    pub async fn delete_report_link(&self, page_id: i64, id: i64) -> Result<bool> {
        let deleted = toasty::sql::statement(
            r#"DELETE FROM "report_links" WHERE "id" = $1 AND "page_id" = $2"#,
        )
        .bind(id)
        .bind(page_id)
        .exec(&mut self.db())
        .await?;
        Ok(deleted > 0)
    }

    /// The first day with history for a page: a check of one of `monitors`,
    /// or a status set on one of its components.
    pub async fn first_activity(
        &self,
        page_id: i64,
        monitors: &[MonitorId],
    ) -> Result<Option<Date>> {
        let mut db = self.db();
        let ids: Vec<i64> = monitors.iter().map(|m| m.0).collect();
        let fields = MonitorDailyRecord::fields();
        let checked = if ids.is_empty() {
            None
        } else {
            MonitorDailyRecord::filter(fields.monitor_id().in_list(ids))
                .order_by(fields.day().asc())
                .first()
                .exec(&mut db)
                .await?
                .map(|row| row.day)
        };
        let set = ComponentStatusEventRecord::filter_by_page_id(page_id)
            .exec(&mut db)
            .await?
            .into_iter()
            .map(|event| event.at.to_zoned(jiff::tz::TimeZone::UTC).date())
            .min();
        Ok(match (checked, set) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        })
    }

    /// How many raw check results of `monitors` in `[from, to)` an admin
    /// excused from uptime (raw results are pruned with retention).
    pub async fn excused_checks_between(
        &self,
        monitors: &[MonitorId],
        from: Timestamp,
        to: Timestamp,
    ) -> Result<u64> {
        if monitors.is_empty() {
            return Ok(0);
        }
        let mark = |n: usize| {
            if self.backend() == Backend::Postgres {
                format!("${n}")
            } else {
                format!("?{n}")
            }
        };
        let list: Vec<String> = (0..monitors.len()).map(|i| mark(i + 3)).collect();
        let sql = format!(
            r#"SELECT COUNT(*) FROM "check_results"
               WHERE "excused_at" IS NOT NULL AND "checked_at" >= {} AND "checked_at" < {}
                 AND "monitor_id" IN ({})"#,
            mark(1),
            mark(2),
            list.join(", ")
        );
        let mut query = toasty::sql::query(sql)
            .bind(self.raw_timestamp(from))
            .bind(self.raw_timestamp(to));
        for monitor in monitors {
            query = query.bind(monitor.0);
        }
        let rows = query.exec(&mut self.db()).await?;
        let count = match rows.first() {
            Some(row) => Row::new("excused_checks_between", row)?.i64(0)?,
            None => 0,
        };
        Ok(u64::try_from(count).unwrap_or(0))
    }

    /// Public incidents touching `monitors` that overlap `[from, to)`:
    /// started before `to` and not resolved before `from`. Newest first.
    pub async fn incidents_overlapping(
        &self,
        monitors: &[MonitorId],
        from: Timestamp,
        to: Timestamp,
    ) -> Result<Vec<Incident>> {
        let ids = self.incident_ids_for(monitors).await?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let fields = IncidentRecord::fields();
        let records =
            IncidentRecord::filter(fields.id().in_list(ids).and(fields.started_at().lt(to)))
                .exec(&mut self.db())
                .await?
                .into_iter()
                .filter(|incident| incident.resolved_at.is_none_or(|at| at >= from))
                .collect();
        self.public(records).await
    }
}

fn link(record: ReportLinkRecord) -> Result<ReportLink> {
    Ok(ReportLink {
        id: record.id,
        page_id: record.page_id,
        month: record
            .month
            .as_deref()
            .map(|m| convert::parse("report_links.month", m))
            .transpose()?,
        label: record.label,
        expires_at: record.expires_at,
        created_by: record.created_by,
        created_at: record.created_at,
    })
}
