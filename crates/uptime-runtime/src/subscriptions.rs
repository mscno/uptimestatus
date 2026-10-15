//! Delivers status page subscribers' messages: claims due rows of the
//! subscriber outbox, renders them under the page's branding, and sends them
//! by email (Lettermint) or as signed webhooks, with the alert backoff. Also
//! announces maintenance as windows start and end.
//!
//! Emails go from the page's own sender once its domain is verified;
//! otherwise from the default sender, named after the page, with the page's
//! address as Reply-To. Links point at the page's verified custom domain when
//! it has one.

use std::{collections::HashMap, time::Duration};

use jiff::{SignedDuration, Timestamp};
use tokio_util::sync::CancellationToken;
use uptime_domain::{EmailAddress, Look, Mailbox, PageEvent, SubscriberKind, SubscriberStatus};
use uptime_notify::{
    DeliveryError, Email, Sender,
    letters::{self, Branding, Letter},
};
use uptime_store::{
    CONFIRM_FOR, DomainStatus, LinkPurpose, MANAGE_FOR, Message, Page, Store, StoreError,
    SubscriberDelivery,
};
use url::Url;

use crate::{Clock, NotifierConfig, notifier::backoff, system_clock};

/// How far back the first maintenance pass looks after a start.
const MAINTENANCE_CATCH_UP: SignedDuration = SignedDuration::from_mins(5);

/// The subscriber outbox sender.
pub struct Courier {
    store: Store,
    sender: Sender,
    app_url: Url,
    config: NotifierConfig,
    clock: Clock,
}

/// A page as its letters present it.
#[derive(Clone, Debug)]
pub struct PageMail {
    pub slug: String,
    pub brand: Branding,
    pub from: Mailbox,
    pub reply_to: Option<EmailAddress>,
    /// Component key → label on this page.
    pub names: HashMap<String, String>,
}

/// What letters from page `id` look like: its branding, sender and
/// component names. `None` if the page is gone or email is not configured.
pub async fn page_mail(
    store: &Store,
    mailer: Option<&uptime_notify::Mailer>,
    app_url: &Url,
    id: i64,
) -> Result<Option<PageMail>, StoreError> {
    let Some(page) = store.page(id).await? else {
        return Ok(None);
    };
    let page_url = page_url(store, app_url, &page).await?;
    let settings = store.email_settings(id).await?;
    let default_from = mailer.map(|m| m.default_from().clone());
    let (from, reply_to) = match (settings.verified_sender(), default_from) {
        (Some(own), _) => (
            Mailbox::new(
                Some(settings.sender_name.as_deref().unwrap_or(&page.spec.title)),
                own.clone(),
            ),
            settings.reply_to.clone(),
        ),
        (None, Some(default)) => (
            default.named(&page.spec.title),
            settings
                .reply_to
                .clone()
                .or(settings.sender_address.clone()),
        ),
        (None, None) => return Ok(None),
    };
    let names = component_names(store, &page).await?;
    let logo_url = page
        .logo
        .as_deref()
        .and_then(|file| app_url.join(&format!("/media/{file}")).ok());
    Ok(Some(PageMail {
        slug: page.spec.slug.to_string(),
        brand: Branding {
            title: page.spec.title.clone(),
            logo_url,
            accent: page.spec.accent.as_ref().map(|a| a.as_str().to_owned()),
            pixel: page.spec.look != Look::Clean,
            page_url,
            manage_url: None,
            unsubscribe_url: None,
        },
        from,
        reply_to,
        names,
    }))
}

/// The page's verified custom domain, or its address on the console host.
pub async fn page_url(store: &Store, app_url: &Url, page: &Page) -> Result<Url, StoreError> {
    let custom = store
        .domains_for_page(page.id)
        .await?
        .into_iter()
        .find(|d| d.status == DomainStatus::Verified)
        .and_then(|d| Url::parse(&format!("https://{}/", d.hostname)).ok());
    Ok(custom.unwrap_or_else(|| {
        app_url
            .join(&format!("/s/{}/", page.spec.slug))
            .unwrap_or_else(|_| app_url.clone())
    }))
}

async fn component_names(
    store: &Store,
    page: &Page,
) -> Result<HashMap<String, String>, StoreError> {
    let monitors: HashMap<String, String> = store
        .list_monitors()
        .await?
        .into_iter()
        .map(|m| (m.spec.key.to_string(), m.spec.name))
        .collect();
    let mut names = HashMap::new();
    for component in page.spec.sections.iter().flat_map(|s| &s.components) {
        let key = component.key().to_string();
        let name = component
            .label()
            .map(str::to_owned)
            .or_else(|| monitors.get(&key).cloned())
            .unwrap_or_else(|| key.clone());
        names.entry(key).or_insert(name);
    }
    Ok(names)
}

impl Courier {
    /// `sender` must carry a mailer (email configured). `app_url` is the
    /// console's address, used for pages without a custom domain.
    pub fn new(store: Store, sender: Sender, app_url: Url, config: NotifierConfig) -> Self {
        Self {
            store,
            sender,
            app_url,
            config,
            clock: system_clock(),
        }
    }

    pub fn with_clock(self, clock: Clock) -> Self {
        Self { clock, ..self }
    }

    /// Sends every due message (in batches). Returns how many were attempted.
    pub async fn run_once(&self) -> Result<usize, StoreError> {
        let mut attempted = 0;
        let mut pages: HashMap<i64, Option<PageMail>> = HashMap::new();
        loop {
            let now = (self.clock)();
            let batch = self
                .store
                .claim_subscriber_outbox(now, self.config.batch, self.config.lease)
                .await?;
            if batch.is_empty() {
                return Ok(attempted);
            }
            attempted += batch.len();
            for delivery in &batch {
                let page_id = delivery.subscriber.page_id;
                let page = match pages.entry(page_id) {
                    std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::hash_map::Entry::Vacant(entry) => entry.insert(
                        page_mail(&self.store, self.sender.mailer(), &self.app_url, page_id)
                            .await?,
                    ),
                };
                self.attempt(delivery, page.as_ref()).await?;
            }
            if attempted >= 10 * self.config.batch as usize {
                return Ok(attempted);
            }
        }
    }

    #[tracing::instrument(skip_all, fields(message = delivery.id, subscriber = delivery.subscriber.id, key = %delivery.event_key))]
    async fn attempt(
        &self,
        delivery: &SubscriberDelivery,
        page: Option<&PageMail>,
    ) -> Result<(), StoreError> {
        let now = (self.clock)();
        let outcome = match page {
            Some(page) => self.deliver(delivery, page, now).await,
            None => Err(DeliveryError {
                message: "the status page is gone".into(),
                permanent: true,
                retry_after: None,
            }),
        };
        let subscriber = &delivery.subscriber;
        let webhook_event = subscriber.kind == SubscriberKind::Webhook
            && matches!(delivery.message, Message::Event(_));
        match outcome {
            Ok(Sent::Delivered) => {
                if webhook_event {
                    self.store
                        .record_webhook_success(subscriber.id, now)
                        .await?;
                }
                tracing::info!(attempts = delivery.attempts, "subscriber message sent");
                self.store
                    .mark_subscriber_message_sent(delivery.id, now)
                    .await
            }
            Ok(Sent::Skipped(why)) => {
                tracing::info!(why, "subscriber message skipped");
                self.store
                    .mark_subscriber_message_failed(delivery.id, why, None, now)
                    .await
            }
            Err(error) => {
                if webhook_event
                    && self
                        .store
                        .record_webhook_failure(subscriber.id, &error.message, now)
                        .await?
                {
                    tracing::warn!("webhook subscription disabled after repeated failures");
                    return self
                        .store
                        .mark_subscriber_message_failed(delivery.id, &error.message, None, now)
                        .await;
                }
                let retry_at = self.retry_at(delivery, &error, now);
                match retry_at {
                    Some(at) => {
                        tracing::warn!(error = %error.message, attempts = delivery.attempts, retry_at = %at, "subscriber message failed; will retry")
                    }
                    None => {
                        tracing::error!(error = %error.message, attempts = delivery.attempts, "subscriber message failed; giving up")
                    }
                }
                self.store
                    .mark_subscriber_message_failed(delivery.id, &error.message, retry_at, now)
                    .await
            }
        }
    }

    fn retry_at(
        &self,
        delivery: &SubscriberDelivery,
        error: &DeliveryError,
        now: Timestamp,
    ) -> Option<Timestamp> {
        if error.permanent {
            return None;
        }
        let wait = backoff(delivery.attempts).max(error.retry_after.unwrap_or_default());
        let at = now.checked_add(SignedDuration::try_from(wait).ok()?).ok()?;
        let give_up = SignedDuration::try_from(self.config.give_up_after).ok()?;
        let deadline = delivery.created_at.checked_add(give_up).ok()?;
        (at <= deadline).then_some(at)
    }

    async fn deliver(
        &self,
        delivery: &SubscriberDelivery,
        page: &PageMail,
        now: Timestamp,
    ) -> Result<Sent, DeliveryError> {
        let subscriber = &delivery.subscriber;
        let store_error = |e: StoreError| DeliveryError {
            message: format!("reading the subscription failed: {e}"),
            permanent: false,
            retry_after: None,
        };
        let webhook_host = subscriber
            .webhook_url
            .as_ref()
            .and_then(Url::host_str)
            .map(str::to_owned);
        let letter = match &delivery.message {
            Message::Confirm => {
                if subscriber.status != SubscriberStatus::Pending {
                    return Ok(Sent::Skipped("already confirmed or ended"));
                }
                let token = self
                    .store
                    .subscriber_token(subscriber.id, LinkPurpose::Confirm, now + CONFIRM_FOR)
                    .await
                    .map_err(store_error)?;
                let url = page_link(&page.brand.page_url, "subscriptions/confirm", &token);
                letters::confirm_letter(&page.brand, subscriber.kind, &url, webhook_host.as_deref())
            }
            Message::Disabled => letters::disabled_letter(
                &page.brand,
                webhook_host.as_deref().unwrap_or("your endpoint"),
                subscriber.last_error.as_deref(),
            ),
            Message::Event(event) => {
                if subscriber.status != SubscriberStatus::Active {
                    return Ok(Sent::Skipped("subscription is not active"));
                }
                if subscriber.kind == SubscriberKind::Webhook {
                    return self.post_webhook(delivery, page, event).await;
                }
                let brand = self
                    .personal_brand(page, subscriber.id, now)
                    .await
                    .map_err(store_error)?;
                let letter = letters::event_letter(&brand, event, &page.names);
                return self
                    .email(delivery, page, letter, brand.unsubscribe_url.as_ref())
                    .await;
            }
        };
        self.email(delivery, page, letter, None).await
    }

    /// The page's branding with this subscriber's manage and unsubscribe links.
    async fn personal_brand(
        &self,
        page: &PageMail,
        subscriber: i64,
        now: Timestamp,
    ) -> Result<Branding, StoreError> {
        let manage = self
            .store
            .subscriber_token(subscriber, LinkPurpose::Manage, now + MANAGE_FOR)
            .await?;
        let unsubscribe = self
            .store
            .subscriber_token(subscriber, LinkPurpose::Unsubscribe, now + MANAGE_FOR)
            .await?;
        Ok(Branding {
            manage_url: Some(page_link(
                &page.brand.page_url,
                "subscriptions/manage",
                &manage,
            )),
            unsubscribe_url: Some(page_link(
                &page.brand.page_url,
                "subscriptions/unsubscribe",
                &unsubscribe,
            )),
            ..page.brand.clone()
        })
    }

    async fn email(
        &self,
        delivery: &SubscriberDelivery,
        page: &PageMail,
        letter: Letter,
        unsubscribe: Option<&Url>,
    ) -> Result<Sent, DeliveryError> {
        let mailer = self.sender.mailer().ok_or_else(|| DeliveryError {
            message: "email is not configured".into(),
            permanent: true,
            retry_after: None,
        })?;
        let mut headers = Vec::new();
        if let Some(url) = unsubscribe {
            headers.push(("List-Unsubscribe".to_owned(), format!("<{url}>")));
            headers.push((
                "List-Unsubscribe-Post".to_owned(),
                "List-Unsubscribe=One-Click".to_owned(),
            ));
        }
        let tag = match delivery.message {
            Message::Confirm => "subscription-confirm",
            Message::Disabled => "subscription-disabled",
            Message::Event(_) => "status-update",
        };
        mailer
            .send(&Email {
                from: page.from.clone(),
                to: vec![delivery.subscriber.email.clone()],
                reply_to: page.reply_to.clone(),
                subject: letter.subject,
                html: letter.html,
                text: letter.text,
                headers,
                idempotency_key: format!("subscriber-message-{}", delivery.id),
                tag: tag.into(),
            })
            .await?;
        Ok(Sent::Delivered)
    }

    async fn post_webhook(
        &self,
        delivery: &SubscriberDelivery,
        page: &PageMail,
        event: &PageEvent,
    ) -> Result<Sent, DeliveryError> {
        let subscriber = &delivery.subscriber;
        let Some(url) = &subscriber.webhook_url else {
            return Ok(Sent::Skipped("no webhook URL"));
        };
        let secret = self
            .store
            .webhook_secret(subscriber.id)
            .await
            .map_err(|e| DeliveryError {
                message: format!("reading the signing secret failed: {e}"),
                permanent: false,
                retry_after: None,
            })?
            .unwrap_or_default();
        let payload = letters::subscriber_webhook_payload(
            &delivery.id.to_string(),
            &page.slug,
            &page.brand,
            event,
            &page.names,
        );
        let body = serde_json::to_vec(&payload).map_err(|e| DeliveryError {
            message: format!("encoding the payload: {e}"),
            permanent: true,
            retry_after: None,
        })?;
        self.sender
            .post_signed(url, event.type_name(), &secret, body)
            .await?;
        Ok(Sent::Delivered)
    }

    /// Delivers due messages every `poll_interval` and announces maintenance
    /// transitions, until `shutdown` is cancelled.
    pub async fn run(self, shutdown: CancellationToken) {
        let mut since = (self.clock)() - MAINTENANCE_CATCH_UP;
        loop {
            let now = (self.clock)();
            match self.store.announce_maintenance(since, now).await {
                Ok(queued) => {
                    since = now;
                    if queued > 0 {
                        tracing::info!(queued, "maintenance announced to subscribers");
                    }
                }
                Err(error) => {
                    tracing::error!(error = %uptime_domain::Report(&error), "announcing maintenance failed");
                }
            }
            if let Err(error) = self.run_once().await {
                tracing::error!(error = %uptime_domain::Report(&error), "sending subscriber messages failed");
            }
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(self.config.poll_interval) => {}
            }
        }
    }
}

/// What came of one attempt.
enum Sent {
    Delivered,
    /// Not sent and not retried (the subscription changed since it was queued).
    Skipped(&'static str),
}

/// `{page}/{path}?token=…`.
fn page_link(page: &Url, path: &str, token: &str) -> Url {
    let mut url = page.join(path).unwrap_or_else(|_| page.clone());
    url.query_pairs_mut().clear().append_pair("token", token);
    url
}

/// The poll interval subscribers' messages use by default.
pub const POLL_INTERVAL: Duration = Duration::from_secs(10);
