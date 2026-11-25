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
//!
//! Three more conditions of this platform have a line and a switch of their
//! own ([`no_hlsl`], [`stale_clamp`], [`no_depth`]), so that a run can count
//! the tests they kept from running and a run that is meant to prove them
//! fails instead:
//!
//! * [`NO_HLSL`], `VICTORY_REQUIRE_HLSL=1`: a test compiles its own
//!   shader with the system's HLSL compiler, and the host's cannot build
//!   it (Wine's is vkd3d-shader's, not Microsoft's);
//! * [`STALE_CLAMP`], `VICTORY_REQUIRE_CLAMP=1`: the prebuilt argument
//!   clamp was built from an older `clamp.hlsl` and the host cannot
//!   compile the shader itself.
//! * [`NO_DEPTH`], `VICTORY_REQUIRE_DEPTH=1`: no bytecode of the present
//!   `depth.hlsl` is embedded and Wine's compiler cannot build the shader.

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
    skipped_to(&mut std::io::stderr(), what);
}

fn skipped_to(out: &mut dyn Write, what: &str) {
    let _ = writeln!(out, "{}", skip_line(what));
}

/// Why a test that compiles a shader of its own did not run: the system's
/// HLSL compiler refused the source. Under Wine `D3DCompile` is
/// vkd3d-shader's compiler, which does not build everything Microsoft's
/// does. The reason is that compiler's alone: where the process is not
/// under Wine ([`under_wine`]) a shader that does not compile fails its
/// test, as it does upstream ([`hlsl_refusal`]).
pub const NO_HLSL: &str = "the host HLSL compiler cannot build this test shader";

/// Why a test of the argument clamp did not run: `clamp_bytecode.rs` was
/// built from another `clamp.hlsl` than the present one, so it is not
/// used, and the host could not compile the shader either.
pub const STALE_CLAMP: &str = "the prebuilt clamp bytecode is older than clamp.hlsl; \
     rebuild it with the clamp bytecode workflow";

/// Why a test of the depth conversion did not run: `depth_bytecode.rs`
/// holds no bytecode of the present `depth.hlsl` (none was embedded yet, or
/// the source changed since), so there is none to use, and Wine's compiler
/// cannot build the shader.
pub const NO_DEPTH: &str = "no prebuilt depth conversion of this depth.hlsl; \
     build it on a Windows runner and run `cargo xtask embed-depth`";

/// Whether this process runs under Wine (its ntdll exports
/// `wine_get_version`), where the system's HLSL compiler is vkd3d-shader's
/// and not Microsoft's. The skips for a shader the system cannot compile
/// are for that compiler only: anywhere else such a test fails.
pub fn under_wine() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
        // SAFETY: ntdll is always loaded, and both names end with a NUL.
        unsafe {
            let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
            !ntdll.is_null() && GetProcAddress(ntdll, c"wine_get_version".as_ptr().cast()).is_some()
        }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Whether a test whose own shader the host's HLSL compiler cannot build
/// fails: `VICTORY_REQUIRE_HLSL=1`.
pub fn require_hlsl() -> bool {
    required(std::env::var("VICTORY_REQUIRE_HLSL").ok().as_deref())
}

/// Whether a test that needs the argument clamp fails while the prebuilt
/// bytecode is stale: `VICTORY_REQUIRE_CLAMP=1`.
pub fn require_clamp() -> bool {
    required(std::env::var("VICTORY_REQUIRE_CLAMP").ok().as_deref())
}

/// The line of a test that did not run for `what`.
fn skip_line_of(what: &str, test: &str) -> String {
    skip_line(&format!("{what} ({test})"))
}

/// What `test` does about something it needs and does not have (`what`):
/// it is skipped after the one line `skipped: <what> (<test>)` on `out`,
/// or, where the run requires it (`switch` is `1`), fails with what this
/// returns (and no line: it did not skip).
fn unmet(
    switch: &str,
    what: &str,
    test: &str,
    required: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    if required {
        return Err(format!("{switch}=1: {what} ({test})"));
    }
    let _ = writeln!(out, "{}", skip_line_of(what, test));
    Ok(())
}

/// Says that `test` did not run because it needs `what`, as the line
/// `skipped: <what> (<test>)` on standard error, followed by `detail` if
/// there is any.
///
/// # Panics
///
/// When the run requires `what` (`required`, the value of `switch`): that
/// fails the test, with `detail` in the message.
fn needs(switch: &str, what: &str, test: &str, required: bool, detail: Option<&str>) {
    let mut out = std::io::stderr();
    match unmet(switch, what, test, required, &mut out) {
        Ok(()) => {
            if let Some(detail) = detail {
                let _ = writeln!(out, "{}", detail.trim_end());
            }
        }
        Err(why) => match detail {
            Some(detail) => panic!("{why}: {}", detail.trim_end()),
            None => panic!("{why}"),
        },
    }
}

/// Whether `text` is said for the first time in this process: the
/// compiler's messages follow the first skip they caused, not every one.
fn first_time(said: &Mutex<Vec<String>>, text: &str) -> bool {
    let mut said = said.lock().unwrap_or_else(|e| e.into_inner());
    let new = !said.iter().any(|s| s == text);
    if new {
        said.push(text.to_owned());
    }
    new
}

/// What a test does whose own shader the system's HLSL compiler refused
/// with `errors`: under Wine (`wine`), where that compiler is
/// vkd3d-shader's, it may be skipped (`Ok`); with Microsoft's compiler the
/// refusal is the test's failure (`Err`, what it fails with), as it is
/// upstream, whatever the run requires.
fn hlsl_refusal(wine: bool, test: &str, errors: &str) -> Result<(), String> {
    if wine {
        Ok(())
    } else {
        Err(format!(
            "{test}: the system's HLSL compiler (not Wine's) refused the test's shader: {}",
            errors.trim_end()
        ))
    }
}

/// For a test whose own shader the system's HLSL compiler refused with
/// `errors`, under Wine: the line `skipped: ` [`NO_HLSL`] ` (<test>)`, and
/// after it the compiler's messages, unless this process has printed the
/// same ones before. The test returns after it.
///
/// # Panics
///
/// Where the process is not under Wine: the compiler is Microsoft's, and a
/// shader it refuses is a failed test. With `VICTORY_REQUIRE_HLSL=1`:
/// that fails the test under Wine too. Both with the compiler's messages.
pub fn no_hlsl(test: &str, errors: &str) {
    static SAID: Mutex<Vec<String>> = Mutex::new(Vec::new());
    if let Err(why) = hlsl_refusal(under_wine(), test, errors) {
        panic!("{why}");
    }
    let required = require_hlsl();
    let detail = (required || first_time(&SAID, errors)).then_some(errors);
    needs("VICTORY_REQUIRE_HLSL", NO_HLSL, test, required, detail);
}

/// For a test that needs the argument clamp while the prebuilt bytecode
/// is stale and the host cannot compile the shader: the line `skipped: `
/// [`STALE_CLAMP`] ` (<test>)`. The test returns after it.
///
/// # Panics
///
/// With `VICTORY_REQUIRE_CLAMP=1`: that fails the test.
pub fn stale_clamp(test: &str) {
    needs(
        "VICTORY_REQUIRE_CLAMP",
        STALE_CLAMP,
        test,
        require_clamp(),
        None,
    );
}

/// Why a test that needs the software adapter did not run. Not every host
/// has one: D3DMetal under Wine has none.
pub const NO_WARP: &str = "no WARP adapter";

/// Whether a test that needs the software adapter fails where there is
/// none: `VICTORY_REQUIRE_WARP=1`, the switch `victory-shader`'s tests on
/// that adapter have.
pub fn require_warp() -> bool {
    required(std::env::var("VICTORY_REQUIRE_WARP").ok().as_deref())
}

/// For a test that needs the software adapter on a host without one: the
/// line `skipped: ` [`NO_WARP`] ` (<test>)`. The test returns after it.
///
/// # Panics
///
/// With `VICTORY_REQUIRE_WARP=1`: that fails the test.
pub fn no_warp(test: &str) {
    needs("VICTORY_REQUIRE_WARP", NO_WARP, test, require_warp(), None);
}

/// Whether a test that needs the depth conversion fails while no bytecode
/// of the present `depth.hlsl` is embedded: `VICTORY_REQUIRE_DEPTH=1`.
pub fn require_depth() -> bool {
    required(std::env::var("VICTORY_REQUIRE_DEPTH").ok().as_deref())
}

/// For a test that needs the depth conversion while no bytecode of the
/// present `depth.hlsl` is embedded and Wine's compiler cannot build the
/// shader: the line `skipped: ` [`NO_DEPTH`] ` (<test>)`. The test returns
/// after it.
///
/// # Panics
///
/// With `VICTORY_REQUIRE_DEPTH=1`: that fails the test.
pub fn no_depth(test: &str) {
    needs(
        "VICTORY_REQUIRE_DEPTH",
        NO_DEPTH,
        test,
        require_depth(),
        None,
    );
}

/// Whether a device test fails where the host gave the result it is known
/// to give instead of the PC hardware's: `VICTORY_REQUIRE_PC_GPU=1`, the
/// switch `victory-shader`'s tests have for the same.
pub fn require_pc_gpu() -> bool {
    required(std::env::var("VICTORY_REQUIRE_PC_GPU").ok().as_deref())
}

/// For a test that found the host doing what it is known to do instead of
/// what the PC hardware does (`difference` says which, as `docs/macos.md`
/// lists it under "Open differences under D3DMetal"): the line `skipped:
/// <difference> (<test>)`. The caller has checked both that the host is
/// the one the difference was measured on and that the result is the
/// measured one.
///
/// # Panics
///
/// With `VICTORY_REQUIRE_PC_GPU=1`: that fails the test.
pub fn host_differs(test: &str, difference: &str) {
    let required = require_pc_gpu();
    needs("VICTORY_REQUIRE_PC_GPU", difference, test, required, None);
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

/// What the verdict does to a test: whether it has a device, after the
/// [`SKIPPED`] line on `out` where it has none; or, where one is required
/// and missing, what the test fails with (and no line: it did not skip).
fn decide(available: bool, required: bool, out: &mut dyn Write) -> Result<bool, String> {
    match verdict(available, required) {
        Verdict::Run => Ok(true),
        Verdict::Skip => {
            skipped_to(out, NO_DEVICE);
            Ok(false)
        }
        Verdict::Fail => Err(format!("VICTORY_REQUIRE_GPU=1: {NO_DEVICE}")),
    }
}

/// `guard` and whether there is a device, as [`decide`] has it.
///
/// # Panics
///
/// With what [`decide`] fails with; `guard` is still held then, as a
/// failing device test holds it.
fn with_device<G>(guard: G, available: bool, required: bool, out: &mut dyn Write) -> (G, bool) {
    match decide(available, required, out) {
        Ok(device) => (guard, device),
        Err(why) => panic!("{why}"),
    }
}

/// The lock, and whether there is a device; without one, the [`SKIPPED`]
/// line or, where a device is required, a panic.
pub(crate) fn serial_with_device() -> (Serial, bool) {
    let guard = serial();
    let (available, required) = (crate::host_available(), require_gpu());
    with_device(guard, available, required, &mut std::io::stderr())
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

    /// What a test gets for each verdict, with the answer to "is there a
    /// device" given by the test instead of the host: with one it runs and
    /// nothing is printed; without one it is told so after exactly the
    /// line runs are searched for; and where a device is required it
    /// fails with a message that names the switch, without a skip line.
    #[test]
    fn a_run_without_a_device_prints_the_line_or_fails() {
        let mut out = Vec::new();
        assert_eq!(decide(true, false, &mut out), Ok(true));
        assert_eq!(decide(true, true, &mut out), Ok(true));
        assert_eq!(out, b"");

        assert_eq!(decide(false, false, &mut out), Ok(false));
        assert_eq!(out, b"skipped: no host D3D12 device\n");
        assert_eq!(out, format!("{SKIPPED}\n").as_bytes());

        let mut out = Vec::new();
        let failed = decide(false, true, &mut out);
        assert_eq!(
            failed,
            Err("VICTORY_REQUIRE_GPU=1: no host D3D12 device".to_owned())
        );
        assert_eq!(out, b"", "a failed test is not a skipped one");
    }

    /// The same through what the tests call: the guard comes back with
    /// the answer, and the failure is a panic of the calling test (which
    /// is what fails it) carrying the message.
    #[test]
    fn a_required_device_that_is_missing_fails_the_calling_test() {
        let mut out = Vec::new();
        assert_eq!(with_device("guard", true, true, &mut out), ("guard", true));
        assert_eq!(
            with_device("guard", false, false, &mut out),
            ("guard", false)
        );
        assert_eq!(out, format!("{SKIPPED}\n").as_bytes());

        let failed = std::panic::catch_unwind(|| with_device((), false, true, &mut Vec::new()));
        let message = failed.expect_err("a missing required device does not return");
        let message = message.downcast_ref::<String>().map(String::as_str);
        assert_eq!(message, Some("VICTORY_REQUIRE_GPU=1: no host D3D12 device"));
    }

    /// Every skip is one line that starts the same way, and the one for a
    /// missing device is the line runs are searched for.
    #[test]
    fn skips_are_one_uniform_line() {
        assert_eq!(skip_line(NO_DEVICE), SKIPPED);
        assert_eq!(SKIPPED, "skipped: no host D3D12 device");
        assert_eq!(skip_line(NO_WARP), "skipped: no WARP adapter");
    }

    /// `VICTORY_REQUIRE_HLSL` and `VICTORY_REQUIRE_CLAMP` are read as
    /// `VICTORY_REQUIRE_GPU` is, and a test that lacks what they name
    /// prints one line that ends in its own name, or fails with a message
    /// that names the switch and prints no line.
    #[test]
    fn unmet_needs_skip_with_the_tests_name_or_fail() {
        let mut out = Vec::new();
        assert_eq!(
            unmet(
                "VICTORY_REQUIRE_HLSL",
                NO_HLSL,
                "draw::a_test",
                false,
                &mut out
            ),
            Ok(())
        );
        assert_eq!(
            out,
            b"skipped: the host HLSL compiler cannot build this test shader (draw::a_test)\n"
        );
        let mut out = Vec::new();
        assert_eq!(
            unmet(
                "VICTORY_REQUIRE_CLAMP",
                STALE_CLAMP,
                "indirect::a_test",
                false,
                &mut out
            ),
            Ok(())
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "skipped: the prebuilt clamp bytecode is older than clamp.hlsl; \
             rebuild it with the clamp bytecode workflow (indirect::a_test)\n"
        );

        let mut out = Vec::new();
        assert_eq!(
            unmet(
                "VICTORY_REQUIRE_DEPTH",
                NO_DEPTH,
                "twin::a_test",
                false,
                &mut out
            ),
            Ok(())
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "skipped: no prebuilt depth conversion of this depth.hlsl; build it on a \
             Windows runner and run `cargo xtask embed-depth` (twin::a_test)\n"
        );
        assert_eq!(
            unmet(
                "VICTORY_REQUIRE_DEPTH",
                NO_DEPTH,
                "t",
                true,
                &mut Vec::new()
            )
            .unwrap_err(),
            format!("VICTORY_REQUIRE_DEPTH=1: {NO_DEPTH} (t)")
        );

        let mut out = Vec::new();
        assert_eq!(
            unmet("VICTORY_REQUIRE_HLSL", NO_HLSL, "t", true, &mut out),
            Err(
                "VICTORY_REQUIRE_HLSL=1: the host HLSL compiler cannot build this test shader (t)"
                    .to_owned()
            )
        );
        assert_eq!(out, b"", "a failed test is not a skipped one");

        // Through what the tests call: a required need that is unmet is a
        // panic of the calling test, with the compiler's text in it.
        let failed = std::panic::catch_unwind(|| {
            needs(
                "VICTORY_REQUIRE_CLAMP",
                STALE_CLAMP,
                "t",
                true,
                Some("E5017"),
            )
        });
        let message = failed.expect_err("an unmet required need does not return");
        let message = message.downcast_ref::<String>().expect("a message");
        assert!(message.starts_with("VICTORY_REQUIRE_CLAMP=1: the prebuilt clamp bytecode"));
        assert!(message.ends_with("(t): E5017"), "{message}");

        // A refusal is a skip under Wine only: Microsoft's compiler
        // refusing a test's shader fails the test, whatever is required.
        assert_eq!(hlsl_refusal(true, "draw::a_test", "E5005"), Ok(()));
        assert_eq!(
            hlsl_refusal(false, "draw::a_test", "error X3000: syntax error\n"),
            Err(
                "draw::a_test: the system's HLSL compiler (not Wine's) refused the test's \
                 shader: error X3000: syntax error"
                    .to_owned()
            )
        );
        // The software adapter's line and switch.
        let mut out = Vec::new();
        assert_eq!(
            unmet(
                "VICTORY_REQUIRE_WARP",
                NO_WARP,
                "host::a_test",
                false,
                &mut out
            ),
            Ok(())
        );
        assert_eq!(out, b"skipped: no WARP adapter (host::a_test)\n");
        assert_eq!(
            unmet("VICTORY_REQUIRE_WARP", NO_WARP, "t", true, &mut Vec::new()),
            Err("VICTORY_REQUIRE_WARP=1: no WARP adapter (t)".to_owned())
        );

        // The compiler's messages are printed with the first skip only.
        let said = Mutex::new(Vec::new());
        assert!(first_time(&said, "E5017: not yet implemented"));
        assert!(!first_time(&said, "E5017: not yet implemented"));
        assert!(first_time(&said, "E5005: not declared"));
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
