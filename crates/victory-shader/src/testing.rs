//! What the tests of this crate do where something they need is missing:
//! a host D3D12 device, WARP, the DXIL validator or the compilers of the
//! differential tests. Such a test passes after printing one line
//! `skipped: <reason> (<test>)` past the test harness's capture, so that a
//! run can be searched for what it did not check. A switch per need turns
//! the skip into a failure, for runs that are meant to prove something:
//!
//! * `VICTORY_REQUIRE_GPU=1`: the host's SM 6.6 device (the hardware
//!   adapter);
//! * `VICTORY_REQUIRE_WARP=1`: the WARP device beside it;
//! * `VICTORY_REQUIRE_DXIL=1`: the validator in `dxil.dll`;
//! * `VICTORY_REQUIRE_DIFF=1`: the console and PC compilers.

use std::collections::HashSet;
use std::sync::Mutex;

use crate::emit::require;

/// Whether the switch `var` is on in this run.
pub(crate) fn required(var: &str) -> bool {
    require(std::env::var(var).ok().as_deref())
}

/// The line a skip prints.
pub(crate) fn skip_line(reason: &str, test: &str) -> String {
    format!("skipped: {reason} ({test})")
}

/// The lines already printed: a test that meets the same lack several
/// times says so once.
static PRINTED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Prints the skip line for `reason` in the running test (the harness
/// names each test's thread after it). The line goes to the process's own
/// standard error, which the harness does not capture.
pub(crate) fn skip(reason: &str) {
    use std::io::Write;
    let thread = std::thread::current();
    let line = skip_line(reason, thread.name().unwrap_or("?"));
    let mut printed = PRINTED.lock().unwrap_or_else(|e| e.into_inner());
    if printed
        .get_or_insert_with(HashSet::new)
        .insert(line.clone())
    {
        let _ = writeln!(std::io::stderr(), "{line}");
    }
}

/// What a test without something it needs does: `Ok` to go on after a
/// skip line, `Err` with the failure message when the switch `var`
/// (whose value in this run is `value`) requires it.
pub(crate) fn missing(var: &str, value: Option<&str>, reason: &str) -> Result<(), String> {
    if require(value) {
        Err(format!("{var}=1: {reason}"))
    } else {
        Ok(())
    }
}

/// Fails the test if the switch `var` is on, else prints the skip line.
pub(crate) fn skip_unless_required(var: &str, reason: &str) {
    if let Err(message) = missing(var, std::env::var(var).ok().as_deref(), reason) {
        panic!("{message}");
    }
    skip(reason);
}

/// The host's SM 6.6 device for a test: the hardware adapter, or WARP
/// with `warp`. Without one the test goes on without it after a skip
/// line; `VICTORY_REQUIRE_GPU=1` (hardware) and `VICTORY_REQUIRE_WARP=1`
/// (WARP) make that a failure. D3DMetal under Wine has no WARP.
#[cfg(windows)]
pub(crate) fn host(warp: bool) -> Option<crate::emit::host::Host> {
    let (var, what) = if warp {
        ("VICTORY_REQUIRE_WARP", "no SM 6.6 WARP device")
    } else {
        ("VICTORY_REQUIRE_GPU", "no host D3D12 device")
    };
    match crate::emit::host::Host::new(warp) {
        Ok(host) => Some(host),
        Err(e) => {
            let e = e.trim();
            let why = if e.is_empty() {
                what.to_string()
            } else {
                format!("{what}: {e}")
            };
            skip_unless_required(var, &why);
            None
        }
    }
}

/// Has `dxil.dll` validate an emitted container and checks that the hash
/// we wrote is the one the validator signs it with. Where the validator
/// cannot be loaded (always so off Windows) this is a skip line, and a
/// failure with `VICTORY_REQUIRE_DXIL=1`.
pub(crate) fn dxil(container: &[u8]) {
    #[cfg(windows)]
    crate::emit::validator::check(container);
    #[cfg(not(windows))]
    {
        let _ = container;
        skip_unless_required(
            "VICTORY_REQUIRE_DXIL",
            "DXIL validation: dxil.dll loads on Windows only",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_lines_are_uniform_and_name_the_test() {
        let line = skip_line("no host D3D12 device", "emit::tests::a_test");
        assert_eq!(line, "skipped: no host D3D12 device (emit::tests::a_test)");
        assert!(line.starts_with("skipped: "));
    }

    /// The switches read the run's own environment; what they say follows
    /// [`require`].
    #[test]
    fn the_switches_follow_the_environment() {
        for var in [
            "VICTORY_REQUIRE_GPU",
            "VICTORY_REQUIRE_WARP",
            "VICTORY_REQUIRE_DXIL",
            "VICTORY_REQUIRE_DIFF",
        ] {
            let set = std::env::var(var).ok();
            assert_eq!(required(var), require(set.as_deref()), "{var}");
        }
    }

    /// A switch that is on fails the test with a message naming it; one
    /// that is off, or set to anything but 1, lets the test go on.
    #[test]
    fn a_skip_passes_unless_its_switch_is_on() {
        let var = "VICTORY_REQUIRE_DXIL";
        assert_eq!(
            missing(var, Some("1"), "no dxil.dll"),
            Err("VICTORY_REQUIRE_DXIL=1: no dxil.dll".into())
        );
        for off in [None, Some(""), Some("0"), Some("true")] {
            assert_eq!(missing(var, off, "no dxil.dll"), Ok(()), "{off:?}");
        }
    }
}
