//! What a test of this crate does where the host gives it nothing to
//! check: it passes after printing one line `skipped: <reason> (<test>)`
//! past the test harness's capture, as the shader and GPU tests do, so
//! that `grep 'skipped: '` over a run lists everything the run left out.

/// The line a skip prints.
pub(crate) fn skip_line(reason: &str, test: &str) -> String {
    format!("skipped: {reason} ({test})")
}

/// Prints the skip line for `reason` in the running test (the harness
/// names each test's thread after it). The line goes to the process's own
/// standard error: what a passing test prints with `println!` or
/// `eprintln!` is captured and shown to no one.
pub(crate) fn skip(reason: &str) {
    use std::io::Write;
    let thread = std::thread::current();
    let line = skip_line(reason, thread.name().unwrap_or("?"));
    let _ = writeln!(std::io::stderr(), "{line}");
}

#[test]
fn a_skip_line_names_the_reason_and_the_test() {
    assert_eq!(
        skip_line("not Wine", "modules::xmem::tests::a_test"),
        "skipped: not Wine (modules::xmem::tests::a_test)"
    );
}

/// Runs the test `test` of this binary (its full path, as the harness
/// prints it) alone in a child process, with `envs` added to this
/// process's environment, and returns what it printed and how it ended.
/// For what can only be seen from outside: the ways a process ends.
///
/// The child is a test like any other, so it also runs, as itself, in
/// every ordinary run: it must return at once unless the variable that
/// names its part is set.
pub(crate) fn child(test: &str, envs: &[(&str, &std::ffi::OsStr)]) -> std::process::Output {
    std::process::Command::new(std::env::current_exe().expect("the test binary's own path"))
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .envs(envs.iter().copied())
        .output()
        .expect("the test binary starts again")
}

/// Standard output and standard error of a [`child`], as text.
pub(crate) fn printed(out: &std::process::Output) -> (String, String) {
    let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    (text(&out.stdout), text(&out.stderr))
}

/// In a [`child`]: from here on every log event of the process is a line
/// on its standard error, `LEVEL message name=value ...` (see
/// [`log_line`]), so that its parent can read what was logged before the
/// process ended.
pub(crate) fn print_logs() {
    struct Printer;

    impl tracing::Subscriber for Printer {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            use std::io::Write;
            let mut fields = Fields::default();
            event.record(&mut fields);
            let line = log_line(event.metadata().level().as_str(), &fields.0);
            let _ = writeln!(std::io::stderr(), "{line}");
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    let _ = tracing::subscriber::set_global_default(Printer);
}

/// The fields of a log event, in order: (name, value).
#[derive(Default)]
struct Fields(Vec<(String, String)>);

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push((field.name().to_owned(), format!("{value:?}")));
    }
}

/// The line [`print_logs`] prints for an event: its level, its message,
/// then every other field as `name=value`.
fn log_line(level: &str, fields: &[(String, String)]) -> String {
    let mut line = level.to_owned();
    for (_, value) in fields.iter().filter(|(name, _)| name == "message") {
        line.push(' ');
        line.push_str(value);
    }
    for (name, value) in fields.iter().filter(|(name, _)| name != "message") {
        line.push_str(&format!(" {name}={value}"));
    }
    line
}

#[test]
fn a_log_line_puts_the_message_first() {
    let field = |n: &str, v: &str| (n.to_owned(), v.to_owned());
    assert_eq!(
        log_line(
            "WARN",
            &[field("code", "7"), field("message", "title exits")]
        ),
        "WARN title exits code=7"
    );
}
