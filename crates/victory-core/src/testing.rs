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
        skip_line("not Wine", "policy::tests::a_test"),
        "skipped: not Wine (policy::tests::a_test)"
    );
}
