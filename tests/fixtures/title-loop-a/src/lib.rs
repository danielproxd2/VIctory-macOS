//! A DLL a stand-in title ships (`title_loop_a.dll`), written by us. It and
//! `title_loop_b.dll` import each other, which a title's DLLs may do, and
//! forward exports to each other: one pair of forwarders ends in a function
//! after two steps, the other pair names itself and never ends. No runtime,
//! like `title_quiet.dll`, so that nothing but these tables is in it.
#![no_std]

use core::ffi::c_void;

/// What `title_loop_a_leaf` returns.
const LEAF: u32 = 0x1A00_0001;

#[link(name = "title_loop_b", kind = "raw-dylib")]
unsafe extern "system" {
    fn title_loop_b_leaf() -> u32;
}

#[unsafe(no_mangle)]
pub extern "system" fn title_loop_a_leaf() -> u32 {
    LEAF
}

/// Calls into `title_loop_b.dll`.
#[unsafe(no_mangle)]
pub extern "system" fn title_loop_a_calls_b() -> u32 {
    // SAFETY: the import takes no arguments.
    unsafe { title_loop_b_leaf() }
}

// Exports that name the other DLL's exports have no code here. Linker
// directives in the object file say so, which MSVC's linker and lld both
// read. `title_loop_a_through` is `title_loop_b_through`, which is
// `title_loop_a_leaf`; `title_loop_a_round` is `title_loop_b_round`, which
// is `title_loop_a_round` again.
core::arch::global_asm!(
    ".section .drectve,\"yni\"",
    ".ascii \" /EXPORT:title_loop_a_through=title_loop_b.title_loop_b_through\"",
    ".ascii \" /EXPORT:title_loop_a_round=title_loop_b.title_loop_b_round\"",
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
