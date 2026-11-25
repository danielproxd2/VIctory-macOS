//! What a piece of VIctory logs while it runs, caught in memory.
//!
//! `victory-gpu` measures its host when it makes its device and says what
//! it found in log lines only (`host.rs`: the 64-bit atomics probe). The
//! doctor reads those lines here, so that it reports the verdict a run
//! acts on and not a second measurement of its own.

use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::span;

/// One log line: its level, its message and its fields as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Logged {
    pub(crate) level: tracing::Level,
    pub(crate) message: String,
    pub(crate) fields: Vec<(String, String)>,
}

impl Logged {
    /// The field `name` of the line, as it would be printed.
    pub(crate) fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Default)]
struct Fields {
    message: String,
    fields: Vec<(String, String)>,
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let text = format!("{value:?}");
        if field.name() == "message" {
            self.message = text;
        } else {
            self.fields.push((field.name().to_owned(), text));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_owned();
        } else {
            self.fields
                .push((field.name().to_owned(), value.to_owned()));
        }
    }
}

/// Keeps every event; spans are of no interest and all get one id.
struct Keep(Arc<Mutex<Vec<Logged>>>);

impl tracing::Subscriber for Keep {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }

    fn record(&self, _: &span::Id, _: &span::Record<'_>) {}

    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Logged {
                level: *event.metadata().level(),
                message: fields.message,
                fields: fields.fields,
            });
    }

    fn enter(&self, _: &span::Id) {}

    fn exit(&self, _: &span::Id) {}
}

/// Runs `f` and returns what it logged on this thread beside its result.
/// Those lines do not reach the process's log.
pub(crate) fn capture<T>(f: impl FnOnce() -> T) -> (T, Vec<Logged>) {
    let kept = Arc::new(Mutex::new(Vec::new()));
    let dispatch = tracing::Dispatch::new(Keep(kept.clone()));
    let result = tracing::dispatcher::with_default(&dispatch, f);
    let logged = std::mem::take(&mut *kept.lock().unwrap_or_else(|e| e.into_inner()));
    (result, logged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_keeps_the_message_and_the_fields_of_each_line() {
        let (result, logged) = capture(|| {
            tracing::info!(atomics = false, "host 64-bit atomics");
            tracing::warn!(
                host = %"Some Adapter",
                texel = format_args!("{:#x}", 0x12),
                "did not run"
            );
            7
        });
        assert_eq!(result, 7);
        assert_eq!(logged.len(), 2, "{logged:?}");
        assert_eq!(logged[0].level, tracing::Level::INFO);
        assert_eq!(logged[0].message, "host 64-bit atomics");
        assert_eq!(logged[0].field("atomics"), Some("false"));
        assert_eq!(logged[1].level, tracing::Level::WARN);
        assert_eq!(logged[1].message, "did not run");
        assert_eq!(logged[1].field("host"), Some("Some Adapter"));
        assert_eq!(logged[1].field("texel"), Some("0x12"));
        assert_eq!(logged[1].field("absent"), None);
    }

    #[test]
    fn nothing_logged_is_an_empty_list() {
        let ((), logged) = capture(|| {});
        assert!(logged.is_empty());
    }
}
