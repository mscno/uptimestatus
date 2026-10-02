//! Background work: the check scheduler, the event bus, the dead-man heartbeat,
//! the retention janitor and custom-domain verification.

mod cluster;
mod domains;
mod events;
mod janitor;
pub mod notifier;
mod probe;
pub mod schedule;
mod scheduler;

pub use cluster::{Cluster, Handlers, Reload, apply as apply_bus_message};
pub use domains::{
    BoxFuture, DomainProbe, DomainSink, DomainVerifier, VerifierConfig, VerifyDomain,
};
pub use events::{CheckCompleted, EventBus, Forwarder};
pub use janitor::Janitor;
pub use notifier::{Deliver, Notifier, NotifierConfig};
pub use probe::Probe;
pub use scheduler::{Clock, Scheduler, SchedulerConfig, system_clock};
