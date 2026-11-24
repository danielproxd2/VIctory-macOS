//! For tests that need the host D3D12 device, here and in the crates built
//! on this one (victory-hle, the apps). Not used by a run.
//!
//! A test that needs a device starts with [`gpu`] and keeps what it gets
//! to its end:
//!
//! ```no_run
//! let Some(_gpu) = victory_gpu::test_support::gpu() else {
//!     return; // "skipped: no host D3D12 device" was printed
//! };
//! // ... create lists, submit, read back ...
//! ```
//!
//! That gives every such test the same three properties:
//!
//! * one at a time: the device is one per process, and tests on it from
//!   several threads now and then hang or end the process;
//! * visible when it did not run: one [`SKIPPED`] line per test, past the
//!   test harness's capture, so a run can be searched for it;
//! * failing where a device is expected: with `VICTORY_REQUIRE_GPU=1`
//!   (`tools/macos/test-wine.sh` passes it on) a missing device is a
//!   failed test, not a skipped one.
//!
//! A test that only reaches the device where there is one (it creates a
//! command list, say, and asserts nothing about the host) takes [`serial`]
//! instead. A test that cannot run for another reason says so with
//! [`skipped`].

use std::io::Write;
use std::sync::{Mutex, MutexGuard};

/// One device test at a time. The host device is one per process, and
/// victory-shader's device tests found that several test threads on one
/// adapter now and then leave every dispatch unfinished (the test binary
/// hangs) or end the process with an access violation; a host failure here
/// ends the process as well.
static GPU_TESTS: Mutex<()> = Mutex::new(());

/// Held by the one test that may use the host device.
pub type Serial = MutexGuard<'static, ()>;

/// Waits for the other device tests of this process and keeps them out
/// until the guard is dropped. A test that failed while holding it has
/// poisoned the lock, which does not fail the tests after it.
///
/// The lock is this process's: a test that starts a child process which
/// creates a device holds it for as long as the child lives.
pub fn serial() -> Serial {
    GPU_TESTS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether a test that needs a host device fails where there is none:
/// `VICTORY_REQUIRE_GPU=1`, for runs that are meant to prove something
/// about a device.
pub fn require_gpu() -> bool {
    required(std::env::var("VICTORY_REQUIRE_GPU").ok().as_deref())
}

fn required(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.trim() == "1")
}

const NO_DEVICE: &str = "no host D3D12 device";

/// What every test without a device prints, so that a run can be searched
/// for it.
pub const SKIPPED: &str = "skipped: no host D3D12 device";

fn skip_line(what: &str) -> String {
    format!("skipped: {what}")
}

/// Says that the calling test did not run and why, as one line
/// `skipped: <what>` written straight to standard error: the test harness
/// shows what a passing test prints only on request, and a skipped test
/// passes.
pub fn skipped(what: &str) {
    let _ = writeln!(std::io::stderr(), "{}", skip_line(what));
}

/// What a test that needs a device does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Run,
    Skip,
    Fail,
}

fn verdict(available: bool, required: bool) -> Verdict {
    match (available, required) {
        (true, _) => Verdict::Run,
        (false, false) => Verdict::Skip,
        (false, true) => Verdict::Fail,
    }
}

/// The lock, and whether there is a device; without one, the [`SKIPPED`]
/// line or, where a device is required, a panic.
pub(crate) fn serial_with_device() -> (Serial, bool) {
    let guard = serial();
    match verdict(crate::host_available(), require_gpu()) {
        Verdict::Run => (guard, true),
        Verdict::Skip => {
            skipped(NO_DEVICE);
            (guard, false)
        }
        Verdict::Fail => panic!("VICTORY_REQUIRE_GPU=1: {NO_DEVICE}"),
    }
}

/// For a test that needs the host device: [`serial`]'s guard if there is
/// one ([`crate::host_available`]), to be kept to the end of the test.
/// Without a device this is `None` after one [`SKIPPED`] line, and the
/// test returns.
///
/// # Panics
///
/// Without a device when `VICTORY_REQUIRE_GPU=1`: that fails the test.
pub fn gpu() -> Option<Serial> {
    let (guard, device) = serial_with_device();
    device.then_some(guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gpu_lock_survives_a_panicking_holder() {
        let holder = std::thread::spawn(|| {
            let _held = GPU_TESTS.lock().unwrap_or_else(|e| e.into_inner());
            // Unwinds as a failed assertion does, without its message.
            std::panic::resume_unwind(Box::new("a device test failed"));
        });
        assert!(holder.join().is_err());
        // The holder died with the lock, and the next test still gets it.
        assert!(GPU_TESTS.is_poisoned());
        drop(serial());
        drop(serial());
    }

    /// Only `1` requires a device; the variable unset, empty or anything
    /// else leaves the tests skipping.
    #[test]
    fn require_gpu_reads_the_environment() {
        assert!(required(Some("1")));
        assert!(required(Some(" 1\n")));
        for other in [None, Some(""), Some("0"), Some("true"), Some("11")] {
            assert!(!required(other), "{other:?}");
        }
    }

    /// With a device the test runs whatever the run requires; without one
    /// it is skipped, and fails only where a device is required.
    #[test]
    fn a_missing_device_skips_or_fails_as_the_run_requires() {
        assert_eq!(verdict(true, false), Verdict::Run);
        assert_eq!(verdict(true, true), Verdict::Run);
        assert_eq!(verdict(false, false), Verdict::Skip);
        assert_eq!(verdict(false, true), Verdict::Fail);
    }

    /// Every skip is one line that starts the same way, and the one for a
    /// missing device is the line runs are searched for.
    #[test]
    fn skips_are_one_uniform_line() {
        assert_eq!(skip_line(NO_DEVICE), SKIPPED);
        assert_eq!(SKIPPED, "skipped: no host D3D12 device");
        assert_eq!(skip_line("no WARP adapter"), "skipped: no WARP adapter");
    }

    /// Where there is no host module there is no device, and where there
    /// is one, asking twice gives the same answer (it is made once).
    #[test]
    fn host_availability_is_stable() {
        let _gpu = serial();
        let first = crate::host_available();
        assert_eq!(crate::host_available(), first);
        #[cfg(not(windows))]
        assert!(!first);
    }
}
