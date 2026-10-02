use std::{future::Future, time::Duration};

use uptime_domain::{CheckSpec, Observation};

/// Something that can execute a check. Implemented by [`uptime_probe::Prober`];
/// tests substitute scripted probes.
pub trait Probe: Send + Sync + 'static {
    fn probe(
        &self,
        check: &CheckSpec,
        timeout: Duration,
    ) -> impl Future<Output = Observation> + Send;
}

impl Probe for uptime_probe::Prober {
    fn probe(
        &self,
        check: &CheckSpec,
        timeout: Duration,
    ) -> impl Future<Output = Observation> + Send {
        uptime_probe::Prober::probe(self, check, timeout)
    }
}
