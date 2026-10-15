//! Configuration as code: monitors and status pages in one TOML file, as read
//! by `uptimestatus seed` and written by `uptimestatus export`.

use serde::{Deserialize, Serialize};

use crate::{MonitorSpec, PageSpec};

/// `[[monitor]]` and `[[page]]` tables.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    #[serde(default, rename = "monitor", skip_serializing_if = "Vec::is_empty")]
    pub monitors: Vec<MonitorSpec>,
    #[serde(default, rename = "page", skip_serializing_if = "Vec::is_empty")]
    pub pages: Vec<PageSpec>,
}

impl ConfigFile {
    pub fn from_toml(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        let body = toml::to_string_pretty(self)?;
        Ok(format!(
            "# uptimestatus configuration: `uptimestatus seed <file>` creates what is missing.\n\n{body}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use pretty_assertions::assert_eq;

    use super::*;
    use crate::{
        CheckPolicy, CheckSpec, ComponentSpec, DatabaseCheck, DnsCheck, DnsRecordType, GroupCheck,
        GroupMode, GrpcCheck, HttpCheck, MqttCheck, PingCheck, PushCheck, SectionSpec, TcpCheck,
        Theme,
    };

    fn monitor(key: &str, check: CheckSpec) -> MonitorSpec {
        MonitorSpec {
            key: key.parse().unwrap(),
            name: format!("The {key}"),
            check,
            policy: CheckPolicy {
                degraded_after: Some(Duration::from_millis(1500)),
                ..CheckPolicy::default()
            },
            active: true,
            tags: Vec::new(),
            group: None,
        }
    }

    fn file() -> ConfigFile {
        ConfigFile {
            monitors: vec![
                monitor(
                    "api",
                    CheckSpec::Http(HttpCheck {
                        headers: vec![("accept".into(), "application/json".into())],
                        ..HttpCheck::get("https://api.example.com/health".parse().unwrap())
                    }),
                ),
                monitor(
                    "db",
                    CheckSpec::Tcp(TcpCheck::connect("db.example.com", 5432)),
                ),
                monitor(
                    "spf",
                    CheckSpec::Dns(DnsCheck {
                        name: "example.com".into(),
                        record_type: DnsRecordType::Txt,
                        resolver: Some("1.1.1.1".parse().unwrap()),
                        expect: Some("v=spf1".into()),
                    }),
                ),
                monitor(
                    "backup",
                    CheckSpec::Push(PushCheck {
                        token: "abcdefghijklmnopqrstuvwx".into(),
                    }),
                ),
                monitor(
                    "edge",
                    CheckSpec::Ping(PingCheck {
                        packet_timeout: Duration::from_millis(500),
                        ..PingCheck::host("edge.example.com")
                    }),
                ),
                monitor(
                    "jobs",
                    CheckSpec::Database(DatabaseCheck {
                        query: Some("SELECT count(*) FROM jobs".into()),
                        expect: Some("0".into()),
                        ..DatabaseCheck::url("postgres://monitor:pw@db.example.com/app")
                    }),
                ),
                monitor(
                    "users",
                    CheckSpec::Grpc(GrpcCheck {
                        service: Some("api.v1.Users".into()),
                        metadata: vec![("authorization".into(), "Bearer t".into())],
                        ..GrpcCheck::new("https://grpc.example.com".parse().unwrap())
                    }),
                ),
                monitor(
                    "broker",
                    CheckSpec::Mqtt(MqttCheck {
                        tls: true,
                        topic: Some("health".into()),
                        ..MqttCheck::connect("broker.example.com")
                    }),
                ),
                monitor(
                    "platform",
                    CheckSpec::Group(GroupCheck {
                        path: "prod/eu".into(),
                        mode: GroupMode::All,
                        show_members: true,
                    }),
                ),
            ],
            pages: vec![PageSpec {
                slug: "platform".parse().unwrap(),
                title: "Platform".into(),
                description: Some("Core services".into()),
                accent: Some("#8c6bff".parse().unwrap()),
                theme: Theme::Dark,
                look: Default::default(),
                published: true,
                website: Some(crate::Website {
                    url: "https://privatenpm.com".parse().unwrap(),
                    label: Some("Return to PrivateNPM".into()),
                }),
                sections: vec![SectionSpec {
                    name: "Core".into(),
                    components: vec![
                        ComponentSpec {
                            monitor: "api".parse().unwrap(),
                            label: Some("Public API".into()),
                        },
                        ComponentSpec {
                            monitor: "db".parse().unwrap(),
                            label: None,
                        },
                    ],
                }],
            }],
        }
    }

    #[test]
    fn round_trips_through_toml() {
        let text = file().to_toml().unwrap();
        assert!(
            text.contains("[[monitor]]") && text.contains("[[page]]"),
            "{text}"
        );
        assert!(
            text.contains(r#"interval = "1m""#) && text.contains(r#"packet_timeout = "500ms""#),
            "durations stay human: {text}"
        );
        assert!(text.contains(r#"type = "group""#), "{text}");
        assert_eq!(ConfigFile::from_toml(&text).unwrap(), file());
    }

    #[test]
    fn typos_are_rejected() {
        let error = ConfigFile::from_toml("[[monitr]]\nkey = \"x\"").unwrap_err();
        assert!(error.to_string().contains("monitr"), "{error}");
    }

    #[test]
    fn an_empty_file_is_empty() {
        assert_eq!(ConfigFile::from_toml("").unwrap(), ConfigFile::default());
    }
}
