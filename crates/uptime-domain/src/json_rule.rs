//! Response assertions on JSON bodies: a path into the document and,
//! optionally, the value it must have.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `path` must exist (and not be `null`); with `expect`, its text form must
/// equal it. Paths look like `data.status` or `items[0].ok` (a leading `$.`
/// is allowed).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsonRule {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<String>,
}

/// One step into a document.
#[derive(Debug, PartialEq, Eq)]
enum Step<'a> {
    Key(&'a str),
    Index(usize),
}

/// Why a path could not be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid JSON path: {0}")]
pub struct PathError(pub &'static str);

fn parse(path: &str) -> Result<Vec<Step<'_>>, PathError> {
    let path = path.trim();
    let path = path.strip_prefix('$').unwrap_or(path);
    let path = path.strip_prefix('.').unwrap_or(path);
    if path.is_empty() {
        return Err(PathError("empty path"));
    }
    let mut steps = Vec::new();
    for segment in path.split('.') {
        let (key, mut rest) = match segment.find('[') {
            Some(at) => segment.split_at(at),
            None => (segment, ""),
        };
        if key.is_empty() && rest.is_empty() {
            return Err(PathError("empty segment"));
        }
        if !key.is_empty() {
            steps.push(Step::Key(key));
        }
        while !rest.is_empty() {
            let inner = rest
                .strip_prefix('[')
                .and_then(|r| r.split_once(']'))
                .ok_or(PathError("unclosed `[`"))?;
            let index = inner
                .0
                .parse()
                .map_err(|_| PathError("index is not a number"))?;
            steps.push(Step::Index(index));
            rest = inner.1;
        }
    }
    Ok(steps)
}

impl JsonRule {
    /// Whether the path is well-formed.
    pub fn validate(&self) -> Result<(), PathError> {
        parse(&self.path).map(drop)
    }

    /// Whether `body` (JSON text) satisfies the rule. Bodies that are not
    /// JSON, and paths that are not there, never match.
    pub fn matches(&self, body: &str) -> bool {
        let Ok(steps) = parse(&self.path) else {
            return false;
        };
        let Ok(document) = serde_json::from_str::<Value>(body) else {
            return false;
        };
        let mut at = &document;
        for step in steps {
            let next = match step {
                Step::Key(key) => at.get(key),
                Step::Index(i) => at.get(i),
            };
            match next {
                Some(value) => at = value,
                None => return false,
            }
        }
        match (&self.expect, at) {
            (_, Value::Null) => false,
            (None, _) => true,
            (Some(expected), Value::String(text)) => text == expected,
            (Some(expected), other) => {
                let text = other.to_string();
                text == *expected
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn rule(path: &str, expect: Option<&str>) -> JsonRule {
        JsonRule {
            path: path.into(),
            expect: expect.map(Into::into),
        }
    }

    #[rstest]
    #[case("status", Some("ok"), r#"{"status":"ok"}"#, true)]
    #[case("status", Some("ok"), r#"{"status":"down"}"#, false)]
    #[case("$.data.ready", Some("true"), r#"{"data":{"ready":true}}"#, true)]
    #[case("items[1].n", Some("2"), r#"{"items":[{"n":1},{"n":2}]}"#, true)]
    #[case("items[5]", None, r#"{"items":[1]}"#, false)]
    #[case("a", None, r#"{"a":0}"#, true)]
    #[case("a", None, r#"{"a":null}"#, false)]
    #[case("a", None, r#"{"b":1}"#, false)]
    #[case("a", None, "not json", false)]
    #[case("[0]", Some("x"), r#"["x"]"#, true)]
    fn matches_documents(
        #[case] path: &str,
        #[case] expect: Option<&str>,
        #[case] body: &str,
        #[case] expected: bool,
    ) {
        assert_eq!(rule(path, expect).matches(body), expected);
    }

    #[rstest]
    #[case("", false)]
    #[case("a..b", false)]
    #[case("a[x]", false)]
    #[case("a[1", false)]
    #[case("a.b[0][1]", true)]
    #[case("$.a", true)]
    fn validates_paths(#[case] path: &str, #[case] ok: bool) {
        assert_eq!(rule(path, None).validate().is_ok(), ok);
    }
}
