//! Executes checks and reports what happened as an [`Observation`].
//!
//! Probes never judge health; that is [`uptime_domain::evaluate`]'s job. They
//! do enforce the [`AddressPolicy`] on every connection they make.

mod cert;
mod dns;
mod domains;
mod http;
pub mod policy;
mod prober;
mod resolve;
mod tcp;

pub use domains::DomainInspector;
pub use policy::AddressPolicy;
pub use prober::{Prober, ProberConfig, SetupError};
pub use uptime_domain::Observation;
