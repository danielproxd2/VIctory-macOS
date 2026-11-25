//! A stand-in for a title, written by us. It imports from DLLs that do not
//! exist on a PC, which is exactly the situation victory-loader handles.
//! Loaded normally, this DLL would fail with "module not found".

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};

const DLL_THREAD_ATTACH: u32 = 2;

#[link(name = "xgameruntime", kind = "raw-dylib")]
unsafe extern "system" {
    fn XGameRuntimeInitialize() -> i32;
}

#[link(name = "victory-probe-missing", kind = "raw-dylib")]
unsafe extern "system" {
    fn VIctoryProbeNotImplemented(x: u64) -> u64;
}

// A DLL the "title" ships: tests/fixtures/title-a. One import by name, one by
// ordinal only and one of data.
#[link(name = "title_a", kind = "raw-dylib")]
unsafe extern "system" {
    fn title_a_named() -> u32;
    #[link_ordinal(7)]
    fn title_a_by_ordinal() -> u32;
    static TITLE_A_DATA: u32;
}

// Delay-loaded (see build.rs): bound through the delay-load table.
#[cfg_attr(
    any(target_env = "msvc", target_abi = "llvm"),
    link(name = "victory-probe-delayed", kind = "raw-dylib")
)]
#[cfg_attr(
    all(target_env = "gnu", not(target_abi = "llvm")),
    link(name = "probe_delayed", kind = "static")
)]
unsafe extern "system" {
    fn VIctoryProbeDelayed() -> u32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetTickCount() -> u32;
    fn GetCurrentProcessId() -> u32;
}

/// Console-only API, answered by the resolver's HLE implementation.
#[unsafe(no_mangle)]
pub extern "system" fn probe_xgameruntime_init() -> i32 {
    unsafe { XGameRuntimeInitialize() }
}

/// Exists on the host too, but the resolver overrides it.
#[unsafe(no_mangle)]
pub extern "system" fn probe_tick() -> u32 {
    unsafe { GetTickCount() }
}

/// Exists on the host and is passed through to it.
#[unsafe(no_mangle)]
pub extern "system" fn probe_pid() -> u32 {
    unsafe { GetCurrentProcessId() }
}

/// An export of a title-shipped DLL, imported by name.
#[unsafe(no_mangle)]
pub extern "system" fn probe_title() -> u32 {
    unsafe { title_a_named() }
}

/// An export of a title-shipped DLL that has an ordinal and no name.
#[unsafe(no_mangle)]
pub extern "system" fn probe_title_ordinal() -> u32 {
    unsafe { title_a_by_ordinal() }
}

/// A data export of a title-shipped DLL: the import slot holds its address.
#[unsafe(no_mangle)]
pub extern "system" fn probe_title_data() -> u32 {
    unsafe { TITLE_A_DATA }
}

/// A delay-loaded console API.
#[unsafe(no_mangle)]
pub extern "system" fn probe_delayed() -> u32 {
    unsafe { VIctoryProbeDelayed() }
}

/// Nobody implements this one: it must land in an unimplemented-import stub.
#[unsafe(no_mangle)]
pub extern "system" fn probe_missing() -> u64 {
    unsafe { VIctoryProbeNotImplemented(42) }
}

/// What every thread's copy of `TLS_COUNTER` starts as: a value of the
/// image's TLS template, which the tests look for there.
const TLS_TEMPLATE: u32 = 0x4154_0029;

thread_local! {
    /// Static thread-local data, as titles have: no lazy set-up, the
    /// thread's block is a copy of the template in the image.
    static TLS_COUNTER: Cell<u32> = const { Cell::new(TLS_TEMPLATE) };
}

/// Adds one to the calling thread's counter and returns it: the template's
/// value plus the number of calls on this thread.
#[unsafe(no_mangle)]
pub extern "system" fn probe_tls_counter() -> u32 {
    TLS_COUNTER.with(|counter| {
        let next = counter.get().wrapping_add(1);
        counter.set(next);
        next
    })
}

static TLS_THREAD_STARTS: AtomicU32 = AtomicU32::new(0);
static MAIN_THREAD_STARTS: AtomicU32 = AtomicU32::new(0);

/// A TLS callback, as titles have: counts thread starts.
unsafe extern "system" fn on_thread(_: *mut c_void, reason: u32, _: *mut c_void) {
    if reason == DLL_THREAD_ATTACH {
        TLS_THREAD_STARTS.fetch_add(1, Ordering::Relaxed);
    }
}

#[used]
#[unsafe(link_section = ".CRT$XLB")]
static ON_THREAD: unsafe extern "system" fn(*mut c_void, u32, *mut c_void) = on_thread;

/// Called by the CRT's DLL entry point: counts thread starts too, and calls
/// an import the resolver overrides, as title code calls our runtime.
#[unsafe(no_mangle)]
pub extern "system" fn DllMain(_: *mut c_void, reason: u32, _: *mut c_void) -> i32 {
    if reason == DLL_THREAD_ATTACH {
        MAIN_THREAD_STARTS.fetch_add(1, Ordering::Relaxed);
        unsafe { GetTickCount() };
    }
    if reason == DLL_PROCESS_DETACH {
        DETACHES.fetch_add(1, Ordering::Relaxed);
    }
    1
}

const DLL_PROCESS_DETACH: u32 = 0;
static DETACHES: AtomicU32 = AtomicU32::new(0);

/// `DLL_PROCESS_DETACH` notices seen by `DllMain`.
#[unsafe(no_mangle)]
pub extern "system" fn probe_detaches() -> u32 {
    DETACHES.load(Ordering::Relaxed)
}

/// Thread starts seen by the TLS callback (high half) and `DllMain` (low).
#[unsafe(no_mangle)]
pub extern "system" fn probe_thread_starts() -> u64 {
    // SAFETY: reading our own static keeps the linker from dropping it.
    unsafe { std::ptr::from_ref(&ON_THREAD).read_volatile() };
    u64::from(TLS_THREAD_STARTS.load(Ordering::Relaxed)) << 32
        | u64::from(MAIN_THREAD_STARTS.load(Ordering::Relaxed))
}
