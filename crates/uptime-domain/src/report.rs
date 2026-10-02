//! Errors for logs: the whole cause chain on one line.

use std::{error::Error, fmt};

/// Displays an error followed by each of its sources, outermost first
/// ("sending the alert: connection refused"). A source whose text is already
/// in the line (as with `#[error("…: {0}")]` wrappers) is left out.
///
/// Log fields use it as `error = %Report(&error)`.
///
/// ```
/// # use uptime_domain::Report;
/// let error = std::io::Error::other("disk full");
/// assert_eq!(Report(&error).to_string(), "disk full");
/// ```
pub struct Report<'a>(pub &'a (dyn Error + 'static));

impl fmt::Display for Report<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut messages = Vec::new();
        let mut next = Some(self.0);
        while let Some(error) = next {
            messages.push(error.to_string());
            next = error.source();
        }
        f.write_str(&chain(messages))
    }
}

/// Joins messages (outermost first) with ": ", skipping empty ones and any
/// the line already contains.
pub fn chain(messages: impl IntoIterator<Item = String>) -> String {
    let mut line = String::new();
    for message in messages {
        let message = message.trim();
        if message.is_empty() || line.contains(message) {
            continue;
        }
        if !line.is_empty() {
            line.push_str(": ");
        }
        line.push_str(message);
    }
    line
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[derive(Debug)]
    struct Wrapped {
        message: &'static str,
        source: Option<Box<dyn Error + 'static>>,
    }

    impl fmt::Display for Wrapped {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.message)
        }
    }

    impl Error for Wrapped {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.source.as_deref()
        }
    }

    fn wrap(message: &'static str, source: Option<Wrapped>) -> Wrapped {
        Wrapped {
            message,
            source: source.map(|s| Box::new(s) as Box<dyn Error>),
        }
    }

    #[test]
    fn reports_every_cause_outermost_first() {
        let error = wrap(
            "delivering alerts failed",
            Some(wrap(
                "error sending request",
                Some(wrap("connection refused", None)),
            )),
        );
        assert_eq!(
            Report(&error).to_string(),
            "delivering alerts failed: error sending request: connection refused"
        );
    }

    #[test]
    fn skips_causes_the_message_already_quotes() {
        let error = wrap(
            "invalid config: bad port",
            Some(wrap("bad port", Some(wrap("", None)))),
        );
        assert_eq!(Report(&error).to_string(), "invalid config: bad port");
    }

    #[test]
    fn a_lone_error_is_just_its_message() {
        assert_eq!(
            Report(&std::io::Error::other("disk full")).to_string(),
            "disk full"
        );
    }
}
