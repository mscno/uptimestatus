//! Custom domains for status pages and their verification state.

use std::{fmt, str::FromStr};

use jiff::{SignedDuration, Timestamp};
use uptime_domain::{Hostname, PageSlug};

use crate::{
    Result, Store, StoreError, convert,
    models::{CustomDomainRecord, StatusPageRecord},
};

/// Where a domain is in verification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DomainStatus {
    /// Added; DNS not checked (or not checked since it changed).
    Pending,
    /// DNS points here; the host router serves the page on it.
    Verified,
    /// The last DNS check failed; `last_error` says why.
    Failed,
}

impl DomainStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Verified => "verified",
            Self::Failed => "failed",
        }
    }
}

impl FromStr for DomainStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "verified" => Ok(Self::Verified),
            "failed" => Ok(Self::Failed),
            other => Err(format!("unknown domain status `{other}`")),
        }
    }
}

impl fmt::Display for DomainStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A stored custom domain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomDomain {
    pub id: i64,
    pub page_id: i64,
    pub hostname: Hostname,
    pub status: DomainStatus,
    pub last_error: Option<String>,
    pub verified_at: Option<Timestamp>,
    pub cert_ok_at: Option<Timestamp>,
    pub last_checked_at: Option<Timestamp>,
}

impl Store {
    /// Adds a hostname to a page, pending verification.
    pub async fn add_domain(
        &self,
        page_id: i64,
        hostname: &Hostname,
        now: Timestamp,
    ) -> Result<CustomDomain> {
        let mut db = self.db();
        let mut tx = self.transaction(&mut db).await?;
        if StatusPageRecord::filter_by_id(page_id)
            .first()
            .exec(&mut tx)
            .await?
            .is_none()
        {
            return Err(StoreError::PageNotFound(page_id));
        }
        if CustomDomainRecord::filter_by_hostname(hostname.as_str())
            .first()
            .exec(&mut tx)
            .await?
            .is_some()
        {
            return Err(StoreError::DuplicateDomain(hostname.clone()));
        }
        let record = toasty::create!(CustomDomainRecord {
            page_id: page_id,
            hostname: hostname.as_str(),
            status: DomainStatus::Pending.as_str(),
            created_at: now,
        })
        .exec(&mut tx)
        .await?;
        tx.commit().await?;
        custom_domain(record)
    }

    /// Removes a domain. Returns whether it existed.
    pub async fn remove_domain(&self, id: i64) -> Result<bool> {
        let deleted = toasty::sql::statement(r#"DELETE FROM "custom_domains" WHERE "id" = $1"#)
            .bind(id)
            .exec(&mut self.db())
            .await?;
        Ok(deleted > 0)
    }

    pub async fn domain(&self, id: i64) -> Result<Option<CustomDomain>> {
        CustomDomainRecord::filter_by_id(id)
            .first()
            .exec(&mut self.db())
            .await?
            .map(custom_domain)
            .transpose()
    }

    /// A page's domains, ordered by hostname.
    pub async fn domains_for_page(&self, page_id: i64) -> Result<Vec<CustomDomain>> {
        let mut domains = CustomDomainRecord::filter_by_page_id(page_id)
            .exec(&mut self.db())
            .await?
            .into_iter()
            .map(custom_domain)
            .collect::<Result<Vec<_>>>()?;
        domains.sort_by(|a, b| a.hostname.cmp(&b.hostname));
        Ok(domains)
    }

    /// Domains to (re)check: every unverified one, and verified ones last
    /// checked longer than `recheck_after` ago.
    pub async fn domains_to_check(
        &self,
        now: Timestamp,
        recheck_after: SignedDuration,
    ) -> Result<Vec<CustomDomain>> {
        let stale_before = now.checked_sub(recheck_after).unwrap_or(now);
        CustomDomainRecord::all()
            .exec(&mut self.db())
            .await?
            .into_iter()
            .map(custom_domain)
            .filter(|domain| match domain {
                Ok(domain) => {
                    domain.status != DomainStatus::Verified
                        || domain
                            .last_checked_at
                            .is_none_or(|checked| checked < stale_before)
                }
                Err(_) => true,
            })
            .collect()
    }

    /// Records a DNS check: verified (keeping the first verification time) or failed.
    pub async fn record_domain_check(
        &self,
        id: i64,
        result: Result<(), String>,
        now: Timestamp,
    ) -> Result<CustomDomain> {
        let current = self
            .domain(id)
            .await?
            .ok_or(StoreError::DomainNotFound(id))?;
        let update = CustomDomainRecord::update_by_id(id).last_checked_at(Some(now));
        match result {
            Ok(()) => {
                update
                    .status(DomainStatus::Verified.as_str())
                    .last_error(None::<String>)
                    .verified_at(Some(current.verified_at.unwrap_or(now)))
                    .exec(&mut self.db())
                    .await?;
            }
            Err(reason) => {
                update
                    .status(DomainStatus::Failed.as_str())
                    .last_error(Some(reason))
                    .exec(&mut self.db())
                    .await?;
            }
        }
        self.domain(id).await?.ok_or(StoreError::DomainNotFound(id))
    }

    /// Records the HTTPS pre-warm: a certificate is being served, or why not.
    pub async fn record_certificate(
        &self,
        id: i64,
        result: Result<(), String>,
        now: Timestamp,
    ) -> Result<CustomDomain> {
        let update = CustomDomainRecord::update_by_id(id);
        match result {
            Ok(()) => {
                update
                    .cert_ok_at(Some(now))
                    .last_error(None::<String>)
                    .exec(&mut self.db())
                    .await?
            }
            Err(reason) => {
                update
                    .cert_ok_at(None::<Timestamp>)
                    .last_error(Some(reason))
                    .exec(&mut self.db())
                    .await?
            }
        }
        self.domain(id).await?.ok_or(StoreError::DomainNotFound(id))
    }

    /// Every verified hostname with the slug of the page it serves.
    pub async fn verified_domains(&self) -> Result<Vec<(Hostname, PageSlug)>> {
        let rows = toasty::sql::query(
            r#"SELECT d."hostname", p."slug" FROM "custom_domains" d
               JOIN "status_pages" p ON p."id" = d."page_id"
               WHERE d."status" = 'verified' ORDER BY d."hostname""#,
        )
        .exec(&mut self.db())
        .await?;
        rows.iter()
            .map(|row| {
                let row = convert::Row::new("verified_domains", row)?;
                Ok((
                    convert::parse("custom_domains.hostname", row.string(0)?)?,
                    convert::parse("status_pages.slug", row.string(1)?)?,
                ))
            })
            .collect()
    }
}

fn custom_domain(record: CustomDomainRecord) -> Result<CustomDomain> {
    Ok(CustomDomain {
        id: record.id,
        page_id: record.page_id,
        hostname: convert::parse("custom_domains.hostname", &record.hostname)?,
        status: convert::parse("custom_domains.status", &record.status)?,
        last_error: record.last_error,
        verified_at: record.verified_at,
        cert_ok_at: record.cert_ok_at,
        last_checked_at: record.last_checked_at,
    })
}
