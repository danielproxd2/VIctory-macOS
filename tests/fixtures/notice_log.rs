//! Where title_a.dll and title_b.dll write the notices their `DllMain`s
//! get, one line a notice ("<name> <reason>"). Both include this file.
//!
//! The file is `fixture-notices.log` in the folder the DLL was loaded from,
//! if a test put one there: the DLL's own folder is the test's own, so tests
//! that run side by side in one process each read their own notices. With
//! no such file it is the one `VICTORY_FIXTURE_LOG` names, which is for a
//! process that runs one title.

use std::ffi::{OsString, c_void};
use std::io::Write;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;

/// The log a test puts beside a DLL to get its notices.
const BESIDE: &str = "fixture-notices.log";

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleFileNameW(module: *mut c_void, name: *mut u16, size: u32) -> u32;
    fn CreateMutexW(attributes: *const c_void, owned: i32, name: *const u16) -> *mut c_void;
    fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
    fn ReleaseMutex(handle: *mut c_void) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

/// `WaitForSingleObject` without a time limit.
const INFINITE: u32 = u32::MAX;

/// The one writer of this process's logs at a time, until it is dropped.
///
/// Each DLL that includes this file has statics of its own, so what they
/// share is a mutex the system keeps by name. One whose owner was ended
/// while it wrote is handed on by the system, so nothing waits for good.
struct OneWriter(*mut c_void);

impl OneWriter {
    fn wait() -> OneWriter {
        let name: Vec<u16> = format!("victory-fixture-notices-{}\0", std::process::id())
            .encode_utf16()
            .collect();
        // SAFETY: a NUL-terminated name; no security attributes.
        let mutex = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if !mutex.is_null() {
            // SAFETY: a mutex handle of ours.
            unsafe { WaitForSingleObject(mutex, INFINITE) };
        }
        OneWriter(mutex)
    }
}

impl Drop for OneWriter {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the handle `wait` made; released by the thread that
            // waited for it.
            unsafe {
                ReleaseMutex(self.0);
                CloseHandle(self.0);
            }
        }
    }
}

/// The folder the DLL `module` was loaded from.
fn folder(module: *mut c_void) -> Option<PathBuf> {
    let mut name = [0u16; 1024];
    // SAFETY: the buffer is as long as said.
    let len = unsafe { GetModuleFileNameW(module, name.as_mut_ptr(), name.len() as u32) } as usize;
    // 0 is a failure, and a full buffer a name that was cut.
    if len == 0 || len >= name.len() {
        return None;
    }
    let path = PathBuf::from(OsString::from_wide(&name[..len]));
    path.parent().map(|p| p.to_owned())
}

/// Appends "<name> <reason>" for the DLL `module` to its log, if it has one.
///
/// Notices come on any thread, and two DLLs may share a log. A line is one
/// write, so lines do not mix; and one writer at a time opens the file and
/// writes, because appending alone does not make writers take turns
/// everywhere: under Wine two lines written at the same moment landed at
/// the same place, and one was lost.
pub fn log(module: *mut c_void, name: &str, reason: u32) {
    let beside = folder(module)
        .map(|f| f.join(BESIDE))
        .filter(|p| p.is_file());
    let Some(path) = beside.or_else(|| std::env::var_os("VICTORY_FIXTURE_LOG").map(PathBuf::from))
    else {
        return;
    };
    let line = format!("{name} {reason}\n");
    let _one_writer = OneWriter::wait();
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = file.write(line.as_bytes());
    }
}
