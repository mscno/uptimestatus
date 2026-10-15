//! Email through Lettermint: sending (project token), sending domains (team
//! token) and verifying Lettermint's webhook deliveries.
//!
//! Error messages kept in delivery history never quote the API's text, which
//! can contain recipient addresses: only the status and the API's error code.

use std::time::Duration;

use jiff::Timestamp;
use lettermint::{
    Lettermint, SendOptions,
    types::{GetDomainQuery, GetDomainQueryIncludeItem, StoreDomainData},
};
use uptime_domain::{DnsRecord, DomainState, EmailAddress, Mailbox, SenderDomain};

use crate::DeliveryError;

/// How long one API call may take.
const TIMEOUT: Duration = Duration::from_secs(20);

/// One message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Email {
    pub from: Mailbox,
    /// At most 10 (Lettermint's limit per message).
    pub to: Vec<EmailAddress>,
    pub reply_to: Option<EmailAddress>,
    pub subject: String,
    pub html: String,
    pub text: String,
    /// Extra headers, e.g. `List-Unsubscribe`.
    pub headers: Vec<(String, String)>,
    /// Makes retries safe: Lettermint sends one message per key.
    pub idempotency_key: String,
    /// Groups messages in Lettermint's dashboard.
    pub tag: String,
}

/// How to reach Lettermint.
#[derive(Clone, Debug)]
pub struct MailerConfig {
    pub project_token: String,
    /// Registers per-page sending domains; optional.
    pub team_token: Option<String>,
    /// The sender for alerts and pages without a verified domain of their own.
    pub default_from: Mailbox,
    /// API base URL override (tests).
    pub api_url: Option<String>,
}

/// A failed setup or Team API call.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct MailError(pub String);

/// Sends email. Cheap to clone.
#[derive(Clone)]
pub struct Mailer {
    client: Lettermint,
    default_from: Mailbox,
    manages_domains: bool,
}

impl std::fmt::Debug for Mailer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mailer")
            .field("default_from", &self.default_from.to_string())
            .field("manages_domains", &self.manages_domains)
            .finish_non_exhaustive()
    }
}

impl Mailer {
    pub fn new(config: MailerConfig) -> Result<Self, MailError> {
        let mut builder = Lettermint::builder()
            .sending_token(config.project_token)
            .timeout(TIMEOUT);
        let manages_domains = config.team_token.is_some();
        if let Some(team) = config.team_token {
            builder = builder.team_token(team);
        }
        if let Some(url) = config.api_url {
            builder = builder.base_url(url);
        }
        let client = builder
            .build()
            .map_err(|e| MailError(format!("configuring Lettermint: {e}")))?;
        Ok(Self {
            client,
            default_from: config.default_from,
            manages_domains,
        })
    }

    pub fn default_from(&self) -> &Mailbox {
        &self.default_from
    }

    /// Whether sending domains can be registered through the Team API.
    pub fn manages_domains(&self) -> bool {
        self.manages_domains
    }

    /// Sends one message.
    pub async fn send(&self, email: &Email) -> Result<(), DeliveryError> {
        let to: Vec<String> = email.to.iter().map(ToString::to_string).collect();
        let mut message = self
            .client
            .emails()
            .compose()
            .from(email.from.to_string())
            .to(to)
            .subject(&email.subject)
            .html(&email.html)
            .text(&email.text)
            .tag(&email.tag);
        if let Some(reply_to) = &email.reply_to {
            message = message.reply_to(reply_to.to_string());
        }
        if !email.headers.is_empty() {
            message = message.headers(email.headers.clone());
        }
        message
            .send(SendOptions::new().idempotency_key(&email.idempotency_key))
            .await
            .map(|_| ())
            .map_err(|error| delivery_error(&error))
    }

    /// Registers `domain` for sending (or finds it, if it exists) and returns
    /// the DNS records to publish.
    pub async fn register_domain(&self, domain: &str) -> Result<SenderDomain, MailError> {
        let domains = self.client.domains();
        let created = domains
            .create(&StoreDomainData {
                domain: domain.to_owned(),
            })
            .await;
        let id = match created {
            Ok(data) => data.id,
            Err(error) if error.status() == Some(422) || error.status() == Some(409) => {
                // Already registered (perhaps in the dashboard): find it.
                self.find_domain(domain)
                    .await?
                    .ok_or_else(|| MailError(api_message("adding the domain", &error)))?
            }
            Err(error) => return Err(MailError(api_message("adding the domain", &error))),
        };
        self.domain(&id).await
    }

    async fn find_domain(&self, domain: &str) -> Result<Option<String>, MailError> {
        let page = self
            .client
            .domains()
            .list(&lettermint::types::ListDomainsQuery {
                filter_domain: Some(domain.to_owned()),
                ..Default::default()
            })
            .await
            .map_err(|e| MailError(api_message("listing domains", &e)))?;
        Ok(page
            .data
            .into_iter()
            .find(|d| d.domain.eq_ignore_ascii_case(domain))
            .map(|d| d.id))
    }

    /// A registered domain with its DNS records and their status.
    pub async fn domain(&self, id: &str) -> Result<SenderDomain, MailError> {
        let data = self
            .client
            .domains()
            .retrieve(
                id,
                &GetDomainQuery {
                    include: Some(vec![GetDomainQueryIncludeItem::DnsRecords]),
                },
            )
            .await
            .map_err(|e| MailError(api_message("reading the domain", &e)))?;
        let records: Vec<DnsRecord> = data
            .dns_records
            .unwrap_or_default()
            .into_iter()
            .map(|r| DnsRecord {
                kind: r.r#type.as_str().to_owned(),
                name: r.fqdn,
                value: r.content,
                purpose: r.purpose.as_str().to_owned(),
                status: r.status.as_str().to_owned(),
                required: r.required_for_verification,
            })
            .collect();
        let required: Vec<&DnsRecord> = records.iter().filter(|r| r.required).collect();
        let status = if !required.is_empty() && required.iter().all(|r| r.status == "active") {
            DomainState::Verified
        } else if required.iter().any(|r| r.status == "failed") {
            DomainState::Failed
        } else {
            DomainState::Pending
        };
        Ok(SenderDomain {
            id: data.id,
            name: data.domain,
            status,
            records,
            checked_at: Some(Timestamp::now()),
        })
    }

    /// Asks Lettermint to check the domain's DNS now, then reads it back.
    /// Records that are not in place yet are not an error.
    pub async fn verify_domain(&self, id: &str) -> Result<SenderDomain, MailError> {
        match self.client.domains().verify_dns_records(id).await {
            Ok(_) => {}
            Err(error) if error.status() == Some(422) => {}
            Err(error) => return Err(MailError(api_message("verifying the domain", &error))),
        }
        self.domain(id).await
    }
}

/// Verifies a Lettermint webhook delivery (`X-Lettermint-Signature` and
/// `X-Lettermint-Delivery`) and returns its event and data.
pub fn verify_webhook(
    secret: &str,
    body: &[u8],
    signature: Option<&str>,
    delivery: Option<&str>,
) -> Result<(String, serde_json::Value), MailError> {
    let webhook =
        lettermint::Webhook::new(secret).map_err(|e| MailError(format!("webhook secret: {e}")))?;
    let signature = signature.ok_or_else(|| MailError("missing signature".into()))?;
    let payload = webhook
        .verify_signature(body, signature, delivery)
        .map_err(|e| MailError(e.message().to_owned()))?;
    Ok((payload.event.as_str().to_owned(), payload.data))
}

fn api_message(action: &str, error: &lettermint::Error) -> String {
    match (error.status(), error.code()) {
        (Some(status), Some(code)) => {
            format!("{action} failed: Lettermint answered {status} ({code})")
        }
        (Some(status), None) => format!("{action} failed: Lettermint answered {status}"),
        _ => format!("{action} failed: {error}"),
    }
}

/// Retry policy: rejected messages and credential problems are permanent;
/// rate limits, server errors and network trouble are retried.
fn delivery_error(error: &lettermint::Error) -> DeliveryError {
    let message = api_message("sending email", error);
    match error.status() {
        Some(429) => DeliveryError::retry_after(
            message,
            error
                .api_error()
                .and_then(lettermint::ApiError::retry_after),
        ),
        Some(status) if (500..600).contains(&status) => DeliveryError::transient(message),
        Some(_) => DeliveryError::permanent(message),
        None => match error {
            lettermint::Error::Config(_) | lettermint::Error::InvalidInput(_) => {
                DeliveryError::permanent(message)
            }
            _ => DeliveryError::transient(message),
        },
    }
}
