//! A DLL a stand-in title ships (`title_quiet.dll`), written by us, with
//! nothing that runs per thread but its entry point: no thread-local
//! storage and no TLS callbacks. The C runtime brings both on some
//! toolchains, so this DLL has no runtime at all and its entry point is the
//! function below, which counts the thread notices it gets.
#![no_std]

use core::ffi::c_void;

/// What `title_quiet_value` returns.
const VALUE: u32 = 0x0C00_0001;

#[unsafe(no_mangle)]
pub extern "system" fn title_quiet_value() -> u32 {
    VALUE
}

const DLL_THREAD_ATTACH: u32 = 2;
const DLL_THREAD_DETACH: u32 = 3;

// Counted and read with one instruction each: in a debug build `core`'s
// atomic types and volatile reads bring its panic machinery with them,
// and with it imports, and this DLL has none.
static mut THREAD_ATTACHES: u32 = 0;
static mut THREAD_DETACHES: u32 = 0;

/// Adds one to `counter`, whichever threads do so at the same time.
fn count(counter: *mut u32) {
    // SAFETY: one locked instruction on a static of this DLL.
    unsafe { core::arch::asm!("lock inc dword ptr [{}]", in(reg) counter) };
}

fn counted(counter: *const u32) -> u32 {
    let value: u32;
    // SAFETY: one load of a static of this DLL.
    unsafe { core::arch::asm!("mov {:e}, dword ptr [{}]", out(reg) value, in(reg) counter) };
    value
}

/// The thread notices the entry point got: starts in the high half, ends in
/// the low one. A DLL without static TLS may turn them off
/// (`DisableThreadLibraryCalls`), and then this stays as it is.
#[unsafe(no_mangle)]
pub extern "system" fn title_quiet_thread_notices() -> u64 {
    let attaches = counted(&raw const THREAD_ATTACHES) as u64;
    attaches << 32 | counted(&raw const THREAD_DETACHES) as u64
}

/// The DLL's entry point, under the name each toolchain's linker looks for.
#[cfg_attr(target_env = "msvc", unsafe(export_name = "_DllMainCRTStartup"))]
#[cfg_attr(not(target_env = "msvc"), unsafe(export_name = "DllMainCRTStartup"))]
pub extern "system" fn entry(_: *mut c_void, reason: u32, _: *mut c_void) -> i32 {
    if reason == DLL_THREAD_ATTACH {
        count(&raw mut THREAD_ATTACHES);
    } else if reason == DLL_THREAD_DETACH {
        count(&raw mut THREAD_DETACHES);
    }
    1
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
