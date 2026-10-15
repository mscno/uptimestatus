//! Alert delivery: one [`Alert`] rendered for Slack (Block Kit), Discord
//! (embeds), a generic JSON webhook (optionally HMAC-signed) or email
//! (Lettermint), then sent. Also status page subscribers' branded emails
//! ([`letters`]) and signed webhooks.

pub mod letters;
mod mail;
mod render;
mod send;

pub use mail::{Email, MailError, Mailer, MailerConfig, verify_webhook};
pub use render::{discord_payload, signature, slack_payload, webhook_payload};
pub use send::{DeliveryError, Sender};
pub use uptime_domain::{Alert, ChannelKind, ChannelSpec};
