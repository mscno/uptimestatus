//! Monitor groups: a slash-separated path (`prod/eu`) that arranges monitors
//! into a tree, each node summarising its children.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{CheckError, DownReason, FailureKind, Health, MonitorState, Verdict};

/// The deepest group path.
pub const MAX_DEPTH: usize = 4;
/// The longest path segment.
pub const MAX_SEGMENT_LEN: usize = 32;

/// Why a group path was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GroupError {
    #[error("group names use a-z, 0-9, `-` and `_` (up to 32 characters each): `{0}`")]
    Invalid(String),
    #[error("groups can be nested at most 4 levels deep")]
    TooDeep,
}

/// Cleans `prod / EU` into `prod/eu`; blank means no group.
pub fn normalize_group(input: &str) -> Result<Option<String>, GroupError> {
    let mut segments = Vec::new();
    for raw in input.trim().trim_matches('/').split('/') {
        let segment = raw.trim().to_lowercase();
        if segment.is_empty() {
            if segments.is_empty() && raw.is_empty() && input.trim().trim_matches('/').is_empty() {
                return Ok(None);
            }
            return Err(GroupError::Invalid(raw.to_owned()));
        }
        let valid = segment.len() <= MAX_SEGMENT_LEN
            && segment
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_".contains(&b));
        if !valid {
            return Err(GroupError::Invalid(segment));
        }
        segments.push(segment);
    }
    if segments.len() > MAX_DEPTH {
        return Err(GroupError::TooDeep);
    }
    Ok(Some(segments.join("/")))
}

/// The state a group shows: the worst of its children's (paused ones do not count).
pub fn rollup(states: impl IntoIterator<Item = MonitorState>) -> MonitorState {
    let mut worst = MonitorState::Paused;
    let rank = |state: MonitorState| match state {
        MonitorState::Down => 5,
        MonitorState::Degraded | MonitorState::Pending => 4,
        MonitorState::Maintenance => 3,
        MonitorState::Up => 2,
        MonitorState::Unknown => 1,
        MonitorState::Paused => 0,
    };
    for state in states {
        if rank(state) > rank(worst) {
            worst = if matches!(state, MonitorState::Pending) {
                MonitorState::Degraded
            } else {
                state
            };
        }
    }
    worst
}

/// How a group monitor combines its members' states.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupMode {
    /// DOWN when any member is down; DEGRADED while any member is degraded or
    /// failing unconfirmed (pending).
    #[default]
    Any,
    /// DOWN only when every member is down; DEGRADED while some are.
    All,
}

impl GroupMode {
    pub const ALL: [Self; 2] = [Self::Any, Self::All];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::All => "all",
        }
    }

    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl std::str::FromStr for GroupMode {
    type Err = CheckError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|mode| mode.as_str() == s)
            .ok_or(CheckError::GroupMode)
    }
}

/// A monitor whose state is derived from the monitors in a group folder
/// (`path` and everything below it). It probes nothing itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupCheck {
    /// The folder summarised, e.g. `prod/eu`.
    pub path: String,
    #[serde(default, skip_serializing_if = "GroupMode::is_default")]
    pub mode: GroupMode,
    /// Status pages list the members (names and states) under the group.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub show_members: bool,
}

impl GroupCheck {
    /// A group over `path` in the default mode.
    pub fn path(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            mode: GroupMode::default(),
            show_members: false,
        }
    }

    /// The path must already be in normalized form.
    pub fn validate(&self) -> Result<(), CheckError> {
        match normalize_group(&self.path) {
            Ok(Some(path)) if path == self.path => Ok(()),
            _ => Err(CheckError::GroupPath),
        }
    }

    /// Whether a monitor in folder `group` belongs to this group.
    pub fn covers(&self, group: Option<&str>) -> bool {
        group.is_some_and(|group| {
            group == self.path
                || group
                    .strip_prefix(self.path.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    }
}

/// A monitor as group evaluation sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Member<'a> {
    pub key: &'a str,
    pub name: &'a str,
    /// The folder the monitor is in.
    pub group: Option<&'a str>,
    /// Set when the monitor is itself a group monitor.
    pub summarises: Option<&'a GroupCheck>,
    pub state: MonitorState,
}

/// The keys of the monitors `of` directly includes (none unless it is a group).
fn includes<'a>(of: &Member<'a>, all: &[Member<'a>]) -> Vec<&'a str> {
    let Some(group) = of.summarises else {
        return Vec::new();
    };
    all.iter()
        .filter(|m| m.key != of.key && group.covers(m.group))
        .map(|m| m.key)
        .collect()
}

/// Whether `from` includes `target`, directly or through nested groups.
fn reaches(from: &str, target: &str, all: &[Member<'_>]) -> bool {
    let mut seen = vec![from];
    let mut stack = vec![from];
    while let Some(key) = stack.pop() {
        let Some(member) = all.iter().find(|m| m.key == key) else {
            continue;
        };
        for next in includes(member, all) {
            if next == target {
                return true;
            }
            if !seen.contains(&next) {
                seen.push(next);
                stack.push(next);
            }
        }
    }
    false
}

/// The members of `group` (one of `all`): every monitor in or below its
/// path except itself and any nested group that includes it back.
pub fn members<'a>(group: &Member<'a>, all: &[Member<'a>]) -> Vec<Member<'a>> {
    let Some(check) = group.summarises else {
        return Vec::new();
    };
    all.iter()
        .filter(|m| m.key != group.key && check.covers(m.group))
        .filter(|m| m.summarises.is_none() || !reaches(m.key, group.key, all))
        .copied()
        .collect()
}

/// Group monitors that include each other (directly or through other
/// groups): the keys around the first loop found, sorted.
pub fn find_cycle(all: &[Member<'_>]) -> Option<Vec<String>> {
    let start = all
        .iter()
        .find(|m| m.summarises.is_some() && reaches(m.key, m.key, all))?;
    let mut keys: Vec<String> = all
        .iter()
        .filter(|m| {
            m.summarises.is_some()
                && (m.key == start.key
                    || (reaches(start.key, m.key, all) && reaches(m.key, start.key, all)))
        })
        .map(|m| m.key.to_owned())
        .collect();
    keys.sort();
    Some(keys)
}

/// What a group's members amount to on one evaluation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupOutcome {
    /// Nothing to judge (no active, checked members): the group shows UNKNOWN.
    Empty(String),
    /// Every counted member is in maintenance, so the group is too.
    Maintenance,
    Judged(Verdict),
}

/// Judges a group from its members' states. Paused and never-checked
/// members do not count; members in maintenance count only when all are.
pub fn judge(mode: GroupMode, members: &[Member<'_>]) -> GroupOutcome {
    let counted: Vec<&Member<'_>> = members
        .iter()
        .filter(|m| {
            !matches!(
                m.state,
                MonitorState::Paused | MonitorState::Unknown | MonitorState::Maintenance
            )
        })
        .collect();
    if counted.is_empty() {
        return if members.iter().any(|m| m.state == MonitorState::Maintenance) {
            GroupOutcome::Maintenance
        } else if members.is_empty() {
            GroupOutcome::Empty("the group has no members".into())
        } else {
            GroupOutcome::Empty("no member of the group is active and checked yet".into())
        };
    }
    let down: Vec<&str> = counted
        .iter()
        .filter(|m| m.state == MonitorState::Down)
        .map(|m| m.name)
        .collect();
    let troubled = counted.iter().any(|m| {
        matches!(
            m.state,
            MonitorState::Down | MonitorState::Pending | MonitorState::Degraded
        )
    });
    let is_down = match mode {
        GroupMode::Any => !down.is_empty(),
        GroupMode::All => down.len() == counted.len(),
    };
    let verdict = |health, reason| Verdict {
        health,
        latency: None,
        status_code: None,
        response_body: None,
        reason,
    };
    GroupOutcome::Judged(if is_down {
        verdict(
            Health::Down,
            Some(DownReason::Probe {
                kind: FailureKind::Reported,
                message: format!(
                    "{} of {} members down: {}",
                    down.len(),
                    counted.len(),
                    down.join(", ")
                ),
            }),
        )
    } else if troubled {
        verdict(Health::Degraded, None)
    } else {
        verdict(Health::Up, None)
    })
}

/// One group in the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node<T> {
    /// The last path segment.
    pub name: String,
    /// The full path, e.g. `prod/eu`.
    pub path: String,
    pub items: Vec<T>,
    pub children: Vec<Node<T>>,
}

impl<T> Node<T> {
    /// Every item in this group and below.
    pub fn all_items(&self) -> Vec<&T> {
        let mut all: Vec<&T> = self.items.iter().collect();
        for child in &self.children {
            all.extend(child.all_items());
        }
        all
    }
}

/// Arranges `(group, item)` pairs into the ungrouped items and a tree of
/// groups (each level in name order; items keep their input order).
pub fn tree<T>(items: impl IntoIterator<Item = (Option<String>, T)>) -> (Vec<T>, Vec<Node<T>>) {
    let mut ungrouped = Vec::new();
    let mut by_path: BTreeMap<String, Vec<T>> = BTreeMap::new();
    for (group, item) in items {
        match group.filter(|g| !g.is_empty()) {
            Some(path) => by_path.entry(path).or_default().push(item),
            None => ungrouped.push(item),
        }
    }
    let mut roots: Vec<Node<T>> = Vec::new();
    for (path, items) in by_path {
        let mut level = &mut roots;
        let mut so_far = String::new();
        let segments: Vec<&str> = path.split('/').collect();
        for (depth, segment) in segments.iter().enumerate() {
            if !so_far.is_empty() {
                so_far.push('/');
            }
            so_far.push_str(segment);
            let at = match level.iter().position(|n| n.name == *segment) {
                Some(at) => at,
                None => {
                    level.push(Node {
                        name: (*segment).to_owned(),
                        path: so_far.clone(),
                        items: Vec::new(),
                        children: Vec::new(),
                    });
                    level.len() - 1
                }
            };
            if depth + 1 == segments.len() {
                level[at].items.extend(items);
                break;
            }
            level = &mut level[at].children;
        }
    }
    (ungrouped, roots)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("", Ok(None))]
    #[case("  / ", Ok(None))]
    #[case("Prod", Ok(Some("prod".to_owned())))]
    #[case(" prod / EU ", Ok(Some("prod/eu".to_owned())))]
    #[case("/a/b/c/d/", Ok(Some("a/b/c/d".to_owned())))]
    #[case("a/b/c/d/e", Err(GroupError::TooDeep))]
    #[case("a//b", Err(GroupError::Invalid(String::new())))]
    #[case("no way", Err(GroupError::Invalid("no way".to_owned())))]
    fn groups_are_normalized(
        #[case] input: &str,
        #[case] expected: Result<Option<String>, GroupError>,
    ) {
        assert_eq!(normalize_group(input), expected);
    }

    #[rstest]
    #[case(&[MonitorState::Up, MonitorState::Down], MonitorState::Down)]
    #[case(&[MonitorState::Up, MonitorState::Pending], MonitorState::Degraded)]
    #[case(&[MonitorState::Up, MonitorState::Maintenance], MonitorState::Maintenance)]
    #[case(&[MonitorState::Up, MonitorState::Paused], MonitorState::Up)]
    #[case(&[MonitorState::Paused, MonitorState::Paused], MonitorState::Paused)]
    #[case(&[MonitorState::Unknown, MonitorState::Up], MonitorState::Up)]
    #[case(&[], MonitorState::Paused)]
    fn a_group_shows_its_worst_child(
        #[case] states: &[MonitorState],
        #[case] expected: MonitorState,
    ) {
        assert_eq!(rollup(states.iter().copied()), expected);
    }

    fn member<'a>(
        key: &'a str,
        group: Option<&'a str>,
        summarises: Option<&'a GroupCheck>,
        state: MonitorState,
    ) -> Member<'a> {
        Member {
            key,
            name: key,
            group,
            summarises,
            state,
        }
    }

    #[test]
    fn group_paths_must_be_normalized() {
        assert_eq!(GroupCheck::path("prod/eu").validate(), Ok(()));
        for bad in ["", "Prod", "prod/", "a/b/c/d/e"] {
            assert_eq!(GroupCheck::path(bad).validate(), Err(CheckError::GroupPath));
        }
    }

    #[test]
    fn groups_cover_their_folder_and_below() {
        let group = GroupCheck::path("prod");
        assert!(group.covers(Some("prod")));
        assert!(group.covers(Some("prod/eu")));
        assert!(!group.covers(Some("production")));
        assert!(!group.covers(Some("dev")));
        assert!(!group.covers(None));
    }

    #[test]
    fn members_exclude_the_group_itself_and_groups_that_include_it_back() {
        let prod = GroupCheck::path("prod");
        let eu = GroupCheck::path("prod/eu");
        // `back` sits under prod but summarises `ops`, where prod's group lives.
        let back = GroupCheck::path("ops");
        let all = [
            member("prod-group", Some("ops"), Some(&prod), MonitorState::Up),
            member("api", Some("prod"), None, MonitorState::Up),
            member("db", Some("prod/eu"), None, MonitorState::Down),
            member("eu-group", Some("prod"), Some(&eu), MonitorState::Down),
            member("back", Some("prod"), Some(&back), MonitorState::Up),
            member("web", Some("dev"), None, MonitorState::Up),
            member("loose", None, None, MonitorState::Up),
        ];
        let keys: Vec<&str> = members(&all[0], &all).iter().map(|m| m.key).collect();
        assert_eq!(keys, ["api", "db", "eu-group"], "`back` would be a cycle");
        let keys: Vec<&str> = members(&all[3], &all).iter().map(|m| m.key).collect();
        assert_eq!(keys, ["db"]);
        assert_eq!(
            find_cycle(&all),
            Some(vec!["back".to_owned(), "prod-group".to_owned()])
        );
        assert_eq!(find_cycle(&all[..4]), None, "nesting alone is fine");
        // A group inside its own folder is not a cycle; it just skips itself.
        let own = [member("g", Some("prod"), Some(&prod), MonitorState::Up)];
        assert_eq!(find_cycle(&own), None);
        assert!(members(&own[0], &own).is_empty());
    }

    fn judged(mode: GroupMode, states: &[MonitorState]) -> GroupOutcome {
        let all: Vec<Member<'_>> = states
            .iter()
            .zip(["a", "b", "c", "d"])
            .map(|(state, key)| member(key, Some("g"), None, *state))
            .collect();
        judge(mode, &all)
    }

    fn health(outcome: &GroupOutcome) -> Option<Health> {
        match outcome {
            GroupOutcome::Judged(verdict) => Some(verdict.health),
            _ => None,
        }
    }

    #[rstest]
    #[case(GroupMode::Any, &[MonitorState::Up, MonitorState::Up], Health::Up)]
    #[case(GroupMode::Any, &[MonitorState::Up, MonitorState::Degraded], Health::Degraded)]
    #[case(GroupMode::Any, &[MonitorState::Up, MonitorState::Pending], Health::Degraded)]
    #[case(GroupMode::Any, &[MonitorState::Up, MonitorState::Down], Health::Down)]
    #[case(GroupMode::Any, &[MonitorState::Down, MonitorState::Paused], Health::Down)]
    #[case(GroupMode::Any, &[MonitorState::Up, MonitorState::Maintenance], Health::Up)]
    #[case(GroupMode::All, &[MonitorState::Up, MonitorState::Down], Health::Degraded)]
    #[case(GroupMode::All, &[MonitorState::Down, MonitorState::Down], Health::Down)]
    #[case(GroupMode::All, &[MonitorState::Down, MonitorState::Unknown], Health::Down)]
    #[case(GroupMode::All, &[MonitorState::Up, MonitorState::Up], Health::Up)]
    fn groups_judge_members_by_mode(
        #[case] mode: GroupMode,
        #[case] states: &[MonitorState],
        #[case] expected: Health,
    ) {
        assert_eq!(health(&judged(mode, states)), Some(expected));
    }

    #[test]
    fn a_down_group_names_its_failing_members() {
        let outcome = judged(
            GroupMode::Any,
            &[MonitorState::Down, MonitorState::Up, MonitorState::Down],
        );
        let GroupOutcome::Judged(verdict) = outcome else {
            panic!("expected a verdict")
        };
        assert_eq!(
            verdict.reason.unwrap().to_string(),
            "reported: 2 of 3 members down: a, c"
        );
    }

    #[test]
    fn groups_without_counted_members_are_empty_or_in_maintenance() {
        assert_eq!(
            judged(GroupMode::Any, &[]),
            GroupOutcome::Empty("the group has no members".into())
        );
        assert!(matches!(
            judged(
                GroupMode::Any,
                &[MonitorState::Paused, MonitorState::Unknown]
            ),
            GroupOutcome::Empty(_)
        ));
        assert_eq!(
            judged(
                GroupMode::All,
                &[MonitorState::Maintenance, MonitorState::Paused]
            ),
            GroupOutcome::Maintenance
        );
    }

    #[test]
    fn group_checks_round_trip_through_json() {
        let check = GroupCheck {
            path: "prod".into(),
            mode: GroupMode::All,
            show_members: true,
        };
        let json = serde_json::to_string(&check).unwrap();
        assert_eq!(json, r#"{"path":"prod","mode":"all","show_members":true}"#);
        assert_eq!(serde_json::from_str::<GroupCheck>(&json).unwrap(), check);
        assert_eq!(
            serde_json::to_string(&GroupCheck::path("prod")).unwrap(),
            r#"{"path":"prod"}"#
        );
        assert_eq!("all".parse::<GroupMode>(), Ok(GroupMode::All));
        assert_eq!("most".parse::<GroupMode>(), Err(CheckError::GroupMode));
    }

    #[test]
    fn items_form_a_tree() {
        let (ungrouped, roots) = tree([
            (Some("prod/eu".to_owned()), "a"),
            (None, "loose"),
            (Some("prod".to_owned()), "b"),
            (Some("dev".to_owned()), "c"),
            (Some("prod/eu".to_owned()), "d"),
        ]);

        assert_eq!(ungrouped, ["loose"]);
        let names: Vec<_> = roots.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["dev", "prod"]);
        let prod = &roots[1];
        assert_eq!(prod.items, ["b"]);
        assert_eq!(prod.children[0].path, "prod/eu");
        assert_eq!(prod.children[0].items, ["a", "d"]);
        assert_eq!(prod.all_items(), [&"b", &"a", &"d"]);
    }
}
