//! A DLL a stand-in title ships (`title_loop_b.dll`), written by us: the
//! other half of `title_loop_a.dll`, which says what the two are for.
#![no_std]

use core::ffi::c_void;

/// What `title_loop_b_leaf` returns.
const LEAF: u32 = 0x1B00_0001;

#[link(name = "title_loop_a", kind = "raw-dylib")]
unsafe extern "system" {
    fn title_loop_a_leaf() -> u32;
}

#[unsafe(no_mangle)]
pub extern "system" fn title_loop_b_leaf() -> u32 {
    LEAF
}

/// Calls into `title_loop_a.dll`.
#[unsafe(no_mangle)]
pub extern "system" fn title_loop_b_calls_a() -> u32 {
    // SAFETY: the import takes no arguments.
    unsafe { title_loop_a_leaf() }
}

// See title_loop_a.dll: the first forwarder ends in its `title_loop_a_leaf`,
// the second comes back here without end.
core::arch::global_asm!(
    ".section .drectve,\"yni\"",
    ".ascii \" /EXPORT:title_loop_b_through=title_loop_a.title_loop_a_leaf\"",
    ".ascii \" /EXPORT:title_loop_b_round=title_loop_a.title_loop_a_round\"",
    ".text",
);

/// The DLL's entry point, under the name each toolchain's linker looks for.
#[cfg_attr(target_env = "msvc", unsafe(export_name = "_DllMainCRTStartup"))]
#[cfg_attr(not(target_env = "msvc"), unsafe(export_name = "DllMainCRTStartup"))]
pub extern "system" fn entry(_: *mut c_void, _: u32, _: *mut c_void) -> i32 {
    1
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
