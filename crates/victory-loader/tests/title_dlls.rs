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

/// The forwarder string of the export `name`, which must have no code.
fn forward<'a>(image: &'a ImageInfo, name: &str) -> &'a str {
    let export = image
        .exports
        .iter()
        .find(|e| e.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("no export {name}: {:?}", image.exports));
    assert_eq!(export.rva, None, "{name}");
    export.forward.as_deref().unwrap()
}

/// What `image` imports from `dll` (not delay-loaded).
fn imported_from(image: &ImageInfo, dll: &str) -> Vec<String> {
    image
        .imports
        .iter()
        .filter(|m| m.dll.eq_ignore_ascii_case(dll))
        .flat_map(|m| &m.entries)
        .map(|e| e.symbol.to_string())
        .collect()
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
    // It forwards two more: to an export of title_a.dll that has only an
    // ordinal, and to a DLL of the host.
    assert_eq!(forward(&b, "title_b_forwarded_ordinal"), "title_a.#7");
    assert_eq!(
        forward(&b, "title_b_forwarded_host"),
        "kernel32.GetCurrentProcessId"
    );
    // And it delay-loads title_quiet.dll, with a slot for the module handle.
    let delayed: Vec<_> = b
        .delay_imports
        .iter()
        .filter(|m| m.dll.eq_ignore_ascii_case("title_quiet.dll"))
        .collect();
    assert_eq!(delayed.len(), 1, "{:?}", b.delay_imports);
    assert!(delayed[0].module_handle_rva.is_some());
    let symbols: Vec<_> = delayed[0]
        .entries
        .iter()
        .map(|e| e.symbol.to_string())
        .collect();
    assert_eq!(symbols, ["title_quiet_value"]);
    assert!(
        !b.imports
            .iter()
            .any(|m| m.dll.eq_ignore_ascii_case("title_quiet.dll")),
        "title_quiet.dll must be in the delay-load table only"
    );

    // title_loop_a.dll and title_loop_b.dll import each other, and forward
    // to each other: once to an end, once around.
    let (loop_a, loop_b) = (parsed("title_loop_a.dll"), parsed("title_loop_b.dll"));
    assert_eq!(
        imported_from(&loop_a, "title_loop_b.dll"),
        ["title_loop_b_leaf"]
    );
    assert_eq!(
        imported_from(&loop_b, "title_loop_a.dll"),
        ["title_loop_a_leaf"]
    );
    assert_eq!(
        forward(&loop_a, "title_loop_a_through"),
        "title_loop_b.title_loop_b_through"
    );
    assert_eq!(
        forward(&loop_b, "title_loop_b_through"),
        "title_loop_a.title_loop_a_leaf"
    );
    assert_eq!(
        forward(&loop_a, "title_loop_a_round"),
        "title_loop_b.title_loop_b_round"
    );
    assert_eq!(
        forward(&loop_b, "title_loop_b_round"),
        "title_loop_a.title_loop_a_round"
    );
    for image in [&loop_a, &loop_b] {
        assert!(image.tls.is_none() && image.is_dll());
    }

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

    let _one_log = fixture_log(&log);
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
}

/// Points `VICTORY_FIXTURE_LOG` at `log` until the guard is dropped. The
/// variable is the process's, so the tests that log hold this one at a time.
fn fixture_log(log: &std::path::Path) -> impl Drop {
    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());
    struct Guard(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);
    impl Drop for Guard {
        fn drop(&mut self) {
            // SAFETY: as for `set_var` below.
            unsafe { std::env::remove_var("VICTORY_FIXTURE_LOG") };
        }
    }
    let guard = Guard(ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner()));
    // SAFETY: Windows serialises changes to the environment against reads
    // of it, and only the holders of the lock read this variable: the
    // fixtures' `DllMain`s, attached by the test that holds it.
    unsafe { std::env::set_var("VICTORY_FIXTURE_LOG", log) };
    guard
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

fn names(dlls: &[(String, victory_loader::LoadedImage)]) -> Vec<&str> {
    dlls.iter().map(|(name, _)| name.as_str()).collect()
}

/// A DLL name is the title's own text. As a path it could name any file on
/// the machine; only a plain file name in the title's folder is looked for.
#[test]
fn traversal_names_load_nothing() {
    let outer = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let title = outer.path().join("title");
    std::fs::create_dir(&title).unwrap();
    // A DLL that loads, inside the folder and outside it.
    let escaped = outer.path().join("escape.dll");
    std::fs::copy(fixture("title_a.dll"), &escaped).unwrap();
    std::fs::copy(fixture("title_a.dll"), title.join("inside.dll")).unwrap();
    let absolute = escaped.to_str().unwrap().to_owned();
    let outside = [
        "..\\escape.dll",
        "../escape.dll",
        "inside.dll\\..\\..\\escape.dll",
        ".\\inside.dll",
        "inside.dll.",
        "inside.dll::$DATA",
        absolute.as_str(),
    ];

    // What the title asks for itself.
    let load =
        |name: &str| victory_loader::load_title_dll(name, &title, cache.path(), &|_| true).unwrap();
    for name in outside {
        assert_eq!(names(&load(name)), [""; 0], "{name:?}");
    }

    // What its import table names.
    let named = |name: &str| ImportSymbol::Name {
        hint: 0,
        name: name.to_owned(),
    };
    let mut importer = victory_pe::testing::PeBuilder::new().dll(true);
    for name in outside {
        importer = importer.import(name, &[named("title_a_named")]);
    }
    importer = importer.delay_import(
        "..\\escape.dll",
        victory_pe::testing::DELAYLOAD_RVA_BASED,
        &[named("title_a_named")],
    );
    let importer = ImageInfo::parse(&importer.build().unwrap().bytes).unwrap();
    let asked: Vec<_> = importer.imports.iter().map(|m| m.dll.as_str()).collect();
    assert_eq!(asked, outside);
    assert_eq!(importer.delay_imports[0].dll, "..\\escape.dll");
    let dlls = victory_loader::load_title_dlls(&importer, &title, cache.path(), &|_| true).unwrap();
    assert_eq!(names(&dlls), [""; 0]);

    // The plain name is found, so the folder and the file are as said.
    let dlls = load("inside.dll");
    assert_eq!(names(&dlls), ["inside.dll"]);
    for (_, image) in dlls {
        // SAFETY: none of its code ran or will.
        unsafe { image.unload() };
    }
}

/// A title's folder holding copies of the fixtures `files`.
fn title_dir(files: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for file in files {
        std::fs::copy(fixture(file), dir.path().join(file)).unwrap();
    }
    dir
}

/// Calls the export `name` of `image`, which takes nothing and returns a
/// `u32`.
///
/// # Safety
/// The export has that signature and the imports it uses are bound.
unsafe fn call(image: &victory_loader::LoadedImage, name: &str) -> u32 {
    let address = image.export(name).unwrap_or_else(|| panic!("{name}"));
    // SAFETY: the caller's.
    let f: extern "system" fn() -> u32 = unsafe { std::mem::transmute(address) };
    f()
}

/// title_b.dll imports title_a.dll and delay-loads title_quiet.dll: both
/// are mapped before it, and initialised before it.
#[test]
fn chain_loads_dependencies_first() {
    let title = title_dir(&["title_a.dll", "title_b.dll", "title_quiet.dll"]);
    let cache = tempfile::tempdir().unwrap();
    let dlls = victory_loader::load_title_dll("TITLE_B.DLL", title.path(), cache.path(), &|_| true)
        .unwrap();
    assert_eq!(
        names(&dlls),
        ["title_a.dll", "title_quiet.dll", "title_b.dll"]
    );

    // The same from an image that imports title_b.dll, which is how a
    // title's executable gets them.
    let importer = victory_pe::testing::PeBuilder::new()
        .import(
            "Title_B.dll",
            &[ImportSymbol::Name {
                hint: 0,
                name: "title_b_chain".into(),
            }],
        )
        .build()
        .unwrap();
    let importer = ImageInfo::parse(&importer.bytes).unwrap();
    let other_cache = tempfile::tempdir().unwrap();
    let again =
        victory_loader::load_title_dlls(&importer, title.path(), other_cache.path(), &|_| true)
            .unwrap();
    assert_eq!(
        names(&again),
        ["title_a.dll", "title_quiet.dll", "title_b.dll"]
    );
    for (_, image) in again {
        // SAFETY: none of its code ran or will.
        unsafe { image.unload() };
    }

    // A folder without the delay-loaded DLL still gives the chain.
    let short = title_dir(&["title_a.dll", "title_b.dll"]);
    let short_cache = tempfile::tempdir().unwrap();
    let two =
        victory_loader::load_title_dll("title_b.dll", short.path(), short_cache.path(), &|_| true)
            .unwrap();
    assert_eq!(names(&two), ["title_a.dll", "title_b.dll"]);
    for (_, image) in two {
        // SAFETY: none of its code ran or will.
        unsafe { image.unload() };
    }

    // Attached in the order given, each DLL finds the ones it needs ready.
    let resolver = victory_loader::WithTitleDlls {
        inner: &HostOnly,
        dlls: &dlls,
    };
    let folder = tempfile::tempdir().unwrap();
    let log = folder.path().join("notices.log");
    let one_log = fixture_log(&log);
    for (name, dll) in &dlls {
        let report = dll
            .bind_imports(&resolver, UnimplementedPolicy::Trap)
            .unwrap();
        assert_eq!(report.missing, [], "{name}");
        // SAFETY: bound, and the DLLs before it in the list are attached.
        unsafe { dll.attach() }.unwrap();
    }
    let title_b = &dlls[2].1;
    // SAFETY: both exports take nothing and return a `u32`; the DLLs they
    // call into are bound and attached.
    unsafe {
        assert_eq!(call(title_b, "title_b_chain"), 0x0B00_0000 | 0x0A00_0001);
        assert_eq!(call(title_b, "title_b_delayed"), 0x0C00_0001);
    }
    for (_, dll) in dlls.iter().rev() {
        // SAFETY: attached above; nothing else uses them.
        unsafe { dll.detach() };
    }
    drop(one_log);
    // Process attaches and detaches only: threads of other tests start and
    // end meanwhile, and the fixtures log those notices too.
    let text = std::fs::read_to_string(&log).unwrap();
    let process: Vec<_> = text
        .lines()
        .filter(|l| l.ends_with(" 1") || l.ends_with(" 0"))
        .collect();
    assert_eq!(
        process,
        ["title_a 1", "title_b 1", "title_b 0", "title_a 0"],
        "{text}"
    );
}

/// Two DLLs that import each other: each is mapped once, the one asked for
/// last, and each reaches the other.
#[test]
fn import_cycle_terminates() {
    let title = title_dir(&["title_loop_a.dll", "title_loop_b.dll"]);
    let cache = tempfile::tempdir().unwrap();
    let dlls =
        victory_loader::load_title_dll("title_loop_a.dll", title.path(), cache.path(), &|_| true)
            .unwrap();
    assert_eq!(names(&dlls), ["title_loop_b.dll", "title_loop_a.dll"]);
    assert_ne!(dlls[0].1.base(), dlls[1].1.base());

    let resolver = victory_loader::WithTitleDlls {
        inner: &HostOnly,
        dlls: &dlls,
    };
    for (name, dll) in &dlls {
        let report = dll
            .bind_imports(&resolver, UnimplementedPolicy::Trap)
            .unwrap();
        assert_eq!(report.missing, [], "{name}");
        assert_eq!((report.title, report.hle, report.host), (1, 0, 0), "{name}");
    }
    // SAFETY: the exports take nothing and return a `u32`, and the one
    // import each uses is bound. Neither DLL has anything to initialise.
    unsafe {
        assert_eq!(call(&dlls[1].1, "title_loop_a_calls_b"), 0x1B00_0001);
        assert_eq!(call(&dlls[0].1, "title_loop_b_calls_a"), 0x1A00_0001);
    }

    // Asked for from the other end, the order is the other way round.
    let other_cache = tempfile::tempdir().unwrap();
    let wanted = |_: &str| true;
    let reversed = victory_loader::load_title_dll(
        "title_loop_b.dll",
        title.path(),
        other_cache.path(),
        &wanted,
    )
    .unwrap();
    assert_eq!(names(&reversed), ["title_loop_a.dll", "title_loop_b.dll"]);
    for (_, image) in reversed {
        // SAFETY: none of its code ran or will.
        unsafe { image.unload() };
    }
}
