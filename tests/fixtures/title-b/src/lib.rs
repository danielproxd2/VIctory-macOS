//! A DLL a stand-in title ships (`title_b.dll`), written by us. It imports
//! `title_a.dll`, so the two form a chain, and it forwards exports: to
//! `title_a.dll` by name and by ordinal, and to a DLL of the host. It
//! delay-loads `title_quiet.dll` (see build.rs). Its `DllMain` writes every
//! notice it gets to the file `VICTORY_FIXTURE_LOG` names.

use std::ffi::c_void;
use std::io::Write;

/// What `title_b_chain` adds to the value it gets from `title_a.dll`.
const CHAIN: u32 = 0x0B00_0000;

#[link(name = "title_a", kind = "raw-dylib")]
unsafe extern "system" {
    fn title_a_named() -> u32;
}

// Delay-loaded (see build.rs): another DLL the title ships, bound through
// the delay-load table, whose descriptor has a slot for its module handle.
#[cfg_attr(
    any(target_env = "msvc", target_abi = "llvm"),
    link(name = "title_quiet", kind = "raw-dylib")
)]
#[cfg_attr(
    all(target_env = "gnu", not(target_abi = "llvm")),
    link(name = "title_quiet_delayed", kind = "static")
)]
unsafe extern "system" {
    fn title_quiet_value() -> u32;
}

/// Calls into `title_quiet.dll` through the delay-load table.
#[unsafe(no_mangle)]
pub extern "system" fn title_b_delayed() -> u32 {
    // SAFETY: the import takes no arguments.
    unsafe { title_quiet_value() }
}

/// Calls into `title_a.dll`.
#[unsafe(no_mangle)]
pub extern "system" fn title_b_chain() -> u32 {
    // SAFETY: the import takes no arguments.
    CHAIN | unsafe { title_a_named() }
}

// `title_b_forwarded` is `title_a.dll`'s `title_a_named`: an export that
// names another DLL's export has no code here. A linker directive in the
// object file says so, which MSVC's linker and lld both read.
// `title_b_forwarded_ordinal` names `title_a.dll`'s export 7, which has no
// name, and `title_b_forwarded_host` a function of the host's kernel32.dll.
std::arch::global_asm!(
    ".section .drectve,\"yni\"",
    ".ascii \" /EXPORT:title_b_forwarded=title_a.title_a_named\"",
    ".ascii \" /EXPORT:title_b_forwarded_ordinal=title_a.#7\"",
    ".ascii \" /EXPORT:title_b_forwarded_host=kernel32.GetCurrentProcessId\"",
    ".text",
);

#[unsafe(no_mangle)]
pub extern "system" fn DllMain(_: *mut c_void, reason: u32, _: *mut c_void) -> i32 {
    log("title_b", reason);
    1
}

/// Appends "<name> <reason>" to the file `VICTORY_FIXTURE_LOG` names.
fn log(name: &str, reason: u32) {
    let Some(path) = std::env::var_os("VICTORY_FIXTURE_LOG") else {
        return;
    };
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path);
    if let Ok(mut file) = file {
        let _ = writeln!(file, "{name} {reason}");
    }
}
