//! A DLL a stand-in title ships (`title_quiet.dll`), written by us, with
//! nothing that runs per thread: no thread-local storage and no TLS
//! callbacks. The C runtime brings both on some toolchains, so this DLL has
//! no runtime at all and its entry point is the function below.
#![no_std]

use core::ffi::c_void;

/// What `title_quiet_value` returns.
const VALUE: u32 = 0x0C00_0001;

#[unsafe(no_mangle)]
pub extern "system" fn title_quiet_value() -> u32 {
    VALUE
}

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
