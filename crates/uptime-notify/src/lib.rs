//! Alert delivery: one [`Alert`] rendered for Slack (Block Kit), Discord
//! (embeds) or a generic JSON webhook (optionally HMAC-signed), then POSTed.

mod render;
mod send;

pub use render::{discord_payload, signature, slack_payload, webhook_payload};
pub use send::{DeliveryError, Sender};
pub use uptime_domain::{Alert, ChannelKind, ChannelSpec};
