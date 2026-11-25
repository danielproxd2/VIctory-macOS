//! A DLL a stand-in title ships (`title_a.dll`), written by us. It exports a
//! function by name, a function by ordinal only and a data symbol, and its
//! `DllMain` writes every notice it gets to the file `VICTORY_FIXTURE_LOG`
//! names. With `VICTORY_FIXTURE_STUCK` set, it does not return from the
//! notice that the process ends.

use std::ffi::c_void;
use std::io::Write;

/// What `title_a_named` returns.
const NAMED: u32 = 0x0A00_0001;
/// What the export with ordinal 7 returns.
const BY_ORDINAL: u32 = 0x0A00_0007;
/// What `TITLE_A_DATA` holds.
const DATA: u32 = 0x0A00_DA7A;

#[unsafe(no_mangle)]
pub extern "system" fn title_a_named() -> u32 {
    NAMED
}

#[unsafe(no_mangle)]
pub static TITLE_A_DATA: u32 = DATA;

// The export with ordinal 7 has no name. Rust exports every symbol it names
// by that name, so the function is assembly, which Rust does not export, and
// the export is a linker directive in the object file, which MSVC's linker
// and lld both read.
std::arch::global_asm!(
    ".globl title_a_by_ordinal",
    "title_a_by_ordinal:",
    "mov eax, {value}",
    "ret",
    ".section .drectve,\"yni\"",
    ".ascii \" /EXPORT:title_a_by_ordinal,@7,NONAME\"",
    ".text",
    value = const BY_ORDINAL,
);

#[unsafe(no_mangle)]
pub extern "system" fn DllMain(_: *mut c_void, reason: u32, _: *mut c_void) -> i32 {
    log("title_a", reason);
    // Told to, it never comes back from the notice of the process's end,
    // as detach code does that waits for a lock a thread held when it was
    // ended.
    if reason == DLL_PROCESS_DETACH && std::env::var_os("VICTORY_FIXTURE_STUCK").is_some() {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
    1
}

const DLL_PROCESS_DETACH: u32 = 0;

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
