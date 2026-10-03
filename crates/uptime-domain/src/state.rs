//! The monitor state machine.

use std::{fmt, str::FromStr, time::Duration};

use serde::{Deserialize, Serialize};

use crate::{CheckPolicy, Health};

/// Where a monitor stands after its most recent check.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorState {
    /// Never checked.
    #[default]
    Unknown,
    Up,
    /// Up, but slower than the degraded threshold.
    Degraded,
    /// Failing, but not yet confirmed (retries left).
    Pending,
    /// Confirmed failure.
    Down,
    /// Inside a maintenance window; alerts suppressed.
    Maintenance,
    /// Not scheduled.
    Paused,
}

impl MonitorState {
    pub const ALL: [Self; 7] = [
        Self::Unknown,
        Self::Up,
        Self::Degraded,
        Self::Pending,
        Self::Down,
        Self::Maintenance,
        Self::Paused,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Up => "up",
            Self::Degraded => "degraded",
            Self::Pending => "pending",
            Self::Down => "down",
            Self::Maintenance => "maintenance",
            Self::Paused => "paused",
        }
    }
}

impl fmt::Display for MonitorState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The stored string was not a known [`MonitorState`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown monitor state `{0}`")]
pub struct UnknownState(pub String);

impl FromStr for MonitorState {
    type Err = UnknownState;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|state| state.as_str() == s)
            .ok_or_else(|| UnknownState(s.to_owned()))
    }
}

/// The part of a monitor's runtime the state machine owns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Runtime {
    pub state: MonitorState,
    pub consecutive_failures: u32,
}

/// A change worth notifying someone about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transition {
    WentDown,
    Recovered,
    /// Still down; periodic reminder (`resend_every`).
    Resend,
}

/// The outcome of applying one check result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    pub runtime: Runtime,
    pub transition: Option<Transition>,
    /// How long until the next check (retry interval while PENDING or DOWN).
    pub next_in: Duration,
}

/// Advances a monitor's runtime by one evaluated check.
///
/// `retries = N` means failures 1..=N are PENDING and failure N+1 is DOWN.
pub fn apply(prev: Runtime, policy: &CheckPolicy, health: Health, in_maintenance: bool) -> Step {
    let scheduled = |runtime: Runtime, transition| Step {
        next_in: if matches!(runtime.state, MonitorState::Pending | MonitorState::Down) {
            policy.retry_interval
        } else {
            policy.interval
        },
        runtime,
        transition,
    };

    if in_maintenance {
        return scheduled(
            Runtime {
                state: MonitorState::Maintenance,
                consecutive_failures: 0,
            },
            None,
        );
    }

    let was_down = prev.state == MonitorState::Down;
    match health {
        Health::Up | Health::Degraded => {
            let state = if health == Health::Up {
                MonitorState::Up
            } else {
                MonitorState::Degraded
            };
            let transition = was_down.then_some(Transition::Recovered);
            scheduled(
                Runtime {
                    state,
                    consecutive_failures: 0,
                },
                transition,
            )
        }
        Health::Down => {
            let failures = prev.consecutive_failures.saturating_add(1);
            if was_down {
                let since_down = failures.saturating_sub(policy.retries.saturating_add(1));
                let resend = policy.resend_every > 0
                    && since_down > 0
                    && since_down.is_multiple_of(policy.resend_every);
                scheduled(
                    Runtime {
                        state: MonitorState::Down,
                        consecutive_failures: failures,
                    },
                    resend.then_some(Transition::Resend),
                )
            } else if failures <= policy.retries {
                scheduled(
                    Runtime {
                        state: MonitorState::Pending,
                        consecutive_failures: failures,
                    },
                    None,
                )
            } else {
                scheduled(
                    Runtime {
                        state: MonitorState::Down,
                        consecutive_failures: failures,
                    },
                    Some(Transition::WentDown),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    use super::*;
    use MonitorState::*;

    const INTERVAL: Duration = Duration::from_secs(60);
    const RETRY: Duration = Duration::from_secs(20);

    fn policy(retries: u32, resend_every: u32) -> CheckPolicy {
        CheckPolicy {
            interval: INTERVAL,
            retry_interval: RETRY,
            retries,
            resend_every,
            ..Default::default()
        }
    }

    fn rt(state: MonitorState, consecutive_failures: u32) -> Runtime {
        Runtime {
            state,
            consecutive_failures,
        }
    }

    fn step(
        state: MonitorState,
        failures: u32,
        transition: Option<Transition>,
        next_in: Duration,
    ) -> Step {
        Step {
            runtime: rt(state, failures),
            transition,
            next_in,
        }
    }

    #[rstest]
    // Healthy results from any non-DOWN state: no transition, failures reset.
    #[case(rt(Unknown, 0), Health::Up, step(Up, 0, None, INTERVAL))]
    #[case(rt(Up, 0), Health::Up, step(Up, 0, None, INTERVAL))]
    #[case(rt(Up, 0), Health::Degraded, step(Degraded, 0, None, INTERVAL))]
    #[case(rt(Pending, 2), Health::Up, step(Up, 0, None, INTERVAL))]
    #[case(rt(Maintenance, 0), Health::Up, step(Up, 0, None, INTERVAL))]
    // Recovery from DOWN notifies.
    #[case(
        rt(Down, 7),
        Health::Up,
        step(Up, 0, Some(Transition::Recovered), INTERVAL)
    )]
    #[case(
        rt(Down, 7),
        Health::Degraded,
        step(Degraded, 0, Some(Transition::Recovered), INTERVAL)
    )]
    fn healthy_results(#[case] prev: Runtime, #[case] health: Health, #[case] expected: Step) {
        assert_eq!(apply(prev, &policy(2, 0), health, false), expected);
    }

    #[rstest]
    // retries = 2: failures 1 and 2 are PENDING (retry cadence), failure 3 is DOWN.
    #[case(rt(Up, 0), step(Pending, 1, None, RETRY))]
    #[case(rt(Pending, 1), step(Pending, 2, None, RETRY))]
    #[case(rt(Pending, 2), step(Down, 3, Some(Transition::WentDown), RETRY))]
    #[case(rt(Unknown, 0), step(Pending, 1, None, RETRY))]
    #[case(rt(Degraded, 0), step(Pending, 1, None, RETRY))]
    // Already DOWN: stays DOWN quietly (resend disabled).
    #[case(rt(Down, 3), step(Down, 4, None, RETRY))]
    fn failures_with_retries(#[case] prev: Runtime, #[case] expected: Step) {
        assert_eq!(apply(prev, &policy(2, 0), Health::Down, false), expected);
    }

    #[test]
    fn zero_retries_goes_down_on_first_failure() {
        assert_eq!(
            apply(rt(Up, 0), &policy(0, 0), Health::Down, false),
            step(Down, 1, Some(Transition::WentDown), RETRY)
        );
    }

    #[test]
    fn resend_fires_every_n_failures_after_going_down() {
        // retries = 0, resend_every = 3: DOWN at failure 1, reminders at 4, 7, ...
        let policy = policy(0, 3);
        let mut runtime = rt(Up, 0);
        let mut transitions = Vec::new();
        for _ in 0..8 {
            let step = apply(runtime, &policy, Health::Down, false);
            transitions.push(step.transition);
            runtime = step.runtime;
        }
        assert_eq!(
            transitions,
            vec![
                Some(Transition::WentDown),
                None,
                None,
                Some(Transition::Resend),
                None,
                None,
                Some(Transition::Resend),
                None,
            ]
        );
    }

    #[rstest]
    #[case(Health::Up)]
    #[case(Health::Degraded)]
    #[case(Health::Down)]
    fn maintenance_suppresses_everything(#[case] health: Health) {
        assert_eq!(
            apply(rt(Down, 5), &policy(0, 1), health, true),
            step(Maintenance, 0, None, INTERVAL)
        );
    }

    #[test]
    fn failure_after_maintenance_starts_retry_counting_fresh() {
        assert_eq!(
            apply(rt(Maintenance, 0), &policy(1, 0), Health::Down, false),
            step(Pending, 1, None, RETRY)
        );
    }

    #[test]
    fn states_round_trip_through_strings() {
        for state in MonitorState::ALL {
            assert_eq!(state.as_str().parse::<MonitorState>(), Ok(state));
        }
        assert_eq!(
            "sideways".parse::<MonitorState>(),
            Err(UnknownState("sideways".into()))
        );
    }
}
