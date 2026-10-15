//! Status page subscribers (email and webhook), each page's email settings,
//! and the subscriber outbox.
//!
//! Contact details are sealed (`subscribers.email`, `.webhook_url`,
//! `.webhook_secret`) and found through blind indexes. News reaches
//! subscribers by [`fan_out`]: inside the transaction that records an
//! incident update or maintenance window, one outbox row per interested,
//! confirmed subscriber, keyed so the same news is never queued twice. The
//! store only fans out when subscriptions are enabled
//! ([`Store::with_subscriptions`], i.e. email is configured).
//!
//! Links in emails (confirm, manage, unsubscribe) are signed with the
//! keyring's blind-index keys and a per-subscriber salt, so changing the salt
//! (on unsubscribing or resubscribing) revokes every earlier link.

use std::{collections::HashMap, time::Duration};

use jiff::Timestamp;
use uptime_domain::{
    DomainState, EmailAddress, MonitorKey, PageEvent, Selection, SenderDomain, SubscriberKind,
    SubscriberStatus,
};
use url::Url;

use crate::{
    Backend, Result, Store, StoreError, convert,
    models::{
        MonitorRecord, PageComponentRecord, PageEmailRecord, PageSectionRecord, StatusPageRecord,
        SubscriberOutboxRecord, SubscriberRecord,
    },
    secrets::Sealer,
};

/// Associated data (and reseal targets) of the sealed subscriber columns.
pub(crate) const EMAIL: &str = "subscribers.email";
pub(crate) const WEBHOOK_URL: &str = "subscribers.webhook_url";
pub(crate) const WEBHOOK_SECRET: &str = "subscribers.webhook_secret";
/// Blind index purposes.
const EMAIL_INDEX: &str = "subscriber_email";
const URL_INDEX: &str = "subscriber_webhook_url";
const LINK: &str = "subscriber_link";

/// How long a confirmation link works.
pub const CONFIRM_FOR: jiff::SignedDuration = jiff::SignedDuration::from_hours(7 * 24);
/// How long manage and unsubscribe links in emails work.
pub const MANAGE_FOR: jiff::SignedDuration = jiff::SignedDuration::from_hours(365 * 24);

/// A webhook is disabled after this many failed deliveries in a row …
pub const DISABLE_AFTER_FAILURES: i32 = 10;
/// … spanning at least this long.
pub const DISABLE_AFTER: jiff::SignedDuration = jiff::SignedDuration::from_hours(24);

/// A page's email settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmailSettings {
    pub page_id: i64,
    /// Visitors may subscribe (when email is configured).
    pub subscriptions: bool,
    /// Subscribers hear about automatic incidents too.
    pub notify_automatic: bool,
    pub sender_name: Option<String>,
    /// Sends from here once its domain is verified.
    pub sender_address: Option<EmailAddress>,
    pub reply_to: Option<EmailAddress>,
    pub domain: Option<SenderDomain>,
}

impl EmailSettings {
    fn defaults(page_id: i64) -> Self {
        Self {
            page_id,
            subscriptions: true,
            notify_automatic: true,
            sender_name: None,
            sender_address: None,
            reply_to: None,
            domain: None,
        }
    }

    /// The page's own sender, when its domain is verified.
    pub fn verified_sender(&self) -> Option<&EmailAddress> {
        let address = self.sender_address.as_ref()?;
        self.domain
            .as_ref()
            .filter(|d| d.status == DomainState::Verified && d.name == address.domain())
            .map(|_| address)
    }
}

/// What admins edit (the sending domain is managed separately).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmailSettingsInput {
    pub subscriptions: bool,
    pub notify_automatic: bool,
    pub sender_name: Option<String>,
    pub sender_address: Option<EmailAddress>,
    pub reply_to: Option<EmailAddress>,
}

/// A stored subscription.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subscriber {
    pub id: i64,
    pub page_id: i64,
    pub kind: SubscriberKind,
    /// The subscriber (email), or the webhook's owner.
    pub email: EmailAddress,
    pub webhook_url: Option<Url>,
    pub selection: Selection,
    pub status: SubscriberStatus,
    pub secret_shown: bool,
    pub created_at: Timestamp,
    pub confirmed_at: Option<Timestamp>,
    pub ended_at: Option<Timestamp>,
    pub failures: u32,
    pub failing_since: Option<Timestamp>,
    pub last_delivery_at: Option<Timestamp>,
    pub last_error: Option<String>,
}

/// What a visitor asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NewSubscription {
    Email {
        email: EmailAddress,
        selection: Selection,
    },
    Webhook {
        url: Url,
        /// Confirms the subscription and hears about failures.
        email: EmailAddress,
        selection: Selection,
    },
}

/// What [`Store::subscribe`] did. Visitors are never told which.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Subscribed {
    /// A new subscription, waiting for confirmation (a confirmation is queued).
    Created(Subscriber),
    /// It existed and was not active: confirmation (re)queued.
    Reconfirm(Subscriber),
    /// Already active, or suppressed: nothing sent.
    Unchanged(Subscriber),
}

/// What a signed link may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkPurpose {
    Confirm,
    Manage,
    Unsubscribe,
}

impl LinkPurpose {
    fn code(self) -> &'static str {
        match self {
            Self::Confirm => "c",
            Self::Manage => "m",
            Self::Unsubscribe => "u",
        }
    }
}

/// A queued message to a subscriber.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// News: email or webhook, as the subscriber is.
    Event(PageEvent),
    /// The confirmation email (for webhooks: to the owner).
    Confirm,
    /// Tells a webhook's owner it was disabled after failing.
    Disabled,
}

impl Message {
    fn kind(&self) -> &'static str {
        match self {
            Self::Event(_) => "event",
            Self::Confirm => "confirm",
            Self::Disabled => "disabled",
        }
    }
}

/// A claimed outbox row: send, then report back with
/// [`Store::mark_subscriber_message_sent`] or
/// [`Store::mark_subscriber_message_failed`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubscriberDelivery {
    pub id: i64,
    pub subscriber: Subscriber,
    pub message: Message,
    pub event_key: String,
    /// Including this one.
    pub attempts: u32,
    pub created_at: Timestamp,
}

/// Counts per status, for the console.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubscriberCounts {
    pub active: usize,
    pub pending: usize,
    pub inactive: usize,
}

const CLAIM_SQL: &str = r#"
WITH due AS (
    SELECT "id" FROM "subscriber_outbox"
    WHERE "sent_at" IS NULL AND "failed_at" IS NULL AND "next_attempt_at" <= $1
    ORDER BY "next_attempt_at", "id"
    LIMIT $2
    FOR UPDATE SKIP LOCKED
)
UPDATE "subscriber_outbox" o
SET "attempts" = o."attempts" + 1,
    "next_attempt_at" = $1 + $3 * INTERVAL '1 millisecond'
FROM due
WHERE o."id" = due."id"
RETURNING o."id"
"#;

const CLAIM_SQLITE_SQL: &str = r#"
UPDATE "subscriber_outbox"
SET "attempts" = "attempts" + 1, "next_attempt_at" = ?3
WHERE "id" IN (
    SELECT "id" FROM "subscriber_outbox"
    WHERE "sent_at" IS NULL AND "failed_at" IS NULL AND "next_attempt_at" <= ?1
    ORDER BY "next_attempt_at", "id" LIMIT ?2
)
RETURNING "id"
"#;

impl Store {
    /// Lets the store queue news for subscribers. Off by default: without
    /// email, nothing is queued.
    #[must_use]
    pub fn with_subscriptions(mut self, enabled: bool) -> Self {
        self.subscriptions = enabled;
        self
    }

    /// Whether news is queued for subscribers.
    pub fn subscriptions_enabled(&self) -> bool {
        self.subscriptions
    }

    // ── Email settings ───────────────────────────────────────────────────

    /// A page's email settings (defaults until saved).
    pub async fn email_settings(&self, page_id: i64) -> Result<EmailSettings> {
        let record = PageEmailRecord::filter_by_page_id(page_id)
            .first()
            .exec(&mut self.db())
            .await?;
        match record {
            Some(record) => email_settings(record),
            None => Ok(EmailSettings::defaults(page_id)),
        }
    }

    /// Saves what admins edit. Changing the sender's domain forgets the
    /// registered sending domain.
    pub async fn save_email_settings(
        &self,
        page_id: i64,
        input: &EmailSettingsInput,
    ) -> Result<EmailSettings> {
        if self.page(page_id).await?.is_none() {
            return Err(StoreError::PageNotFound(page_id));
        }
        let current = self.email_settings(page_id).await?;
        let domain = current.domain.filter(|d| {
            input
                .sender_address
                .as_ref()
                .is_some_and(|a| a.domain() == d.name)
        });
        let name = input
            .sender_name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_owned);
        let mut db = self.db();
        let mut tx = self.transaction(&mut db).await?;
        toasty::sql::statement(r#"DELETE FROM "page_email_settings" WHERE "page_id" = $1"#)
            .bind(page_id)
            .exec(&mut tx)
            .await?;
        toasty::create!(PageEmailRecord {
            page_id: page_id,
            subscriptions: input.subscriptions,
            notify_automatic: input.notify_automatic,
            sender_name: name,
            sender_address: input.sender_address.as_ref().map(ToString::to_string),
            reply_to: input.reply_to.as_ref().map(ToString::to_string),
            domain_id: domain.as_ref().map(|d| d.id.clone()),
            domain_name: domain.as_ref().map(|d| d.name.clone()),
            domain_status: domain.as_ref().map(|d| d.status.as_str().to_owned()),
            domain_records: domain.as_ref().map(records_json).transpose()?,
            domain_checked_at: domain.as_ref().and_then(|d| d.checked_at),
        })
        .exec(&mut tx)
        .await?;
        tx.commit().await?;
        self.email_settings(page_id).await
    }

    /// Records the page's sending domain (or forgets it).
    pub async fn set_sender_domain(
        &self,
        page_id: i64,
        domain: Option<&SenderDomain>,
    ) -> Result<EmailSettings> {
        let current = self.email_settings(page_id).await?;
        if PageEmailRecord::filter_by_page_id(page_id)
            .first()
            .exec(&mut self.db())
            .await?
            .is_none()
        {
            self.save_email_settings(
                page_id,
                &EmailSettingsInput {
                    subscriptions: current.subscriptions,
                    notify_automatic: current.notify_automatic,
                    sender_name: current.sender_name.clone(),
                    sender_address: current.sender_address.clone(),
                    reply_to: current.reply_to.clone(),
                },
            )
            .await?;
        }
        PageEmailRecord::update_by_page_id(page_id)
            .domain_id(domain.map(|d| d.id.clone()))
            .domain_name(domain.map(|d| d.name.clone()))
            .domain_status(domain.map(|d| d.status.as_str().to_owned()))
            .domain_records(domain.map(records_json).transpose()?)
            .domain_checked_at(domain.and_then(|d| d.checked_at))
            .exec(&mut self.db())
            .await?;
        self.email_settings(page_id).await
    }

    // ── Subscriptions ────────────────────────────────────────────────────

    /// Subscribes a visitor (double opt-in): a new or inactive subscription
    /// becomes pending and gets a confirmation; an active or suppressed one is
    /// left alone.
    pub async fn subscribe(
        &self,
        page_id: i64,
        request: &NewSubscription,
        now: Timestamp,
    ) -> Result<Subscribed> {
        let (kind, email, url, selection) = match request {
            NewSubscription::Email { email, selection } => {
                (SubscriberKind::Email, email, None, selection)
            }
            NewSubscription::Webhook {
                url,
                email,
                selection,
            } => (SubscriberKind::Webhook, email, Some(url), selection),
        };
        let existing = match url {
            None => self.find_email_subscriber(page_id, email).await?,
            Some(url) => self.find_webhook_subscriber(page_id, url).await?,
        };
        let mut db = self.db();
        let mut tx = self.transaction(&mut db).await?;
        let outcome = match existing {
            Some(found) => match found.status {
                SubscriberStatus::Active | SubscriberStatus::Suppressed => {
                    Subscribed::Unchanged(found)
                }
                SubscriberStatus::Pending
                | SubscriberStatus::Unsubscribed
                | SubscriberStatus::Disabled => {
                    SubscriberRecord::update_by_id(found.id)
                        .status(SubscriberStatus::Pending.as_str())
                        .email(self.sealer().seal(EMAIL, email.as_str()))
                        .email_index(self.sealer().blind_index(EMAIL_INDEX, &email.normalized()))
                        .components(selection_json(selection)?)
                        .link_salt(salt())
                        .ended_at(None::<Timestamp>)
                        .failures(0)
                        .failing_since(None::<Timestamp>)
                        .exec(&mut tx)
                        .await?;
                    queue(&mut tx, found.id, &Message::Confirm, &confirm_key(now), now).await?;
                    Subscribed::Reconfirm(found)
                }
            },
            None => {
                let record = toasty::create!(SubscriberRecord {
                    page_id: page_id,
                    kind: kind.as_str(),
                    email: self.sealer().seal(EMAIL, email.as_str()),
                    email_index: self.sealer().blind_index(EMAIL_INDEX, &email.normalized()),
                    webhook_url: url.map(|u| self.sealer().seal(WEBHOOK_URL, u.as_str())),
                    url_index: url.map(|u| self.sealer().blind_index(URL_INDEX, u.as_str())),
                    webhook_secret: None::<String>,
                    secret_shown: false,
                    components: selection_json(selection)?,
                    status: SubscriberStatus::Pending.as_str(),
                    link_salt: salt(),
                    created_at: now,
                    failures: 0,
                })
                .exec(&mut tx)
                .await?;
                queue(
                    &mut tx,
                    record.id,
                    &Message::Confirm,
                    &confirm_key(now),
                    now,
                )
                .await?;
                Subscribed::Created(subscriber(self.sealer(), record)?)
            }
        };
        tx.commit().await?;
        Ok(match outcome {
            Subscribed::Reconfirm(s) => Subscribed::Reconfirm(self.require_subscriber(s.id).await?),
            other => other,
        })
    }

    async fn find_email_subscriber(
        &self,
        page_id: i64,
        email: &EmailAddress,
    ) -> Result<Option<Subscriber>> {
        let indexes = self
            .sealer()
            .blind_indexes(EMAIL_INDEX, &email.normalized());
        let fields = SubscriberRecord::fields();
        let candidates = SubscriberRecord::filter(
            fields
                .page_id()
                .eq(page_id)
                .and(fields.kind().eq(SubscriberKind::Email.as_str()))
                .and(fields.email_index().in_list(indexes)),
        )
        .exec(&mut self.db())
        .await?;
        for record in candidates {
            let found = subscriber(self.sealer(), record)?;
            if found.email.normalized() == email.normalized() {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    async fn find_webhook_subscriber(&self, page_id: i64, url: &Url) -> Result<Option<Subscriber>> {
        let indexes = self.sealer().blind_indexes(URL_INDEX, url.as_str());
        let fields = SubscriberRecord::fields();
        let candidates = SubscriberRecord::filter(
            fields
                .page_id()
                .eq(page_id)
                .and(fields.url_index().in_list(indexes)),
        )
        .exec(&mut self.db())
        .await?;
        for record in candidates {
            let found = subscriber(self.sealer(), record)?;
            if found.webhook_url.as_ref() == Some(url) {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    pub async fn subscriber(&self, id: i64) -> Result<Option<Subscriber>> {
        SubscriberRecord::filter_by_id(id)
            .first()
            .exec(&mut self.db())
            .await?
            .map(|record| subscriber(self.sealer(), record))
            .transpose()
    }

    async fn require_subscriber(&self, id: i64) -> Result<Subscriber> {
        self.subscriber(id)
            .await?
            .ok_or(StoreError::SubscriberNotFound(id))
    }

    /// A page's subscribers, newest first.
    pub async fn list_subscribers(&self, page_id: i64) -> Result<Vec<Subscriber>> {
        let mut list = SubscriberRecord::filter(SubscriberRecord::fields().page_id().eq(page_id))
            .exec(&mut self.db())
            .await?
            .into_iter()
            .map(|record| subscriber(self.sealer(), record))
            .collect::<Result<Vec<_>>>()?;
        list.sort_by_key(|s| std::cmp::Reverse((s.created_at, s.id)));
        Ok(list)
    }

    /// Active, pending and other subscribers of a page.
    pub async fn subscriber_counts(&self, page_id: i64) -> Result<SubscriberCounts> {
        let mut counts = SubscriberCounts::default();
        for record in SubscriberRecord::filter(SubscriberRecord::fields().page_id().eq(page_id))
            .exec(&mut self.db())
            .await?
        {
            match convert::parse::<SubscriberStatus>("subscribers.status", &record.status)? {
                SubscriberStatus::Active => counts.active += 1,
                SubscriberStatus::Pending => counts.pending += 1,
                _ => counts.inactive += 1,
            }
        }
        Ok(counts)
    }

    /// Confirms a pending subscription (idempotent for active ones). Webhooks
    /// get their signing secret here. Returns `None` if it cannot be confirmed
    /// (unsubscribed, suppressed or disabled since the link was sent).
    pub async fn confirm_subscriber(&self, id: i64, now: Timestamp) -> Result<Option<Subscriber>> {
        let found = self.require_subscriber(id).await?;
        match found.status {
            SubscriberStatus::Active => return Ok(Some(found)),
            SubscriberStatus::Pending => {}
            _ => return Ok(None),
        }
        let mut update = SubscriberRecord::update_by_id(id)
            .status(SubscriberStatus::Active.as_str())
            .confirmed_at(Some(now));
        if found.kind == SubscriberKind::Webhook {
            update = update
                .webhook_secret(Some(self.sealer().seal(WEBHOOK_SECRET, &webhook_secret())))
                .secret_shown(false);
        }
        update.exec(&mut self.db()).await?;
        Ok(Some(self.require_subscriber(id).await?))
    }

    /// Stops sending to a subscriber and revokes its links.
    pub async fn unsubscribe(&self, id: i64, now: Timestamp) -> Result<Subscriber> {
        let found = self.require_subscriber(id).await?;
        if matches!(
            found.status,
            SubscriberStatus::Pending | SubscriberStatus::Active
        ) {
            SubscriberRecord::update_by_id(id)
                .status(SubscriberStatus::Unsubscribed.as_str())
                .ended_at(Some(now))
                .link_salt(salt())
                .exec(&mut self.db())
                .await?;
            self.drop_queued(id).await?;
        }
        self.require_subscriber(id).await
    }

    /// Deletes a subscriber outright (admins).
    pub async fn remove_subscriber(&self, id: i64) -> Result<bool> {
        let deleted = toasty::sql::statement(r#"DELETE FROM "subscribers" WHERE "id" = $1"#)
            .bind(id)
            .exec(&mut self.db())
            .await?;
        Ok(deleted > 0)
    }

    /// Changes which components a subscriber follows.
    pub async fn set_subscriber_selection(
        &self,
        id: i64,
        selection: &Selection,
    ) -> Result<Subscriber> {
        self.require_subscriber(id).await?;
        SubscriberRecord::update_by_id(id)
            .components(selection_json(selection)?)
            .exec(&mut self.db())
            .await?;
        self.require_subscriber(id).await
    }

    /// The webhook's signing secret, the first time it is asked for after
    /// confirmation (or rotation); `None` afterwards.
    pub async fn reveal_webhook_secret(&self, id: i64) -> Result<Option<String>> {
        let Some(record) = SubscriberRecord::filter_by_id(id)
            .first()
            .exec(&mut self.db())
            .await?
        else {
            return Err(StoreError::SubscriberNotFound(id));
        };
        if record.secret_shown {
            return Ok(None);
        }
        let Some(secret) = self
            .sealer()
            .open_opt(WEBHOOK_SECRET, record.webhook_secret.as_deref())?
        else {
            return Ok(None);
        };
        SubscriberRecord::update_by_id(id)
            .secret_shown(true)
            .exec(&mut self.db())
            .await?;
        Ok(Some(secret))
    }

    /// Replaces an active webhook's signing secret; it can be revealed once.
    pub async fn rotate_webhook_secret(&self, id: i64) -> Result<()> {
        let found = self.require_subscriber(id).await?;
        if found.kind != SubscriberKind::Webhook || found.status != SubscriberStatus::Active {
            return Ok(());
        }
        SubscriberRecord::update_by_id(id)
            .webhook_secret(Some(self.sealer().seal(WEBHOOK_SECRET, &webhook_secret())))
            .secret_shown(false)
            .exec(&mut self.db())
            .await?;
        Ok(())
    }

    /// The signing secret deliveries are signed with.
    pub async fn webhook_secret(&self, id: i64) -> Result<Option<String>> {
        let Some(record) = SubscriberRecord::filter_by_id(id)
            .first()
            .exec(&mut self.db())
            .await?
        else {
            return Ok(None);
        };
        Ok(self
            .sealer()
            .open_opt(WEBHOOK_SECRET, record.webhook_secret.as_deref())?)
    }

    /// Never sends to `email` again, on any page (bounces, complaints).
    /// Returns the subscriptions that changed.
    pub async fn suppress_email(
        &self,
        email: &EmailAddress,
        now: Timestamp,
    ) -> Result<Vec<Subscriber>> {
        let indexes = self
            .sealer()
            .blind_indexes(EMAIL_INDEX, &email.normalized());
        let candidates =
            SubscriberRecord::filter(SubscriberRecord::fields().email_index().in_list(indexes))
                .exec(&mut self.db())
                .await?;
        let mut changed = Vec::new();
        for record in candidates {
            let found = subscriber(self.sealer(), record)?;
            if found.email.normalized() != email.normalized()
                || found.status == SubscriberStatus::Suppressed
            {
                continue;
            }
            SubscriberRecord::update_by_id(found.id)
                .status(SubscriberStatus::Suppressed.as_str())
                .ended_at(Some(now))
                .link_salt(salt())
                .exec(&mut self.db())
                .await?;
            self.drop_queued(found.id).await?;
            changed.push(self.require_subscriber(found.id).await?);
        }
        Ok(changed)
    }

    /// Unsubscribes every subscription of `email` on any page (the provider's
    /// unsubscribe event). Returns those that changed.
    pub async fn unsubscribe_email(
        &self,
        email: &EmailAddress,
        now: Timestamp,
    ) -> Result<Vec<Subscriber>> {
        let indexes = self
            .sealer()
            .blind_indexes(EMAIL_INDEX, &email.normalized());
        let candidates = SubscriberRecord::filter(
            SubscriberRecord::fields()
                .email_index()
                .in_list(indexes)
                .and(
                    SubscriberRecord::fields()
                        .kind()
                        .eq(SubscriberKind::Email.as_str()),
                ),
        )
        .exec(&mut self.db())
        .await?;
        let mut changed = Vec::new();
        for record in candidates {
            let found = subscriber(self.sealer(), record)?;
            if found.email.normalized() == email.normalized()
                && matches!(
                    found.status,
                    SubscriberStatus::Active | SubscriberStatus::Pending
                )
            {
                changed.push(self.unsubscribe(found.id, now).await?);
            }
        }
        Ok(changed)
    }

    /// Deletes unsent messages for a subscriber.
    async fn drop_queued(&self, id: i64) -> Result<()> {
        toasty::sql::statement(
            r#"DELETE FROM "subscriber_outbox"
               WHERE "subscriber_id" = $1 AND "sent_at" IS NULL AND "failed_at" IS NULL"#,
        )
        .bind(id)
        .exec(&mut self.db())
        .await?;
        Ok(())
    }

    // ── Signed links ─────────────────────────────────────────────────────

    /// A token for a link that lets the bearer act on `subscriber` until
    /// `expires`.
    pub async fn subscriber_token(
        &self,
        subscriber: i64,
        purpose: LinkPurpose,
        expires: Timestamp,
    ) -> Result<String> {
        let record = SubscriberRecord::filter_by_id(subscriber)
            .first()
            .exec(&mut self.db())
            .await?
            .ok_or(StoreError::SubscriberNotFound(subscriber))?;
        let exp = expires.as_second();
        let payload = link_payload(subscriber, purpose, exp, &record.link_salt);
        let signature = self.sealer().blind_index(LINK, &payload);
        Ok(format!(
            "{subscriber}.{}.{exp}.{}",
            purpose.code(),
            &signature[..32]
        ))
    }

    /// The subscriber a valid, unexpired `token` for `purpose` names.
    pub async fn open_subscriber_token(
        &self,
        token: &str,
        purpose: LinkPurpose,
        now: Timestamp,
    ) -> Result<Option<Subscriber>> {
        let parts: Vec<&str> = token.split('.').collect();
        let [id, code, exp, signature] = parts.as_slice() else {
            return Ok(None);
        };
        let (Ok(id), Ok(exp)) = (id.parse::<i64>(), exp.parse::<i64>()) else {
            return Ok(None);
        };
        if *code != purpose.code() || exp < now.as_second() || signature.len() != 32 {
            return Ok(None);
        }
        let Some(record) = SubscriberRecord::filter_by_id(id)
            .first()
            .exec(&mut self.db())
            .await?
        else {
            return Ok(None);
        };
        let payload = link_payload(id, purpose, exp, &record.link_salt);
        let valid = self
            .sealer()
            .blind_indexes(LINK, &payload)
            .iter()
            .any(|expected| constant_time_eq(&expected.as_bytes()[..32], signature.as_bytes()));
        if !valid {
            return Ok(None);
        }
        Ok(Some(subscriber(self.sealer(), record)?))
    }

    // ── Fan-out and outbox ───────────────────────────────────────────────

    /// Queues `event` for the subscribers of every page showing its monitors,
    /// once per `key` (see [`fan_out`]). For news not tied to an incident or
    /// maintenance window, such as a manual component's status change; pass
    /// `page` for news about one page only.
    pub async fn notify_subscribers(
        &self,
        event: &PageEvent,
        key: &str,
        page: Option<i64>,
        now: Timestamp,
    ) -> Result<usize> {
        if !self.subscriptions {
            return Ok(0);
        }
        let mut db = self.db();
        let mut tx = self.transaction(&mut db).await?;
        let queued = fan_out(&mut tx, true, event, key, page, false, now).await?;
        tx.commit().await?;
        Ok(queued)
    }

    /// Claims up to `limit` due messages, each leased for `lease`.
    pub async fn claim_subscriber_outbox(
        &self,
        now: Timestamp,
        limit: u32,
        lease: Duration,
    ) -> Result<Vec<SubscriberDelivery>> {
        let mut db = self.db();
        let rows = if self.backend() == Backend::Postgres {
            toasty::sql::query(CLAIM_SQL)
                .bind(self.raw_timestamp(now))
                .bind(i64::from(limit))
                .bind(convert::millis(lease))
                .exec(&mut db)
                .await?
        } else {
            let lease_until = now
                .checked_add(lease)
                .map_err(|e| StoreError::corrupt("claim_subscriber_outbox.lease", e))?;
            toasty::sql::query(CLAIM_SQLITE_SQL)
                .bind(self.raw_timestamp(now))
                .bind(i64::from(limit))
                .bind(self.raw_timestamp(lease_until))
                .exec(&mut db)
                .await?
        };
        let ids = rows
            .iter()
            .map(|row| convert::Row::new("claim_subscriber_outbox", row)?.i64(0))
            .collect::<Result<Vec<_>>>()?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let records =
            SubscriberOutboxRecord::filter(SubscriberOutboxRecord::fields().id().in_list(ids))
                .exec(&mut db)
                .await?;
        let subscriber_ids: Vec<i64> = records.iter().map(|r| r.subscriber_id).collect();
        let subscribers: HashMap<i64, Subscriber> =
            SubscriberRecord::filter(SubscriberRecord::fields().id().in_list(subscriber_ids))
                .exec(&mut db)
                .await?
                .into_iter()
                .map(|record| subscriber(self.sealer(), record).map(|s| (s.id, s)))
                .collect::<Result<_>>()?;
        let mut deliveries = Vec::new();
        for record in records {
            let Some(subscriber) = subscribers.get(&record.subscriber_id).cloned() else {
                continue;
            };
            let message = match record.kind.as_str() {
                "confirm" => Message::Confirm,
                "disabled" => Message::Disabled,
                _ => Message::Event(
                    serde_json::from_str(record.payload.as_deref().unwrap_or_default())
                        .map_err(|e| StoreError::corrupt("subscriber_outbox.payload", e))?,
                ),
            };
            deliveries.push(SubscriberDelivery {
                id: record.id,
                subscriber,
                message,
                event_key: record.event_key,
                attempts: convert::count("subscriber_outbox.attempts", record.attempts)?,
                created_at: record.created_at,
            });
        }
        deliveries.sort_by_key(|d| (d.created_at, d.id));
        Ok(deliveries)
    }

    pub async fn mark_subscriber_message_sent(&self, id: i64, now: Timestamp) -> Result<()> {
        SubscriberOutboxRecord::update_by_id(id)
            .sent_at(Some(now))
            .last_error(None::<String>)
            .exec(&mut self.db())
            .await?;
        Ok(())
    }

    /// Records a failed attempt: retried at `retry_at`, or given up when `None`.
    pub async fn mark_subscriber_message_failed(
        &self,
        id: i64,
        error: &str,
        retry_at: Option<Timestamp>,
        now: Timestamp,
    ) -> Result<()> {
        let update = SubscriberOutboxRecord::update_by_id(id).last_error(Some(error.to_owned()));
        match retry_at {
            Some(at) => update.next_attempt_at(at).exec(&mut self.db()).await?,
            None => update.failed_at(Some(now)).exec(&mut self.db()).await?,
        }
        Ok(())
    }

    /// A webhook delivery worked: its failure streak ends.
    pub async fn record_webhook_success(&self, id: i64, now: Timestamp) -> Result<()> {
        SubscriberRecord::update_by_id(id)
            .failures(0)
            .failing_since(None::<Timestamp>)
            .last_delivery_at(Some(now))
            .last_error(None::<String>)
            .exec(&mut self.db())
            .await?;
        Ok(())
    }

    /// A webhook delivery failed. After [`DISABLE_AFTER_FAILURES`] failures
    /// in a row spanning [`DISABLE_AFTER`], the subscription is disabled, its
    /// queue dropped, and its owner told by email. Returns whether it was
    /// disabled now.
    pub async fn record_webhook_failure(
        &self,
        id: i64,
        error: &str,
        now: Timestamp,
    ) -> Result<bool> {
        let found = self.require_subscriber(id).await?;
        let failures = i32::try_from(found.failures)
            .unwrap_or(i32::MAX)
            .saturating_add(1);
        let since = found.failing_since.unwrap_or(now);
        let disable = found.status == SubscriberStatus::Active
            && failures >= DISABLE_AFTER_FAILURES
            && now.duration_since(since) >= DISABLE_AFTER;
        let mut db = self.db();
        let mut tx = self.transaction(&mut db).await?;
        let mut update = SubscriberRecord::update_by_id(id)
            .failures(failures)
            .failing_since(Some(since))
            .last_delivery_at(Some(now))
            .last_error(Some(error.to_owned()));
        if disable {
            update = update
                .status(SubscriberStatus::Disabled.as_str())
                .ended_at(Some(now));
        }
        update.exec(&mut tx).await?;
        if disable {
            toasty::sql::statement(
                r#"DELETE FROM "subscriber_outbox"
                   WHERE "subscriber_id" = $1 AND "sent_at" IS NULL AND "failed_at" IS NULL"#,
            )
            .bind(id)
            .exec(&mut tx)
            .await?;
            let key = format!("disabled:{}", now.as_second());
            queue(&mut tx, id, &Message::Disabled, &key, now).await?;
        }
        tx.commit().await?;
        Ok(disable)
    }

    /// Deletes delivered or given-up messages queued before `cutoff`.
    pub async fn prune_subscriber_messages_before(&self, cutoff: Timestamp) -> Result<u64> {
        Ok(toasty::sql::statement(
            r#"DELETE FROM "subscriber_outbox"
               WHERE "created_at" < $1 AND ("sent_at" IS NOT NULL OR "failed_at" IS NOT NULL)"#,
        )
        .bind(self.raw_timestamp(cutoff))
        .exec(&mut self.db())
        .await?)
    }

    /// Drops unsent news about an incident (it was deleted).
    pub(crate) async fn drop_incident_news(&self, incident: i64) -> Result<()> {
        toasty::sql::statement(
            r#"DELETE FROM "subscriber_outbox"
               WHERE "event_key" LIKE $1 AND "sent_at" IS NULL AND "failed_at" IS NULL"#,
        )
        .bind(format!("incident:{incident}:%"))
        .exec(&mut self.db())
        .await?;
        Ok(())
    }

    /// Recomputes subscriber blind indexes that do not use the active key.
    pub(crate) async fn reindex_subscribers(&self) -> Result<u64> {
        let mut changed = 0;
        for record in SubscriberRecord::all().exec(&mut self.db()).await? {
            let email: EmailAddress =
                convert::parse(EMAIL, &self.sealer().open(EMAIL, &record.email)?)?;
            let email_index = self.sealer().blind_index(EMAIL_INDEX, &email.normalized());
            let url_index = self
                .sealer()
                .open_opt(WEBHOOK_URL, record.webhook_url.as_deref())?
                .map(|u| self.sealer().blind_index(URL_INDEX, &u));
            if email_index != record.email_index || url_index != record.url_index {
                SubscriberRecord::update_by_id(record.id)
                    .email_index(email_index)
                    .url_index(url_index)
                    .exec(&mut self.db())
                    .await?;
                changed += 1;
            }
        }
        Ok(changed)
    }
}

/// Queues `event` for every active subscriber who wants it, on every
/// published page that shows one of its monitors (or only on `page`), unless
/// the subscriber already has a message for `key`. Automatic incidents skip
/// pages that opted out of them. Does nothing unless `enabled`. Returns how
/// many messages were queued.
pub(crate) async fn fan_out(
    tx: &mut toasty::Transaction<'_>,
    enabled: bool,
    event: &PageEvent,
    key: &str,
    page: Option<i64>,
    automatic: bool,
    now: Timestamp,
) -> Result<usize> {
    if !enabled {
        return Ok(0);
    }
    let mut pages = match page {
        Some(page) => vec![page],
        None => pages_showing(tx, event.monitors()).await?,
    };
    pages.sort_unstable();
    pages.dedup();
    let payload = serde_json::to_string(event)
        .map_err(|e| StoreError::corrupt("subscriber_outbox.payload", e))?;
    let mut queued = 0;
    for page_id in pages {
        let Some(page) = StatusPageRecord::filter_by_id(page_id)
            .first()
            .exec(&mut *tx)
            .await?
        else {
            continue;
        };
        let settings = match PageEmailRecord::filter_by_page_id(page_id)
            .first()
            .exec(&mut *tx)
            .await?
        {
            Some(record) => email_settings(record)?,
            None => EmailSettings::defaults(page_id),
        };
        if !page.published || !settings.subscriptions || (automatic && !settings.notify_automatic) {
            continue;
        }
        let fields = SubscriberRecord::fields();
        let subscribers = SubscriberRecord::filter(
            fields
                .page_id()
                .eq(page_id)
                .and(fields.status().eq(SubscriberStatus::Active.as_str())),
        )
        .exec(&mut *tx)
        .await?;
        for record in subscribers {
            let selection = selection(record.components.as_deref())?;
            if !selection.wants(event) {
                continue;
            }
            let outbox = SubscriberOutboxRecord::fields();
            let already = SubscriberOutboxRecord::filter(
                outbox
                    .subscriber_id()
                    .eq(record.id)
                    .and(outbox.event_key().eq(key)),
            )
            .first()
            .exec(&mut *tx)
            .await?
            .is_some();
            if already {
                continue;
            }
            toasty::create!(SubscriberOutboxRecord {
                subscriber_id: record.id,
                kind: "event",
                event_key: key,
                payload: Some(payload.clone()),
                attempts: 0,
                next_attempt_at: now,
                created_at: now,
            })
            .exec(&mut *tx)
            .await?;
            queued += 1;
        }
    }
    Ok(queued)
}

/// Published or not, the pages whose layout includes any of `monitors`.
async fn pages_showing(
    tx: &mut toasty::Transaction<'_>,
    monitors: &[MonitorKey],
) -> Result<Vec<i64>> {
    if monitors.is_empty() {
        return Ok(StatusPageRecord::all()
            .exec(&mut *tx)
            .await?
            .into_iter()
            .map(|p| p.id)
            .collect());
    }
    let keys: Vec<String> = monitors.iter().map(ToString::to_string).collect();
    let monitor_ids: Vec<i64> = MonitorRecord::filter(MonitorRecord::fields().key().in_list(keys))
        .exec(&mut *tx)
        .await?
        .into_iter()
        .map(|m| m.id)
        .collect();
    if monitor_ids.is_empty() {
        return Ok(Vec::new());
    }
    let section_ids: Vec<i64> = PageComponentRecord::filter(
        PageComponentRecord::fields()
            .monitor_id()
            .in_list(monitor_ids),
    )
    .exec(&mut *tx)
    .await?
    .into_iter()
    .map(|c| c.section_id)
    .collect();
    if section_ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(
        PageSectionRecord::filter(PageSectionRecord::fields().id().in_list(section_ids))
            .exec(&mut *tx)
            .await?
            .into_iter()
            .map(|s| s.page_id)
            .collect(),
    )
}

async fn queue(
    tx: &mut toasty::Transaction<'_>,
    subscriber: i64,
    message: &Message,
    key: &str,
    now: Timestamp,
) -> Result<()> {
    let payload = match message {
        Message::Event(event) => Some(
            serde_json::to_string(event)
                .map_err(|e| StoreError::corrupt("subscriber_outbox.payload", e))?,
        ),
        _ => None,
    };
    toasty::create!(SubscriberOutboxRecord {
        subscriber_id: subscriber,
        kind: message.kind(),
        event_key: key,
        payload: payload,
        attempts: 0,
        next_attempt_at: now,
        created_at: now,
    })
    .exec(&mut *tx)
    .await?;
    Ok(())
}

fn confirm_key(now: Timestamp) -> String {
    format!("confirm:{}", now.as_nanosecond())
}

fn link_payload(id: i64, purpose: LinkPurpose, exp: i64, salt: &str) -> String {
    format!("{id}.{}.{exp}.{salt}", purpose.code())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    // The OS RNG is required for link salts and secrets; there is no safe fallback.
    #[allow(clippy::expect_used)]
    getrandom::fill(&mut buf).expect("the OS random number generator is available");
    hex::encode(buf)
}

fn salt() -> String {
    random_hex(16)
}

fn webhook_secret() -> String {
    format!("whsec_{}", random_hex(32))
}

fn selection_json(selection: &Selection) -> Result<Option<String>> {
    if selection.is_all() {
        return Ok(None);
    }
    serde_json::to_string(selection)
        .map(Some)
        .map_err(|e| StoreError::corrupt("subscribers.components", e))
}

fn selection(json: Option<&str>) -> Result<Selection> {
    match json {
        Some(json) => {
            serde_json::from_str(json).map_err(|e| StoreError::corrupt("subscribers.components", e))
        }
        None => Ok(Selection::all()),
    }
}

fn records_json(domain: &SenderDomain) -> Result<String> {
    serde_json::to_string(&domain.records)
        .map_err(|e| StoreError::corrupt("page_email_settings.domain_records", e))
}

fn email_settings(record: PageEmailRecord) -> Result<EmailSettings> {
    let address = |context: &'static str, value: Option<String>| {
        value
            .as_deref()
            .map(|a| convert::parse::<EmailAddress>(context, a))
            .transpose()
    };
    let domain = match (record.domain_id, record.domain_name, record.domain_status) {
        (Some(id), Some(name), Some(status)) => Some(SenderDomain {
            id,
            name,
            status: convert::parse("page_email_settings.domain_status", &status)?,
            records: match record.domain_records.as_deref() {
                Some(json) => serde_json::from_str(json)
                    .map_err(|e| StoreError::corrupt("page_email_settings.domain_records", e))?,
                None => Vec::new(),
            },
            checked_at: record.domain_checked_at,
        }),
        _ => None,
    };
    Ok(EmailSettings {
        page_id: record.page_id,
        subscriptions: record.subscriptions,
        notify_automatic: record.notify_automatic,
        sender_name: record.sender_name,
        sender_address: address("page_email_settings.sender_address", record.sender_address)?,
        reply_to: address("page_email_settings.reply_to", record.reply_to)?,
        domain,
    })
}

fn subscriber(sealer: &Sealer, record: SubscriberRecord) -> Result<Subscriber> {
    Ok(Subscriber {
        id: record.id,
        page_id: record.page_id,
        kind: convert::parse("subscribers.kind", &record.kind)?,
        email: convert::parse(EMAIL, &sealer.open(EMAIL, &record.email)?)?,
        webhook_url: sealer
            .open_opt(WEBHOOK_URL, record.webhook_url.as_deref())?
            .map(|u| convert::parse(WEBHOOK_URL, &u))
            .transpose()?,
        selection: selection(record.components.as_deref())?,
        status: convert::parse("subscribers.status", &record.status)?,
        secret_shown: record.secret_shown,
        created_at: record.created_at,
        confirmed_at: record.confirmed_at,
        ended_at: record.ended_at,
        failures: convert::count("subscribers.failures", record.failures)?,
        failing_since: record.failing_since,
        last_delivery_at: record.last_delivery_at,
        last_error: record.last_error,
    })
}
