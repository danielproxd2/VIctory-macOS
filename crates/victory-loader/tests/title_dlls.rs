//! The DLLs a title ships: the stand-ins in tests/fixtures, and one that
//! cannot be loaded.
#![cfg(windows)]

use std::ffi::CString;
use std::os::windows::ffi::OsStrExt;

use victory_loader::{ImportResolver, Provenance, Resolution, UnimplementedPolicy};
use victory_pe::{ImageInfo, ImportSymbol};
use windows_sys::Win32::Foundation::FreeLibrary;
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleHandleA, GetProcAddress, LoadLibraryA, LoadLibraryExW,
};

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

/// title_a.dll's `DllMain` writes the notices it gets to the log beside
/// the mapped DLL, which is how tests see initialisation order.
#[test]
fn a_title_dll_fixture_logs_its_notices() {
    let folder = tempfile::tempdir().unwrap();
    let log = notices_in(folder.path());
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

/// The log of the title DLLs mapped from the cache folder `cache`: the
/// fixtures write their notices to this file beside themselves where it
/// exists (tests/fixtures/notice_log.rs). The folder is the test's own, so
/// no other test's DLLs write to it, and nothing the tests share is changed.
fn notices_in(cache: &std::path::Path) -> std::path::PathBuf {
    let log = cache.join("fixture-notices.log");
    std::fs::write(&log, "").unwrap();
    log
}

/// The notices of process attach (1) and detach (0) in `log`, in order.
/// Threads of other tests start and end meanwhile, and the fixtures log
/// those notices (2 and 3) too. Every line must be a whole notice.
fn process_notices(log: &std::path::Path) -> Vec<String> {
    let text = std::fs::read_to_string(log).unwrap();
    assert!(text.is_empty() || text.ends_with('\n'), "{text:?}");
    for line in text.lines() {
        let whole = line
            .split_once(' ')
            .is_some_and(|(name, reason)| name.starts_with("title_") && reason.len() == 1);
        assert!(whole && line.ends_with(['0', '1', '2', '3']), "{text:?}");
    }
    text.lines()
        .filter(|l| l.ends_with(" 1") || l.ends_with(" 0"))
        .map(str::to_owned)
        .collect()
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
    let log = notices_in(cache.path());
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
    assert_eq!(
        process_notices(&log),
        ["title_a 1", "title_b 1", "title_b 0", "title_a 0"]
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

fn named(name: &str) -> ImportSymbol {
    ImportSymbol::Name {
        hint: 0,
        name: name.to_owned(),
    }
}

/// An export of a title DLL that names another DLL's export is that other
/// export: of the title's prepared DLL, or of whoever else provides the
/// DLL. The Windows loader, which knows only the unprepared files, is not
/// asked.
#[test]
fn forwarded_export_resolves_into_the_prepared_module() {
    let title = title_dir(&["title_a.dll", "title_b.dll"]);
    let cache = tempfile::tempdir().unwrap();
    let dlls = victory_loader::load_title_dll("title_b.dll", title.path(), cache.path(), &|_| true)
        .unwrap();
    assert_eq!(names(&dlls), ["title_a.dll", "title_b.dll"]);
    let (title_a, title_b) = (&dlls[0].1, &dlls[1].1);
    let resolver = victory_loader::WithTitleDlls {
        inner: &HostOnly,
        dlls: &dlls,
    };

    // By name.
    let target = title_a.export("title_a_named").unwrap();
    let inside = title_a.base()..title_a.base() + title_a.info().size_of_image as usize;
    assert!(inside.contains(&target));
    assert_eq!(
        resolver.resolve("title_b.dll", &named("title_b_forwarded")),
        Resolution::Found {
            address: target,
            provenance: Provenance::Title,
        }
    );
    // SAFETY: title_a.dll's export takes nothing, returns a `u32` and uses
    // no import.
    let f: extern "system" fn() -> u32 = unsafe { std::mem::transmute(target) };
    assert_eq!(f(), 0x0A00_0001);

    // To an export that has only an ordinal.
    let seventh = title_a.export_symbol(&ImportSymbol::Ordinal(7)).unwrap();
    assert!(inside.contains(&seventh) && seventh != target);
    assert_eq!(
        resolver.resolve("TITLE_B.dll", &named("title_b_forwarded_ordinal")),
        Resolution::Found {
            address: seventh,
            provenance: Provenance::Title,
        }
    );
    // By the forwarding export's own ordinal.
    let ordinal = title_b
        .info()
        .exports
        .iter()
        .find(|e| e.name.as_deref() == Some("title_b_forwarded"))
        .unwrap()
        .ordinal;
    assert_eq!(
        resolver.resolve("title_b.dll", &ImportSymbol::Ordinal(ordinal)),
        resolver.resolve("title_b.dll", &named("title_b_forwarded"))
    );

    // To a DLL the title does not ship: whoever answers for it answers.
    assert_eq!(
        resolver.resolve("title_b.dll", &named("title_b_forwarded_host")),
        HostOnly.resolve("kernel32.dll", &named("GetCurrentProcessId"))
    );
    assert!(matches!(
        HostOnly.resolve("kernel32.dll", &named("GetCurrentProcessId")),
        Resolution::Found {
            provenance: Provenance::Host,
            ..
        }
    ));

    // The image alone has no address for an export without code.
    assert_eq!(title_b.export("title_b_forwarded"), None);
    assert_eq!(title_b.export_symbol(&ImportSymbol::Ordinal(ordinal)), None);
    assert!(title_b.export("title_b_chain").is_some());

    // Windows was not asked for the DLL the forwarders name: it would have
    // mapped the file as it is, or found none.
    for name in [c"title_a.dll", c"title_a"] {
        // SAFETY: a NUL-terminated name; no reference is taken.
        let module = unsafe { GetModuleHandleA(name.as_ptr().cast()) };
        assert!(module.is_null(), "{name:?} is loaded under its own name");
    }
}

/// Counts what it is asked and knows nothing.
#[derive(Default)]
struct Asked(std::cell::RefCell<Vec<String>>);

impl ImportResolver for Asked {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        self.0.borrow_mut().push(format!("{dll}!{symbol}"));
        Resolution::Missing
    }
}

/// Forwarders that name each other have no address: the import is missing,
/// after a fixed number of steps. A chain that ends is followed to its end.
#[test]
fn forwarder_loop_is_an_error() {
    let title = title_dir(&["title_loop_a.dll", "title_loop_b.dll"]);
    let cache = tempfile::tempdir().unwrap();
    let dlls =
        victory_loader::load_title_dll("title_loop_a.dll", title.path(), cache.path(), &|_| true)
            .unwrap();
    assert_eq!(names(&dlls), ["title_loop_b.dll", "title_loop_a.dll"]);
    let loop_a = &dlls[1].1;
    let asked = Asked::default();
    let resolver = victory_loader::WithTitleDlls {
        inner: &asked,
        dlls: &dlls,
    };

    // Two steps: a's forwarder names b's, which names a function of a.
    assert_eq!(
        resolver.resolve("title_loop_a.dll", &named("title_loop_a_through")),
        Resolution::Found {
            address: loop_a.export("title_loop_a_leaf").unwrap(),
            provenance: Provenance::Title,
        }
    );
    assert_eq!(
        *asked.0.borrow(),
        [
            "title_loop_a.dll!title_loop_a_through",
            "title_loop_b.dll!title_loop_b_through",
            "title_loop_a.dll!title_loop_a_leaf",
        ]
    );

    // Around: the import itself and eight forwarders, then no more.
    asked.0.borrow_mut().clear();
    assert_eq!(
        resolver.resolve("title_loop_a.dll", &named("title_loop_a_round")),
        Resolution::Missing
    );
    let steps = asked.0.borrow().clone();
    assert_eq!(steps.len(), 9, "{steps:?}");
    for (i, step) in steps.iter().enumerate() {
        let expected = if i % 2 == 0 {
            "title_loop_a.dll!title_loop_a_round"
        } else {
            "title_loop_b.dll!title_loop_b_round"
        };
        assert_eq!(step, expected, "step {i} of {steps:?}");
    }
}

/// A title can ship one DLL under two names. They are two modules, each
/// with its own data and its own notices.
#[test]
fn identical_bytes_under_two_names_are_two_modules() {
    let title = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    for name in ["one.dll", "two.dll"] {
        std::fs::copy(fixture("title_a.dll"), title.path().join(name)).unwrap();
    }
    let importer = victory_pe::testing::PeBuilder::new()
        .import("one.dll", &[named("title_a_named")])
        .import("two.dll", &[named("title_a_named")])
        .build()
        .unwrap();
    let importer = ImageInfo::parse(&importer.bytes).unwrap();
    let dlls =
        victory_loader::load_title_dlls(&importer, title.path(), cache.path(), &|_| true).unwrap();
    assert_eq!(names(&dlls), ["one.dll", "two.dll"]);
    let (one, two) = (&dlls[0].1, &dlls[1].1);
    assert_ne!(one.base(), two.base(), "one module under two names");

    // Each name resolves into its own module.
    let resolver = victory_loader::WithTitleDlls {
        inner: &HostOnly,
        dlls: &dlls,
    };
    let address = |dll: &str| match resolver.resolve(dll, &named("title_a_named")) {
        Resolution::Found {
            address,
            provenance: Provenance::Title,
        } => address,
        other => panic!("{dll}: {other:?}"),
    };
    let size = one.info().size_of_image as usize;
    assert!((one.base()..one.base() + size).contains(&address("one.dll")));
    assert!((two.base()..two.base() + size).contains(&address("two.dll")));

    // Each is initialised once and told of its end once.
    let log = notices_in(cache.path());
    for (name, dll) in &dlls {
        let report = dll
            .bind_imports(&resolver, UnimplementedPolicy::Trap)
            .unwrap();
        assert_eq!(report.missing, [], "{name}");
        // SAFETY: bound, and it depends on no other title DLL.
        unsafe { dll.attach() }.unwrap();
    }
    for (_, dll) in dlls.iter().rev() {
        // SAFETY: attached above; nothing else uses it.
        unsafe { dll.detach() };
    }
    assert_eq!(
        process_notices(&log),
        ["title_a 1", "title_a 1", "title_a 0", "title_a 0"]
    );

    // The same name again is the copy that is already mapped.
    let again =
        victory_loader::load_title_dll("ONE.DLL", title.path(), cache.path(), &|_| true).unwrap();
    assert_eq!(names(&again), ["one.dll"]);
    assert_eq!(again[0].1.base(), one.base());
}

/// Two title DLLs with one log are attached and detached while threads
/// start and end, which they hear of on those threads. Every notice is a
/// line of its own: a line is written whole or not at all.
#[test]
fn notices_from_many_threads_are_whole_lines() {
    use std::sync::atomic::{AtomicBool, Ordering};
    const STARTERS: usize = 4;
    const ROUNDS: usize = 40;
    let title = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let mut dlls = Vec::new();
    for name in ["one.dll", "two.dll"] {
        std::fs::copy(fixture("title_a.dll"), title.path().join(name)).unwrap();
        dlls.extend(
            victory_loader::load_title_dll(name, title.path(), cache.path(), &|_| true).unwrap(),
        );
    }
    assert_eq!(names(&dlls), ["one.dll", "two.dll"]);
    for (name, dll) in &dlls {
        let report = dll
            .bind_imports(&HostOnly, UnimplementedPolicy::Trap)
            .unwrap();
        assert_eq!(report.missing, [], "{name}");
    }
    let log = notices_in(cache.path());

    static DONE: AtomicBool = AtomicBool::new(false);
    let starters: Vec<_> = (0..STARTERS)
        .map(|_| {
            std::thread::spawn(|| {
                let mut started = 0usize;
                while !DONE.load(Ordering::Relaxed) {
                    std::thread::spawn(|| ()).join().unwrap();
                    started += 1;
                }
                started
            })
        })
        .collect();
    for _ in 0..ROUNDS {
        for (_, dll) in &dlls {
            // SAFETY: bound, and it depends on no other title DLL.
            unsafe { dll.attach() }.unwrap();
        }
        // Long enough for threads to start and end while both are attached.
        std::thread::sleep(std::time::Duration::from_millis(2));
        for (_, dll) in dlls.iter().rev() {
            // SAFETY: attached above; nothing else calls into it.
            unsafe { dll.detach() };
        }
    }
    DONE.store(true, Ordering::Relaxed);
    let started: usize = starters.into_iter().map(|s| s.join().unwrap()).sum();
    assert!(started > 0);

    // Checks every line, the threads' too.
    let process = process_notices(&log);
    let round = ["title_a 1", "title_a 1", "title_a 0", "title_a 0"];
    assert_eq!(process.len(), ROUNDS * round.len(), "{process:?}");
    for (i, notices) in process.chunks(round.len()).enumerate() {
        assert_eq!(notices, round, "round {i}");
    }
}

/// What the module-handle slot of `image`'s delay-load descriptor for
/// title_quiet.dll holds.
fn quiet_handle_slot(image: &victory_loader::LoadedImage) -> usize {
    let descriptor = image
        .info()
        .delay_imports
        .iter()
        .find(|m| m.dll.eq_ignore_ascii_case("title_quiet.dll"))
        .expect("a delay-load descriptor for title_quiet.dll");
    let rva = descriptor.module_handle_rva.expect("a module-handle slot");
    // SAFETY: the parser checked that the slot's 8 bytes lie in the image,
    // which is mapped.
    unsafe { ((image.base() + rva as usize) as *const usize).read_unaligned() }
}

/// The delay-load helper keeps the handle of the DLL it loaded in a slot of
/// the descriptor. We bind delay imports ourselves, so we store it: for a
/// DLL the title ships, the prepared module.
#[test]
fn delay_handle_slot_holds_the_title_dll_base() {
    let title = title_dir(&["title_a.dll", "title_b.dll", "title_quiet.dll"]);
    let cache = tempfile::tempdir().unwrap();
    let dlls = victory_loader::load_title_dll("title_b.dll", title.path(), cache.path(), &|_| true)
        .unwrap();
    assert_eq!(
        names(&dlls),
        ["title_a.dll", "title_quiet.dll", "title_b.dll"]
    );
    let (title_quiet, title_b) = (&dlls[1].1, &dlls[2].1);
    assert_eq!(quiet_handle_slot(title_b), 0, "nothing is bound yet");

    let resolver = victory_loader::WithTitleDlls {
        inner: &HostOnly,
        dlls: &dlls,
    };
    assert_eq!(resolver.module("Title_Quiet.DLL"), Some(title_quiet.base()));
    assert_eq!(resolver.module("kernel32.dll"), None);
    let report = title_b
        .bind_imports(&resolver, UnimplementedPolicy::Trap)
        .unwrap();
    assert_eq!(report.missing, []);
    assert_eq!(quiet_handle_slot(title_b), title_quiet.base());
    // The slot is the one thing written beside the import slots: the
    // delay-loaded function is still reached.
    title_quiet
        .bind_imports(&resolver, UnimplementedPolicy::Trap)
        .unwrap();
    // SAFETY: the export takes nothing and returns a `u32`; the one import
    // it uses is bound, into a DLL with nothing to initialise.
    assert_eq!(unsafe { call(title_b, "title_b_delayed") }, 0x0C00_0001);

    // A resolver in front of another one that has the title's DLLs, as
    // DLLs the title loads later are bound.
    let none: [(String, victory_loader::LoadedImage); 0] = [];
    let outer = victory_loader::WithTitleDlls {
        inner: &resolver,
        dlls: &none,
    };
    assert_eq!(outer.module("title_quiet.dll"), Some(title_quiet.base()));
}

extern "system" fn runtime_quiet_value() -> u32 {
    0x5EED
}

/// Answers for title_quiet.dll as our runtime answers for a console DLL.
struct RuntimeQuiet;

impl ImportResolver for RuntimeQuiet {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        if dll.eq_ignore_ascii_case("title_quiet.dll") && *symbol == named("title_quiet_value") {
            return Resolution::Found {
                address: runtime_quiet_value as *const () as usize,
                provenance: Provenance::Hle,
            };
        }
        HostOnly.resolve(dll, symbol)
    }
}

/// A delay-loaded DLL that our runtime provides, or nobody, has no module
/// of the title's to name: the slot stays as the linker left it, null, which
/// is what the title sees before the first call of a delay-loaded function.
#[test]
fn delay_handle_slot_stays_null_for_hle() {
    // The folder has no title_quiet.dll.
    let title = title_dir(&["title_a.dll", "title_b.dll"]);
    let cache = tempfile::tempdir().unwrap();
    let dlls = victory_loader::load_title_dll("title_b.dll", title.path(), cache.path(), &|_| true)
        .unwrap();
    assert_eq!(names(&dlls), ["title_a.dll", "title_b.dll"]);
    let title_b = &dlls[1].1;

    // Nobody provides it: the import is missing, the slot null.
    let host_only = victory_loader::WithTitleDlls {
        inner: &HostOnly,
        dlls: &dlls,
    };
    let report = title_b
        .bind_imports(&host_only, UnimplementedPolicy::ReturnZero)
        .unwrap();
    let missing: Vec<_> = report
        .missing
        .iter()
        .map(|m| format!("{}!{}", m.dll.to_ascii_lowercase(), m.symbol))
        .collect();
    assert_eq!(missing, ["title_quiet.dll!title_quiet_value"]);
    assert_eq!(quiet_handle_slot(title_b), 0);

    // Our runtime provides it: the function is bound, the slot null.
    let runtime = victory_loader::WithTitleDlls {
        inner: &RuntimeQuiet,
        dlls: &dlls,
    };
    assert_eq!(runtime.module("title_quiet.dll"), None);
    let report = title_b
        .bind_imports(&runtime, UnimplementedPolicy::Trap)
        .unwrap();
    assert_eq!(report.missing, []);
    assert_eq!(report.hle, 1);
    assert_eq!(quiet_handle_slot(title_b), 0);
    // SAFETY: the export takes nothing and returns a `u32`; its import is
    // bound to the function above.
    assert_eq!(unsafe { call(title_b, "title_b_delayed") }, 0x5EED);
}
