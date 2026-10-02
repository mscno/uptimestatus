//! Monitor groups: a slash-separated path (`prod/eu`) that arranges monitors
//! into a tree, each node summarising its children.

use std::collections::BTreeMap;

use crate::MonitorState;

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
