//! End-to-end: prepare, load, bind and call into a stand-in title
//! (tests/fixtures/hle-probe), without any game data.
#![cfg(windows)]

use std::ffi::CString;

use victory_loader::{
    ImportResolver, Provenance, Resolution, UnimplementedPolicy, WithTitleDlls, prepare,
    unimplemented_call_count,
};
use victory_pe::ImportSymbol;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
use windows_sys::Win32::System::Threading::{
    CreateThread, GetExitCodeThread, INFINITE, WaitForSingleObject,
};

mod common;
use common::fixture;

/// What tests/fixtures/title-a answers with.
const TITLE_A_NAMED: u32 = 0x0A00_0001;
const TITLE_A_BY_ORDINAL: u32 = 0x0A00_0007;
const TITLE_A_DATA: u32 = 0x0A00_DA7A;

/// A title's folder holding copies of the fixtures `files`, each under the
/// name it is built with.
fn title_dir(files: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for file in files {
        std::fs::copy(fixture(file), dir.path().join(file)).unwrap();
    }
    dir
}

extern "system" fn fake_xgameruntime_initialize() -> i32 {
    0x1234
}
/// Asks for the current thread, as our logging does: on a thread std is
/// still starting, that must not abort the process.
extern "system" fn fake_get_tick_count() -> u32 {
    let _ = std::thread::current().id();
    777
}
extern "system" fn fake_delayed() -> u32 {
    4242
}

struct TestResolver;

impl ImportResolver for TestResolver {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        let dll = dll.to_ascii_lowercase();
        let ImportSymbol::Name { name, .. } = symbol else {
            return Resolution::Missing;
        };
        let hle = match (dll.as_str(), name.as_str()) {
            ("xgameruntime.dll", "XGameRuntimeInitialize") => {
                Some(fake_xgameruntime_initialize as *const () as usize)
            }
            ("kernel32.dll", "GetTickCount") => Some(fake_get_tick_count as *const () as usize),
            ("victory-probe-delayed.dll", "VIctoryProbeDelayed") => {
                Some(fake_delayed as *const () as usize)
            }
            _ => None,
        };
        if let Some(address) = hle {
            return Resolution::Found {
                address,
                provenance: Provenance::Hle,
            };
        }
        host(&dll, name)
            .map(|address| Resolution::Found {
                address,
                provenance: Provenance::Host,
            })
            .unwrap_or(Resolution::Missing)
    }
}

/// Passes every question on to `inner` and keeps what it answered: who
/// provides each import, by the import's own name.
struct Recording<'a> {
    inner: &'a dyn ImportResolver,
    answers: std::cell::RefCell<Vec<(String, Option<Provenance>)>>,
}

impl ImportResolver for Recording<'_> {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        let answer = self.inner.resolve(dll, symbol);
        let provenance = match answer {
            Resolution::Found { provenance, .. } => Some(provenance),
            Resolution::Missing => None,
        };
        let import = format!("{}!{symbol}", dll.to_ascii_lowercase());
        self.answers.borrow_mut().push((import, provenance));
        answer
    }
}

fn host(dll: &str, name: &str) -> Option<usize> {
    let dll = CString::new(dll).ok()?;
    let name = CString::new(name).ok()?;
    // SAFETY: NUL-terminated strings; loading system DLLs into the test process.
    unsafe {
        let module = LoadLibraryA(dll.as_ptr().cast());
        if module.is_null() {
            return None;
        }
        GetProcAddress(module, name.as_ptr().cast()).map(|f| f as *const () as usize)
    }
}

#[test]
fn loads_binds_and_calls_probe() {
    let original = std::fs::read(fixture("hle_probe.dll")).unwrap();
    let prepared = prepare(&original).unwrap();

    let imports: Vec<_> = prepared
        .info
        .imports
        .iter()
        .map(|m| m.dll.to_ascii_lowercase())
        .collect();
    assert!(
        imports.contains(&"xgameruntime.dll".to_owned()),
        "{imports:?}"
    );
    assert!(
        imports.contains(&"victory-probe-missing.dll".to_owned()),
        "{imports:?}"
    );
    assert!(
        prepared
            .info
            .delay_imports
            .iter()
            .any(|m| m.dll.eq_ignore_ascii_case("victory-probe-delayed.dll")),
        "fixture must delay-load victory-probe-delayed.dll"
    );

    let dir = tempfile::tempdir().unwrap();
    let image = victory_loader::load(&prepared, dir.path()).unwrap();

    // The "title" ships title_a.dll.
    let title_dir = title_dir(&["title_a.dll"]);
    let dlls =
        victory_loader::load_title_dlls(image.info(), title_dir.path(), dir.path(), &|_| true)
            .unwrap();
    let names: Vec<_> = dlls.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["title_a.dll"]);
    let resolver = WithTitleDlls {
        inner: &TestResolver,
        dlls: &dlls,
    };
    for (_, dll) in &dlls {
        dll.bind_imports(&resolver, UnimplementedPolicy::ReturnZero)
            .unwrap();
        // SAFETY: its imports are bound, and it depends on no other title DLL.
        unsafe { dll.attach() }.unwrap();
    }
    let recording = Recording {
        inner: &resolver,
        answers: Default::default(),
    };
    let report = image
        .bind_imports(&recording, UnimplementedPolicy::ReturnZero)
        .unwrap();
    let answers = recording.answers.into_inner();

    // Who provides each import the probe is written to have, by name. The
    // probe's runtime imports more from the host, and which ones depends
    // on the toolchain that built it, so those are held to a rule instead
    // of a list: whatever is not named here comes from the host.
    const FROM_HLE: [&str; 3] = [
        "xgameruntime.dll!XGameRuntimeInitialize",
        "kernel32.dll!GetTickCount",
        "victory-probe-delayed.dll!VIctoryProbeDelayed",
    ];
    const FROM_TITLE: [&str; 3] = [
        "title_a.dll!title_a_named",
        "title_a.dll!#7",
        "title_a.dll!TITLE_A_DATA",
    ];
    const FROM_HOST: &str = "kernel32.dll!GetCurrentProcessId";
    const MISSING: &str = "victory-probe-missing.dll!VIctoryProbeNotImplemented";
    let provided_by = |wanted: Option<Provenance>| -> Vec<&str> {
        let mut names: Vec<_> = answers
            .iter()
            .filter(|(_, provenance)| *provenance == wanted)
            .map(|(import, _)| import.as_str())
            .collect();
        names.sort_unstable();
        names
    };
    let sorted = |names: &[&'static str]| {
        let mut names = names.to_vec();
        names.sort_unstable();
        names
    };
    assert_eq!(provided_by(Some(Provenance::Hle)), sorted(&FROM_HLE));
    assert_eq!(provided_by(Some(Provenance::Title)), sorted(&FROM_TITLE));
    assert_eq!(provided_by(None), [MISSING]);
    let from_host = provided_by(Some(Provenance::Host));
    assert!(from_host.contains(&FROM_HOST), "{from_host:?}");

    // The loader asked once for each import of the image, delay imports
    // included, and for nothing else.
    let info = image.info();
    let mut asked: Vec<_> = answers.iter().map(|(import, _)| import.clone()).collect();
    let mut imported: Vec<_> = info
        .imports
        .iter()
        .chain(&info.delay_imports)
        .flat_map(|m| {
            let dll = m.dll.to_ascii_lowercase();
            m.entries.iter().map(move |e| format!("{dll}!{}", e.symbol))
        })
        .collect();
    asked.sort_unstable();
    imported.sort_unstable();
    assert_eq!(asked, imported);

    // Its report says the same: each count is the number of imports with
    // that answer, and what is missing is missing by name.
    assert_eq!(report.hle, FROM_HLE.len());
    assert_eq!(report.title, FROM_TITLE.len());
    assert_eq!(report.host, from_host.len());
    let missing: Vec<_> = report
        .missing
        .iter()
        .map(|m| format!("{}!{}", m.dll.to_ascii_lowercase(), m.symbol))
        .collect();
    assert_eq!(missing, [MISSING]);

    let call = |name: &str| -> usize { image.export(name).unwrap_or_else(|| panic!("{name}")) };
    // SAFETY: the exports have the signatures declared in the fixture and
    // every import they use has been bound.
    unsafe {
        let f: extern "system" fn() -> i32 = std::mem::transmute(call("probe_xgameruntime_init"));
        assert_eq!(f(), 0x1234, "console API must reach our implementation");

        let f: extern "system" fn() -> u32 = std::mem::transmute(call("probe_tick"));
        assert_eq!(f(), 777, "host API must be overridable");

        let f: extern "system" fn() -> u32 = std::mem::transmute(call("probe_pid"));
        assert_eq!(f(), std::process::id(), "host API must pass through");

        let f: extern "system" fn() -> u32 = std::mem::transmute(call("probe_title"));
        assert_eq!(f(), TITLE_A_NAMED, "title DLL export must be bound");

        let f: extern "system" fn() -> u32 = std::mem::transmute(call("probe_delayed"));
        assert_eq!(f(), 4242, "delay-loaded API must be bound");

        let before = unimplemented_call_count();
        let f: extern "system" fn() -> u64 = std::mem::transmute(call("probe_missing"));
        assert_eq!(f(), 0);
        assert_eq!(unimplemented_call_count(), before + 1);

        // Once attached, every thread start reaches its TLS callback and
        // DllMain (other tests may start threads too, hence at least one).
        assert!(
            image
                .info()
                .tls
                .as_ref()
                .is_some_and(|t| !t.callback_rvas.is_empty())
        );
        image.attach().unwrap();
        let f: extern "system" fn() -> u64 = std::mem::transmute(call("probe_thread_starts"));
        let before = f();
        // A thread std starts, as our runtime's workers are: notified
        // before std sets it up, which must still find its state pristine.
        let worker = std::thread::Builder::new()
            .name("probe-worker".into())
            .spawn(|| std::thread::current().name().map(str::to_owned))
            .unwrap();
        assert_eq!(worker.join().unwrap().as_deref(), Some("probe-worker"));
        let mid = f();
        assert!(mid >> 32 > before >> 32 && mid as u32 > before as u32);
        let before = mid;
        // Starting in title code, as a title's threads do; the extra
        // argument is ignored.
        let start: unsafe extern "system" fn(*mut std::ffi::c_void) -> u32 =
            std::mem::transmute(call("probe_pid"));
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
        let after = f();
        assert!(
            after >> 32 > before >> 32,
            "TLS callback: {before:#x} → {after:#x}"
        );
        assert!(
            after as u32 > before as u32,
            "DllMain: {before:#x} → {after:#x}"
        );
        // It has static TLS, so Windows would refuse too.
        assert_eq!(
            victory_loader::disable_thread_calls(image.base()),
            Some(false)
        );
        assert_eq!(victory_loader::disable_thread_calls(1), None);

        // `FreeLibrary`'s notice, as a failed late load sends it.
        let f: extern "system" fn() -> u32 = std::mem::transmute(call("probe_detaches"));
        assert_eq!(f(), 0);
        image.detach();
        assert_eq!(f(), 1);
        image.unload();
    }
}

/// The probe imports title_a.dll three ways: by name, by ordinal alone and
/// as data. Each slot must end up pointing into the prepared title DLL.
#[test]
fn binds_ordinal_and_data_imports_of_a_title_dll() {
    let prepared = prepare(&std::fs::read(fixture("hle_probe.dll")).unwrap()).unwrap();
    let from_title_a: Vec<_> = prepared
        .info
        .imports
        .iter()
        .filter(|m| m.dll.eq_ignore_ascii_case("title_a.dll"))
        .flat_map(|m| &m.entries)
        .map(|e| e.symbol.to_string())
        .collect();
    for wanted in ["title_a_named", "#7", "TITLE_A_DATA"] {
        assert!(
            from_title_a.iter().any(|s| s == wanted),
            "the probe must import {wanted} from title_a.dll: {from_title_a:?}"
        );
    }

    let cache = tempfile::tempdir().unwrap();
    let image = victory_loader::load(&prepared, cache.path()).unwrap();
    let title_dir = title_dir(&["title_a.dll"]);
    let dlls =
        victory_loader::load_title_dlls(image.info(), title_dir.path(), cache.path(), &|_| true)
            .unwrap();
    let resolver = WithTitleDlls {
        inner: &TestResolver,
        dlls: &dlls,
    };
    let title_a = &dlls[0].1;
    let by_ordinal = resolver.resolve("title_a.dll", &ImportSymbol::Ordinal(7));
    assert_eq!(
        by_ordinal,
        Resolution::Found {
            address: title_a.export_symbol(&ImportSymbol::Ordinal(7)).unwrap(),
            provenance: Provenance::Title,
        }
    );
    assert_eq!(
        resolver.resolve("title_a.dll", &ImportSymbol::Ordinal(700)),
        Resolution::Missing
    );
    title_a
        .bind_imports(&resolver, UnimplementedPolicy::ReturnZero)
        .unwrap();
    image
        .bind_imports(&resolver, UnimplementedPolicy::ReturnZero)
        .unwrap();

    let call = |name: &str| -> u32 {
        let address = image.export(name).unwrap_or_else(|| panic!("{name}"));
        // SAFETY: the probe declares these exports with this signature, and
        // the imports they use are bound. They are plain calls and a read of
        // constant data, which need neither DLL attached.
        let f: extern "system" fn() -> u32 = unsafe { std::mem::transmute(address) };
        f()
    };
    assert_eq!(call("probe_title"), TITLE_A_NAMED);
    assert_eq!(call("probe_title_ordinal"), TITLE_A_BY_ORDINAL);
    assert_eq!(call("probe_title_data"), TITLE_A_DATA);
}

/// `title_a.dll` has an export with ordinal 7 and no name.
#[test]
fn export_symbol_finds_ordinal_only_exports() {
    let title_dir = title_dir(&["title_a.dll"]);
    let cache = tempfile::tempdir().unwrap();
    let dlls =
        victory_loader::load_title_dll("title_a.dll", title_dir.path(), cache.path(), &|_| true)
            .unwrap();
    let title_a = &dlls[0].1;
    let seventh: Vec<_> = title_a
        .info()
        .exports
        .iter()
        .filter(|e| e.ordinal == 7)
        .collect();
    assert_eq!(seventh.len(), 1, "{:?}", title_a.info().exports);
    assert_eq!(seventh[0].name, None, "ordinal 7 must have no name");
    assert_eq!(title_a.export("title_a_by_ordinal"), None);

    let address = title_a.export_symbol(&ImportSymbol::Ordinal(7)).unwrap();
    assert_eq!(
        address,
        title_a.base() + seventh[0].rva.unwrap() as usize,
        "the ordinal must name the prepared image's own code"
    );
    // SAFETY: the fixture's export 7 takes nothing, returns a `u32` and uses
    // no import.
    let f: extern "system" fn() -> u32 = unsafe { std::mem::transmute(address) };
    assert_eq!(f(), TITLE_A_BY_ORDINAL);

    assert_eq!(title_a.export_symbol(&ImportSymbol::Ordinal(700)), None);
    let named = ImportSymbol::Name {
        hint: 0,
        name: "title_a_named".into(),
    };
    assert_eq!(
        title_a.export_symbol(&named),
        title_a.export("title_a_named")
    );
    assert!(title_a.export("title_a_named").is_some());
}

#[test]
fn maps_a_title_dll_the_title_loads_itself() {
    let title_dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    // It imports only host DLLs, which are not in the folder.
    std::fs::copy(fixture("title_a.dll"), title_dir.path().join("Late.dll")).unwrap();
    let load = |name: &str, wanted: &dyn Fn(&str) -> bool| {
        victory_loader::load_title_dll(name, title_dir.path(), cache.path(), wanted).unwrap()
    };
    let dlls = load("LATE.DLL", &|_| true);
    let names: Vec<_> = dlls.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["late.dll"]);
    assert!(dlls[0].1.export("title_a_named").is_some());
    assert!(load("absent.dll", &|_| true).is_empty());
    assert!(load("late.dll", &|_| false).is_empty());
}

/// What every thread's copy of the probe's thread-local counter starts as
/// (`TLS_TEMPLATE` in tests/fixtures/hle-probe).
const TLS_TEMPLATE: u32 = 0x4154_0029;

/// The probe's static TLS: every thread that calls into the image finds a
/// block of its own there that started as a copy of the image's template.
/// That goes for a thread that was running before the image was mapped
/// (a title's DLL that is loaded late is mapped while its threads run),
/// for a thread Rust starts afterwards and for one that starts in title
/// code. Windows gives a DLL it maps its TLS on every thread; the prepared
/// copy keeps its TLS directory so that it does.
#[test]
fn static_tls_template_reaches_existing_new_and_raw_threads() {
    use std::sync::mpsc::channel;

    // A thread that runs before the image is mapped, and until it is told
    // what to call.
    let (tell, told) = channel::<usize>();
    let (answer, answered) = channel::<[u32; 2]>();
    let (ready, is_ready) = channel::<()>();
    let early = std::thread::spawn(move || {
        ready.send(()).unwrap();
        let address = told.recv().unwrap();
        // SAFETY: the probe's `probe_tls_counter`, which takes nothing and
        // returns a `u32`; it uses no import.
        let f: extern "system" fn() -> u32 = unsafe { std::mem::transmute(address) };
        answer.send([f(), f()]).unwrap();
    });
    is_ready.recv().unwrap();

    let prepared = prepare(&std::fs::read(fixture("hle_probe.dll")).unwrap()).unwrap();
    let cache = tempfile::tempdir().unwrap();
    let image = victory_loader::load(&prepared, cache.path()).unwrap();
    image
        .bind_imports(&TestResolver, UnimplementedPolicy::ReturnZero)
        .unwrap();

    // The counter's first value is in the template the mapped image's TLS
    // directory names: it is static TLS, not a slot filled on first use.
    let tls = image.info().tls.as_ref().expect("the probe has static TLS");
    let directory = (image.base() + tls.directory_rva as usize) as *const usize;
    // SAFETY: the mapped image's TLS directory, whose first two fields are
    // the start and end address of the template, relocated by the OS.
    let template = unsafe {
        let (start, end) = (
            directory.read_unaligned(),
            directory.add(1).read_unaligned(),
        );
        assert!(start >= image.base() && start < end, "{start:#x}..{end:#x}");
        std::slice::from_raw_parts(start as *const u8, end - start)
    };
    assert!(
        template.windows(4).any(|w| w == TLS_TEMPLATE.to_le_bytes()),
        "no {TLS_TEMPLATE:#x} in the {} bytes of the TLS template",
        template.len()
    );

    let address = image
        .export("probe_tls_counter")
        .expect("probe_tls_counter");
    // SAFETY: as in the thread above.
    let counter: extern "system" fn() -> u32 = unsafe { std::mem::transmute(address) };

    // This thread was running before the map too.
    assert_eq!(counter(), TLS_TEMPLATE + 1);
    assert_eq!(counter(), TLS_TEMPLATE + 2);

    // The thread that waited: its own copy, counted from the template.
    tell.send(address).unwrap();
    assert_eq!(
        answered.recv().unwrap(),
        [TLS_TEMPLATE + 1, TLS_TEMPLATE + 2]
    );
    early.join().unwrap();

    // A thread Rust starts now.
    let new = std::thread::spawn(move || counter()).join().unwrap();
    assert_eq!(new, TLS_TEMPLATE + 1);

    // A thread that starts in the title's code: its exit code is what the
    // function returned.
    // SAFETY: the export ignores the thread's argument; the handle is
    // valid until closed.
    let raw = unsafe {
        let start: unsafe extern "system" fn(*mut std::ffi::c_void) -> u32 =
            std::mem::transmute(address);
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
        let mut code = 0;
        assert_ne!(GetExitCodeThread(thread, &mut code), 0);
        CloseHandle(thread);
        code
    };
    assert_eq!(raw, TLS_TEMPLATE + 1);

    // None of them touched this thread's copy.
    assert_eq!(counter(), TLS_TEMPLATE + 3);
    // SAFETY: every thread that called into the image has ended, and this
    // one does not again.
    unsafe { image.unload() };
}

/// In a child process of the next test: binds the probe so that an import
/// nobody has traps, and calls it.
#[test]
fn trap_stub_child() {
    let Some(log) = std::env::var_os("VICTORY_TRAP_STUB_LOG") else {
        return;
    };
    println!();
    victory_loader::log_crashes_to(log.into());
    let prepared = prepare(&std::fs::read(fixture("hle_probe.dll")).unwrap()).unwrap();
    let cache = tempfile::tempdir().unwrap();
    let image = victory_loader::load(&prepared, cache.path()).unwrap();
    let report = image
        .bind_imports(&TestResolver, UnimplementedPolicy::Trap)
        .unwrap();
    for missing in &report.missing {
        println!("missing={}!{}", missing.dll, missing.symbol);
    }
    let address = image.export("probe_missing").expect("probe_missing");
    // SAFETY: the probe's export takes nothing and returns a `u64`; the
    // import it calls is bound to the stub this test is about.
    let probe_missing: extern "system" fn() -> u64 = unsafe { std::mem::transmute(address) };
    println!("calling");
    let returned = probe_missing();
    println!("returned={returned}");
}

/// With the policy that is the default, an import nobody implements ends
/// the process when the title calls it, through the stub the loader wrote
/// into the import's slot, and the crash log says which import it was.
#[test]
fn trapped_import_through_the_stub_is_in_the_crash_log() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("crashes.log");
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "trap_stub_child", "--nocapture"])
        .env("VICTORY_TRAP_STUB_LOG", &log)
        .output()
        .unwrap();
    let printed = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<_> = printed.lines().map(str::trim_end).collect();
    assert!(
        lines.iter().any(|l| l
            .eq_ignore_ascii_case("missing=victory-probe-missing.dll!VIctoryProbeNotImplemented")),
        "{printed}"
    );
    // The call was made and did not come back.
    assert!(lines.contains(&"calling"), "{printed}");
    assert!(!printed.contains("returned="), "{printed}");
    assert!(
        !out.status.success(),
        "the trap ends the process: {printed}"
    );
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let wanted =
        "VIctory: unimplemented import victory-probe-missing.dll!VIctoryProbeNotImplemented called";
    assert!(
        text.to_ascii_lowercase()
            .contains(&wanted.to_ascii_lowercase()),
        "crash log: {text:?}"
    );
}
