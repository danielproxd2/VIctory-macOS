//! The DLLs a title ships: the stand-ins in tests/fixtures, and one that
//! cannot be loaded.
#![cfg(windows)]

use std::ffi::CString;
use std::os::windows::ffi::OsStrExt;

use victory_loader::{ImportResolver, Provenance, Resolution, UnimplementedPolicy};
use victory_pe::{ImageInfo, ImportSymbol};
use windows_sys::Win32::Foundation::FreeLibrary;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA, LoadLibraryExW};

mod common;
use common::fixture;

fn parsed(file: &str) -> ImageInfo {
    ImageInfo::parse(&std::fs::read(fixture(file)).unwrap()).unwrap()
}

/// Later tests lean on what each fixture is, so it is checked here once.
#[test]
fn the_title_dll_fixtures_are_what_their_names_say() {
    let a = parsed("title_a.dll");
    let export = |name: &str| a.exports.iter().find(|e| e.name.as_deref() == Some(name));
    assert!(export("title_a_named").is_some_and(|e| e.rva.is_some()));
    assert!(export("TITLE_A_DATA").is_some_and(|e| e.rva.is_some()));
    assert!(
        a.exports
            .iter()
            .any(|e| e.ordinal == 7 && e.name.is_none() && e.rva.is_some()),
        "{:?}",
        a.exports
    );
    assert!(
        !a.imports
            .iter()
            .any(|m| m.dll.to_ascii_lowercase().starts_with("title_")),
        "title_a.dll is the end of the chain"
    );

    // title_b.dll imports title_a.dll and forwards an export to it.
    let b = parsed("title_b.dll");
    let from_a: Vec<_> = b
        .imports
        .iter()
        .filter(|m| m.dll.eq_ignore_ascii_case("title_a.dll"))
        .flat_map(|m| &m.entries)
        .map(|e| e.symbol.to_string())
        .collect();
    assert!(from_a.iter().any(|s| s == "title_a_named"), "{from_a:?}");
    let forwarded = b
        .exports
        .iter()
        .find(|e| e.name.as_deref() == Some("title_b_forwarded"))
        .expect("title_b.dll must export title_b_forwarded");
    assert_eq!(forwarded.rva, None);
    assert_eq!(forwarded.forward.as_deref(), Some("title_a.title_a_named"));
    assert!(
        b.exports
            .iter()
            .any(|e| e.name.as_deref() == Some("title_b_chain") && e.rva.is_some())
    );

    // title_quiet.dll has nothing that runs per thread, and imports nothing.
    let quiet = parsed("title_quiet.dll");
    assert!(quiet.tls.is_none(), "{:?}", quiet.tls);
    assert_eq!(quiet.import_count(), 0, "{:?}", quiet.imports);
    assert!(quiet.is_dll());
    // It is a DLL Windows itself loads, which the tests that plant it rely on.
    let path: Vec<u16> = fixture("title_quiet.dll")
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // SAFETY: a NUL-terminated path to our own fixture, whose entry point
    // does nothing; the export takes nothing and returns a `u32`.
    unsafe {
        let module = LoadLibraryExW(path.as_ptr(), std::ptr::null_mut(), 0);
        assert!(!module.is_null(), "{}", std::io::Error::last_os_error());
        let value = GetProcAddress(module, c"title_quiet_value".as_ptr().cast())
            .expect("title_quiet.dll must export title_quiet_value");
        let value: extern "system" fn() -> u32 = std::mem::transmute(value);
        assert_eq!(value(), 0x0C00_0001);
        FreeLibrary(module);
    }
}

/// Host DLLs only: what title_a.dll's own runtime imports.
struct HostOnly;

impl ImportResolver for HostOnly {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        let ImportSymbol::Name { name, .. } = symbol else {
            return Resolution::Missing;
        };
        let (Ok(dll), Ok(name)) = (CString::new(dll), CString::new(name.as_str())) else {
            return Resolution::Missing;
        };
        // SAFETY: NUL-terminated strings; loading system DLLs into the test process.
        let address = unsafe {
            let module = LoadLibraryA(dll.as_ptr().cast());
            if module.is_null() {
                return Resolution::Missing;
            }
            GetProcAddress(module, name.as_ptr().cast())
        };
        address.map_or(Resolution::Missing, |f| Resolution::Found {
            address: f as *const () as usize,
            provenance: Provenance::Host,
        })
    }
}

/// title_a.dll's `DllMain` writes the notices it gets to the file
/// `VICTORY_FIXTURE_LOG` names, which is how tests see initialisation order.
#[test]
fn a_title_dll_fixture_logs_its_notices() {
    let folder = tempfile::tempdir().unwrap();
    let log = folder.path().join("notices.log");
    let title = tempfile::tempdir().unwrap();
    std::fs::copy(fixture("title_a.dll"), title.path().join("title_a.dll")).unwrap();
    let dlls =
        victory_loader::load_title_dll("title_a.dll", title.path(), folder.path(), &|_| true)
            .unwrap();
    let title_a = &dlls[0].1;
    let report = title_a
        .bind_imports(&HostOnly, UnimplementedPolicy::Trap)
        .unwrap();
    assert_eq!(report.missing, []);

    // SAFETY: Windows serialises changes to the environment against reads
    // of it, and no other test in this program reads this variable.
    unsafe { std::env::set_var("VICTORY_FIXTURE_LOG", &log) };
    let lines = || -> Vec<String> {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        text.lines().map(str::to_owned).collect()
    };
    assert!(lines().is_empty(), "nothing is attached yet: {:?}", lines());
    // SAFETY: its imports are bound, and it depends on no other title DLL.
    unsafe { title_a.attach() }.unwrap();
    assert_eq!(lines().first().map(String::as_str), Some("title_a 1"));
    // SAFETY: attached above, and nothing else uses it.
    unsafe { title_a.detach() };
    assert_eq!(lines().last().map(String::as_str), Some("title_a 0"));
    // SAFETY: as for `set_var` above.
    unsafe { std::env::remove_var("VICTORY_FIXTURE_LOG") };
}

/// A title DLL that cannot be loaded is named in the error.
#[test]
fn a_title_dll_that_does_not_prepare_is_named() {
    let title = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::fs::write(title.path().join("broken.dll"), b"MZ not a PE").unwrap();
    let wanted = |_: &str| true;
    let loaded = victory_loader::load_title_dll("broken.dll", title.path(), cache.path(), &wanted);
    let Err(error) = loaded else {
        panic!("a DLL that is not a PE loaded");
    };
    let text = format!("{error:#}");
    assert!(text.contains("broken.dll"), "{text}");
}
