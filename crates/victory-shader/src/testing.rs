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
//! * `VICTORY_REQUIRE_DIFF=1`: the console and PC compilers;
//! * `VICTORY_REQUIRE_PC_GPU=1`: a host device that gives the results of
//!   the PC hardware the device tests were measured on. Where a host is
//!   known to give another result (Apple's D3DMetal, see `docs/macos.md`,
//!   "Open differences under D3DMetal"), the test that shows it checks
//!   that the result is the measured one, prints the skip line naming the
//!   difference ([`host_differs`]) and passes; with the switch it fails.
//!   A result that is neither the hardware's nor the measured difference
//!   fails either way, and on any other host only the hardware's passes.
//!
//! Every device test of the crate gets its device from [`host`] and its
//! validation from [`dxil`] (`emit::validator::check`), upstream's tests
//! included, so the switches cover all of them. Some of upstream's tests
//! also fail when no device at all ran them (`assert!(ran > 0)`, the
//! VALU tests' [`some_device_ran`], the waterfall and semantics tests):
//! that is kept as upstream wrote it, whatever the switches say. The
//! tests that upstream lets pass on a machine without a device (the
//! hardware-only checks among them) pass here too, after their skip
//! lines.
//!
//! Without its switch such a test passes having checked nothing: a run
//! that is meant to prove the device tests must set
//! `VICTORY_REQUIRE_GPU=1` (and `VICTORY_REQUIRE_DXIL=1` to prove
//! validation), and is otherwise judged by its `skipped:` lines.

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

/// One GPU test at a time. With device tests on the hardware adapter and
/// WARP from several test threads at once, now and then none of their
/// dispatches ever completes and the test binary hangs; and two GPU tests
/// at once on WARP can end the test process with an access violation
/// (7 of 24 runs of the four waterfall tests). Every test that creates a
/// device holds this for as long as it uses the device. Not reentrant: a
/// test takes it once, before its first [`host`] call. A test that
/// failed while holding it does not stop the others.
#[cfg(windows)]
pub(crate) fn one_gpu_test_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
    ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner())
}

/// What a test that runs on every device there is (the hardware adapter
/// and WARP, each a skip where it is missing) ends with, `ran` being the
/// number of devices it ran on: upstream's
/// `assert!(ran > 0, "neither hardware nor WARP SM 6.6 device available")`
/// as a value. A test that ran on no device has compared nothing and
/// fails, with or without a `VICTORY_REQUIRE_*` switch.
pub(crate) fn some_device_ran(ran: usize) -> Result<(), &'static str> {
    if ran > 0 {
        Ok(())
    } else {
        Err("neither hardware nor WARP SM 6.6 device available")
    }
}

/// The host's SM 6.6 device for a test: the hardware adapter, or WARP
/// with `warp`. Without one the test goes on without it after a skip
/// line; `VICTORY_REQUIRE_GPU=1` (hardware) and `VICTORY_REQUIRE_WARP=1`
/// (WARP) make that a failure. D3DMetal under Wine has no WARP. Whether
/// a caller that gets no device from either call fails is the caller's
/// matter ([`some_device_ran`], where upstream's test fails).
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

/// Whether `host` is Apple's D3DMetal (the D3D12 of the Game Porting
/// Toolkit, under Wine): the process runs under Wine and the device's
/// adapter is the one D3DMetal presents, which names itself "AMD
/// Compatibility Mode" (measured: vendor 0x1002, device 0x66AF, on an M5
/// with D3DMetal 4.0 beta 3). No other host is taken for it: a Wine with
/// another D3D12, or Windows on an AMD GPU, is not.
#[cfg(windows)]
pub(crate) fn d3dmetal(host: &crate::emit::host::Host) -> bool {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter1, IDXGIFactory4,
    };
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    // SAFETY: ntdll is always loaded and both names end with a NUL; the
    // DXGI calls are plain queries with valid out pointers.
    unsafe {
        let wine = GetProcAddress(
            GetModuleHandleA(c"ntdll.dll".as_ptr().cast()),
            c"wine_get_version".as_ptr().cast(),
        )
        .is_some();
        if !wine {
            return false;
        }
        let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0)) else {
            return false;
        };
        let luid = host.device.GetAdapterLuid();
        let Ok(adapter) = factory.EnumAdapterByLuid::<IDXGIAdapter1>(luid) else {
            return false;
        };
        adapter
            .GetDesc1()
            .is_ok_and(|desc| is_d3dmetal_adapter(&desc.Description))
    }
}

/// Whether a DXGI adapter description (UTF-16, NUL-padded) is the name
/// D3DMetal gives its adapter.
pub(crate) fn is_d3dmetal_adapter(description: &[u16]) -> bool {
    let end = description
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(description.len());
    String::from_utf16_lossy(&description[..end]) == "AMD Compatibility Mode"
}

/// What a device test does where the host gave the result it is known to
/// give instead of the PC hardware's (`difference` says which, as
/// `docs/macos.md` lists it): the skip line, or a failure with
/// `VICTORY_REQUIRE_PC_GPU=1`. The caller has checked both that the host
/// is the one the difference was measured on ([`d3dmetal`]) and that the
/// result is the measured one.
#[cfg(windows)]
pub(crate) fn host_differs(difference: &str) {
    skip_unless_required("VICTORY_REQUIRE_PC_GPU", difference);
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

    /// A test that ran on no device fails with upstream's message; one
    /// device is enough.
    #[test]
    fn a_test_that_ran_on_no_device_fails() {
        assert_eq!(
            some_device_ran(0),
            Err("neither hardware nor WARP SM 6.6 device available")
        );
        assert_eq!(some_device_ran(1), Ok(()));
        assert_eq!(some_device_ran(2), Ok(()));
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
            "VICTORY_REQUIRE_PC_GPU",
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

    /// A measured host difference is a skip, and a failure naming its
    /// switch when the run requires the PC hardware's results.
    #[test]
    fn a_host_difference_fails_only_when_pc_results_are_required() {
        let var = "VICTORY_REQUIRE_PC_GPU";
        assert_eq!(missing(var, None, "the host rounds otherwise"), Ok(()));
        assert_eq!(
            missing(var, Some("1"), "the host rounds otherwise"),
            Err("VICTORY_REQUIRE_PC_GPU=1: the host rounds otherwise".into())
        );
    }

    /// Only the adapter name D3DMetal presents is taken for it: the whole
    /// NUL-terminated description, not a part of one.
    #[test]
    fn only_d3dmetals_adapter_name_is_taken_for_it() {
        let wide = |name: &str| {
            let mut description: Vec<u16> = name.encode_utf16().collect();
            description.resize(128, 0);
            description
        };
        assert!(is_d3dmetal_adapter(&wide("AMD Compatibility Mode")));
        // Without a terminating NUL: the array's end ends the name.
        let exact: Vec<u16> = "AMD Compatibility Mode".encode_utf16().collect();
        assert!(is_d3dmetal_adapter(&exact));
        for other in [
            "",
            "AMD Radeon RX 6700 XT",
            "AMD Compatibility Mode 2",
            "Microsoft Basic Render Driver",
            "Apple M5",
        ] {
            assert!(!is_d3dmetal_adapter(&wide(other)), "{other}");
        }
    }
}
