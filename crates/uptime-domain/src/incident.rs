//! Incidents: what admins tell visitors about an outage, as a timeline of
//! updates. Automatic incidents follow DOWN and recovery.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

/// How bad it is.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Impact {
    None,
    #[default]
    Minor,
    Major,
    Critical,
}

/// Where the response stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentStatus {
    #[default]
    Investigating,
    Identified,
    Monitoring,
    Resolved,
}

/// Opened by an admin, or by the scheduler on DOWN.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentKind {
    Manual,
    Auto,
}

macro_rules! text_enum {
    ($ty:ty { $($variant:ident => $text:literal, $label:literal),+ $(,)? }) => {
        impl $ty {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }

            pub fn label(self) -> &'static str {
                match self { $(Self::$variant => $label),+ }
            }
        }

        impl FromStr for $ty {
            type Err = String;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($text => Ok(Self::$variant),)+
                    other => Err(format!("unknown value `{other}`")),
                }
            }
        }

        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

text_enum!(Impact {
    None => "none", "No impact",
    Minor => "minor", "Minor",
    Major => "major", "Major",
    Critical => "critical", "Critical",
});

text_enum!(IncidentStatus {
    Investigating => "investigating", "Investigating",
    Identified => "identified", "Identified",
    Monitoring => "monitoring", "Monitoring",
    Resolved => "resolved", "Resolved",
});

text_enum!(IncidentKind {
    Manual => "manual", "Manual",
    Auto => "auto", "Automatic",
});

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn values_round_trip_through_text() {
        for impact in Impact::ALL {
            assert_eq!(impact.as_str().parse::<Impact>(), Ok(*impact));
        }
        for status in IncidentStatus::ALL {
            assert_eq!(status.as_str().parse::<IncidentStatus>(), Ok(*status));
        }
        for kind in IncidentKind::ALL {
            assert_eq!(kind.as_str().parse::<IncidentKind>(), Ok(*kind));
        }
        assert!("bad".parse::<Impact>().is_err());
    }

    #[test]
    fn impacts_are_ordered_by_severity() {
        assert!(Impact::None < Impact::Minor);
        assert!(Impact::Major < Impact::Critical);
    }

    #[test]
    fn labels_are_human() {
        assert_eq!(IncidentStatus::Identified.label(), "Identified");
        assert_eq!(Impact::None.label(), "No impact");
    }
}
