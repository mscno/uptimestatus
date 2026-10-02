//! Capturing log lines in a test.
//!
//! One global subscriber (installed on first use) formats every event as a
//! JSON line; its writer hands each line to the capture active on the
//! current thread, if any. (Thread-scoped subscribers look simpler, but
//! `tracing` caches whether a log statement is wanted process-wide, so a
//! statement first reached by a parallel test without one can stay silenced.)

use std::{
    cell::RefCell,
    io,
    sync::{Arc, Mutex, OnceLock, PoisonError},
};

use serde_json::Value;
use tracing::{Metadata, level_filters::LevelFilter};
use tracing_subscriber::{
    fmt::{MakeWriter, writer::EitherWriter},
    layer::SubscriberExt as _,
};

thread_local! {
    /// This thread's capture and the most verbose level it keeps.
    static ACTIVE: RefCell<Option<(Logs, LevelFilter)>> = const { RefCell::new(None) };
}

/// Everything written to it, as JSON log lines. Also usable as the writer of
/// any `fmt` layer.
#[derive(Clone, Debug, Default)]
pub struct Logs(Arc<Mutex<Vec<u8>>>);

/// Ends a capture (restoring any outer one) when dropped.
#[must_use = "the capture ends when the guard drops"]
pub struct CaptureGuard {
    previous: Option<(Logs, LevelFilter)>,
}

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        ACTIVE.with(|active| *active.borrow_mut() = previous);
    }
}

impl Logs {
    /// Captures events at `level` (`"info"`, `"debug"`, …) and above on this
    /// thread until the guard drops. A `#[tokio::test]` runs every task on its
    /// own thread, so this sees everything the test does.
    pub fn capture(level: &str) -> (Self, CaptureGuard) {
        static INSTALLED: OnceLock<()> = OnceLock::new();
        INSTALLED.get_or_init(|| {
            let subscriber = tracing_subscriber::registry()
                .with(LevelFilter::DEBUG)
                .with(
                    tracing_subscriber::fmt::layer()
                        .json()
                        .flatten_event(true)
                        .with_current_span(true)
                        .with_span_list(false)
                        .with_writer(ToCapture),
                );
            tracing::subscriber::set_global_default(subscriber)
                .expect("no other global subscriber in a test that captures logs");
        });
        let level: LevelFilter = level.parse().expect("a log level");
        let logs = Self::default();
        let previous = ACTIVE.with(|active| active.borrow_mut().replace((logs.clone(), level)));
        (logs, CaptureGuard { previous })
    }

    /// Every line so far.
    pub fn lines(&self) -> Vec<Value> {
        let bytes = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        String::from_utf8_lossy(&bytes)
            .lines()
            .map(|line| serde_json::from_str(line).expect("a JSON log line"))
            .collect()
    }

    /// The lines whose message is `message`.
    pub fn with_message(&self, message: &str) -> Vec<Value> {
        self.lines()
            .into_iter()
            .filter(|line| line["message"] == message)
            .collect()
    }
}

/// Routes a line to the current thread's capture, or nowhere.
struct ToCapture;

impl<'a> MakeWriter<'a> for ToCapture {
    type Writer = EitherWriter<Logs, io::Sink>;

    fn make_writer(&'a self) -> Self::Writer {
        EitherWriter::B(io::sink())
    }

    fn make_writer_for(&'a self, meta: &Metadata<'_>) -> Self::Writer {
        ACTIVE.with(|active| match &*active.borrow() {
            Some((logs, level)) if *meta.level() <= *level => EitherWriter::A(logs.clone()),
            _ => EitherWriter::B(io::sink()),
        })
    }
}

impl io::Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Logs {
    type Writer = Self;

    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}
