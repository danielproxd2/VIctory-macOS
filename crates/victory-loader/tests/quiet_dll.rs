//! `DisableThreadLibraryCalls` for a title DLL that may ask for it: one
//! without static TLS (tests/fixtures/title-quiet).
//!
//! The one test here has a test program of its own. It counts thread
//! notices, and in a program with other tests their threads start and end
//! at any moment.
#![cfg(windows)]

use victory_loader::UnimplementedPolicy;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Threading::{CreateThread, INFINITE, WaitForSingleObject};

mod common;
use common::fixture;

struct Nothing;

impl victory_loader::ImportResolver for Nothing {
    fn resolve(&self, _: &str, _: &victory_pe::ImportSymbol) -> victory_loader::Resolution {
        victory_loader::Resolution::Missing
    }
}

/// Runs a thread Rust starts and one that starts in the DLL's own code, as
/// a title's threads do, each to its end.
fn run_two_threads(start: usize) {
    std::thread::spawn(|| ()).join().unwrap();
    // SAFETY: `start` is title_quiet.dll's `title_quiet_value`, which takes
    // nothing (the thread's argument is ignored) and returns a `u32`.
    unsafe {
        let start: unsafe extern "system" fn(*mut std::ffi::c_void) -> u32 =
            std::mem::transmute(start);
        let thread = CreateThread(
            std::ptr::null(),
            0,
            Some(start),
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
        );
        assert!(!thread.is_null());
        WaitForSingleObject(thread, INFINITE);
        CloseHandle(thread);
    }
}

#[test]
fn quiet_dll_gets_no_thread_notices() {
    let title = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::fs::copy(
        fixture("title_quiet.dll"),
        title.path().join("title_quiet.dll"),
    )
    .unwrap();
    let wanted = |_: &str| true;
    let dlls =
        victory_loader::load_title_dll("title_quiet.dll", title.path(), cache.path(), &wanted)
            .unwrap();
    let quiet = &dlls[0].1;
    assert!(quiet.info().tls.is_none(), "{:?}", quiet.info().tls);
    let report = quiet
        .bind_imports(&Nothing, UnimplementedPolicy::Trap)
        .unwrap();
    assert_eq!(report.missing, []);

    let notices = || -> (u32, u32) {
        let address = quiet.export("title_quiet_thread_notices").unwrap();
        // SAFETY: the export takes nothing and returns a `u64`.
        let f: extern "system" fn() -> u64 = unsafe { std::mem::transmute(address) };
        let both = f();
        ((both >> 32) as u32, both as u32)
    };
    let start = quiet.export("title_quiet_value").unwrap();

    // Mapped but not initialised, it hears of no thread.
    run_two_threads(start);
    assert_eq!(notices(), (0, 0));

    // Attached, its entry point hears of every start and end.
    // SAFETY: it imports nothing and depends on no other DLL.
    unsafe { quiet.attach() }.unwrap();
    run_two_threads(start);
    let (starts, ends) = notices();
    assert!(starts >= 2 && ends >= 2, "{starts} starts, {ends} ends");

    // It has no static TLS, so it may turn the notices off, as on Windows.
    assert_eq!(
        victory_loader::disable_thread_calls(quiet.base()),
        Some(true)
    );
    let before = notices();
    run_two_threads(start);
    run_two_threads(start);
    assert_eq!(notices(), before, "notices after they were turned off");
    // Asking twice changes nothing.
    assert_eq!(
        victory_loader::disable_thread_calls(quiet.base()),
        Some(true)
    );
    run_two_threads(start);
    assert_eq!(notices(), before);

    // SAFETY: attached above; no thread is in it.
    unsafe { quiet.detach() };
    for (_, image) in dlls {
        // SAFETY: none of its code runs any more.
        unsafe { image.unload() };
    }
}
