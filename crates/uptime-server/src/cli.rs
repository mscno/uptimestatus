//! Command-line interface.

use std::{fmt, path::PathBuf, str::FromStr};

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "uptimestatus",
    version,
    about = "Uptime monitoring and status pages"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Serve the public and internal HTTP listeners (the default).
    Serve {
        /// Apply pending migrations before serving (handy in development;
        /// production runs `migrate` as a release step).
        #[arg(long)]
        migrate: bool,
        /// What this instance does: `web` (HTTP), `worker` (checks, alerts,
        /// domain verification) or both, comma-separated.
        #[arg(long, default_value_t = Roles::ALL)]
        roles: Roles,
    },
    /// Apply pending database migrations and exit.
    Migrate,
    /// Create the monitors and status pages in a TOML file (existing keys and
    /// slugs are left alone).
    Seed {
        /// Path to a file of `[[monitor]]` and `[[page]]` tables.
        path: PathBuf,
    },
    /// Write every monitor and status page as TOML (the format `seed` reads).
    Export {
        /// Write here instead of standard output.
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
}

impl Cli {
    /// The subcommand to run; plain `uptimestatus` means `serve`.
    pub fn command(self) -> Command {
        self.command.unwrap_or(Command::Serve {
            migrate: false,
            roles: Roles::ALL,
        })
    }
}

/// The parts of the application an instance runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Roles {
    /// The public and internal HTTP listeners.
    pub web: bool,
    /// The scheduler, janitor, alert sender and domain verifier.
    pub worker: bool,
}

impl Roles {
    pub const ALL: Self = Self {
        web: true,
        worker: true,
    };
}

impl Default for Roles {
    fn default() -> Self {
        Self::ALL
    }
}

impl FromStr for Roles {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut roles = Self {
            web: false,
            worker: false,
        };
        for role in s.split(',').map(str::trim).filter(|r| !r.is_empty()) {
            match role {
                "web" => roles.web = true,
                "worker" => roles.worker = true,
                "all" => roles = Self::ALL,
                other => return Err(format!("unknown role `{other}` (use web, worker or all)")),
            }
        }
        if roles.web || roles.worker {
            Ok(roles)
        } else {
            Err("name at least one role: web, worker".into())
        }
    }
}

impl fmt::Display for Roles {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.web, self.worker) {
            (true, true) => f.write_str("web,worker"),
            (true, false) => f.write_str("web"),
            (false, _) => f.write_str("worker"),
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn parse(args: &[&str]) -> Command {
        Cli::try_parse_from(std::iter::once("uptimestatus").chain(args.iter().copied()))
            .unwrap()
            .command()
    }

    #[test]
    fn defaults_to_serve() {
        assert_eq!(
            parse(&[]),
            Command::Serve {
                migrate: false,
                roles: Roles::ALL
            }
        );
    }

    #[test]
    fn parses_subcommands() {
        assert_eq!(
            parse(&["serve", "--migrate"]),
            Command::Serve {
                migrate: true,
                roles: Roles::ALL
            }
        );
        assert_eq!(
            parse(&["serve", "--roles", "worker"]),
            Command::Serve {
                migrate: false,
                roles: Roles {
                    web: false,
                    worker: true
                }
            }
        );
        assert_eq!(parse(&["migrate"]), Command::Migrate);
        assert_eq!(
            parse(&["export", "-o", "all.toml"]),
            Command::Export {
                output: Some("all.toml".into())
            }
        );
        assert_eq!(
            parse(&["seed", "monitors.toml"]),
            Command::Seed {
                path: "monitors.toml".into()
            }
        );
    }

    #[test]
    fn roles_parse_and_print() {
        assert_eq!("web".parse::<Roles>().unwrap().to_string(), "web");
        assert_eq!("worker, web".parse::<Roles>(), Ok(Roles::ALL));
        assert_eq!("all".parse::<Roles>(), Ok(Roles::ALL));
        assert!("cron".parse::<Roles>().is_err());
        assert!("".parse::<Roles>().is_err());
    }

    #[test]
    fn rejects_unknown_subcommands() {
        assert!(Cli::try_parse_from(["uptimestatus", "explode"]).is_err());
    }

    #[test]
    fn cli_definition_is_consistent() {
        <Cli as clap::CommandFactory>::command().debug_assert();
    }
}
