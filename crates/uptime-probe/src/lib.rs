//! Executes checks and reports what happened as an [`Observation`].
//!
//! Probes never judge health; that is [`uptime_domain::evaluate`]'s job. They
//! do enforce the [`AddressPolicy`] on every connection they make.

mod cert;
mod database;
mod dns;
mod domains;
mod grpc;
mod http;
mod mqtt;
mod ping;
pub mod policy;
mod prober;
mod resolve;
mod tcp;

pub use domains::DomainInspector;
pub use policy::AddressPolicy;
pub use prober::{Prober, ProberConfig, SetupError};
pub use uptime_domain::Observation;

/// The most response text kept for diagnostics.
const MAX_EXCERPT: usize = 4096;

/// `text` cut to [`MAX_EXCERPT`] bytes, on a character boundary.
pub(crate) fn excerpt(mut text: String) -> String {
    let mut end = text.len().min(MAX_EXCERPT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}
