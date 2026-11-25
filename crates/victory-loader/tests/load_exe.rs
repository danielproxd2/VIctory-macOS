//! End-to-end with an executable: prepare, load (which redirects its static
//! TLS), bind, start it on its own thread through its entry point, and end
//! the process the way a title does. The executable is a stand-in written
//! by us (tests/fixtures/hle-probe-exe), without any game data.
//!
//! The title runs in a child process: ending it is part of what is tested,
//! and the loader's exit handling ends every other thread of the process.
//! The child prints what the loader told it, the stand-in writes what it
//! saw to a log, and the tests here read both.
#![cfg(windows)]

use std::ffi::CString;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use victory_loader::{ImportResolver, Provenance, Resolution, UnimplementedPolicy, prepare};
use victory_pe::{ImageInfo, ImportSymbol};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

mod common;
use common::fixture;

/// What tests/fixtures/hle-probe-exe is built with and answers with (its
/// build.rs and the constants at the top of its main.rs).
const STACK_RESERVE: u64 = 0x60_0000;
const TEMPLATE_VALUE: u64 = 41;
const GUARD: u64 = 0x7E57_1E55;
const MAIN_ADDS: u64 = 1;
const WORKER_ADDS: [u64; 2] = [100, 200];
const EXIT_BASE: u64 = 7;

/// What the resolver here answers `XGameRuntimeInitialize` with.
const RUNTIME: i32 = 70;

/// The stack `spawn_main` gives a title whose header asks for less.
const SMALLEST_STACK: u64 = 1 << 20;
/// A header value below that, for the child that is told to use it.
const SMALL_RESERVE: u64 = 0x1_0000;

const SCN_MEM_EXECUTE: u32 = 0x2000_0000;
const TLS64_ADDRESS_OF_INDEX: usize = 16;

const DLL_PROCESS_DETACH: u64 = 0;
const DLL_PROCESS_ATTACH: u64 = 1;
const DLL_THREAD_ATTACH: u64 = 2;
const DLL_THREAD_DETACH: u64 = 3;

unsafe extern "C" {
    /// The C runtime's `exit`, which is how a title ends the process.
    fn exit(code: i32) -> !;
}

extern "system" fn fake_xgameruntime_initialize() -> i32 {
    RUNTIME
}

/// Console APIs from here, everything else from the host; keeps who
/// provided each import, by the import's own name.
#[derive(Default)]
struct TestResolver {
    answers: std::cell::RefCell<Vec<(String, &'static str)>>,
}

impl ImportResolver for TestResolver {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        let dll = dll.to_ascii_lowercase();
        let import = format!("{dll}!{symbol}");
        let found = match symbol {
            ImportSymbol::Name { name, .. }
                if dll == "xgameruntime.dll" && name == "XGameRuntimeInitialize" =>
            {
                Some((
                    fake_xgameruntime_initialize as *const () as usize,
                    Provenance::Hle,
                    "hle",
                ))
            }
            ImportSymbol::Name { name, .. } => {
                host(&dll, name).map(|address| (address, Provenance::Host, "host"))
            }
            ImportSymbol::Ordinal(_) => None,
        };
        let by = found.map_or("missing", |(_, _, by)| by);
        self.answers.borrow_mut().push((import, by));
        match found {
            Some((address, provenance, _)) => Resolution::Found {
                address,
                provenance,
            },
            None => Resolution::Missing,
        }
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

fn exe() -> Vec<u8> {
    std::fs::read(fixture("hle-probe-exe.exe")).unwrap()
}

/// Whether `code` starts with `mov r64, gs:[0x58]`, the load of the TLS
/// array that an executable's code indexes with 0 and that the loader's
/// redirect rewrites: `65 REX.W 8B ModRM 25 58 00 00 00`, into any register
/// but `rsp`. Written out here, apart from the loader's own matcher.
fn is_tls_array_load(code: &[u8]) -> bool {
    match code {
        [
            0x65,
            rex @ (0x48 | 0x4C),
            0x8B,
            modrm,
            0x25,
            0x58,
            0,
            0,
            0,
            ..,
        ] => modrm & 0xC7 == 0x04 && !(*rex == 0x48 && modrm >> 3 & 7 == 4),
        _ => false,
    }
}

/// The RVA of every such load in the executable sections of the file.
fn tls_array_loads(file: &[u8], info: &ImageInfo) -> Vec<u32> {
    let mut loads = Vec::new();
    for section in &info.sections {
        if section.characteristics & SCN_MEM_EXECUTE == 0 {
            continue;
        }
        let start = section.file_offset as usize;
        let len = match section.virtual_size {
            0 => section.file_size,
            size => size.min(section.file_size),
        } as usize;
        let code = &file[start..start + len];
        let mut at = 0;
        while at < code.len() {
            if is_tls_array_load(&code[at..]) {
                loads.push(section.rva + at as u32);
                at += 9;
            } else {
                at += 1;
            }
        }
    }
    loads
}

/// In a child process of the tests below (`VICTORY_EXE_CHILD` says which
/// kind): runs the stand-in executable from its file to the end of the
/// process, printing what the loader said on the way.
#[test]
fn title_child() {
    let Some(kind) = std::env::var_os("VICTORY_EXE_CHILD") else {
        return;
    };
    let dir = PathBuf::from(std::env::var_os("VICTORY_EXE_CHILD_DIR").unwrap());
    // The harness has written "test title_child ... " without ending the
    // line; what is printed here starts on lines of its own.
    println!();
    let original = exe();
    let mut prepared = prepare(&original).unwrap();
    if kind == "small-stack" {
        prepared.info.stack_reserve = SMALL_RESERVE;
    }
    let image = victory_loader::load(&prepared, &dir).unwrap();
    println!("redirected={}", image.redirected_tls_loads());

    // What is in memory now where the file has a load: a call and the
    // four-byte no-op that fills the rest of the nine bytes.
    let loads = tls_array_loads(&original, image.info());
    let calls = loads
        .iter()
        .filter(|&&rva| {
            let at = (image.base() + rva as usize) as *const [u8; 9];
            // SAFETY: nine bytes of a code section of the mapped image.
            let code = unsafe { at.read_unaligned() };
            code[0] == 0xE8 && code[5..] == [0x0F, 0x1F, 0x40, 0x00]
        })
        .count();
    println!("calls_in_memory={calls}");
    let tls = image.info().tls.as_ref().unwrap();
    let index_pointer = image.base() + tls.directory_rva as usize + TLS64_ADDRESS_OF_INDEX;
    // SAFETY: the mapped image's TLS directory, whose index pointer the OS
    // relocated; it points at the image's own index.
    let index = unsafe { (index_pointer as *const *const u32).read_unaligned().read() };
    println!("index_in_image={index}");

    let resolver = TestResolver::default();
    let report = image
        .bind_imports(&resolver, UnimplementedPolicy::ReturnZero)
        .unwrap();
    for (import, by) in resolver.answers.borrow().iter() {
        println!("import={import},{by}");
    }
    println!("missing={}", report.missing.len());

    // SAFETY: the image's imports are bound, and it ships no DLLs.
    let main = unsafe { Arc::new(image).spawn_main(Vec::new()) }.unwrap();
    let code = main.join().unwrap().unwrap();
    println!("exit_code={code}");
    // SAFETY: ends this process as a title does, through the C runtime,
    // whose exit functions include the loader's detach of the title.
    unsafe { exit(code as i32) }
}

/// What a child process left behind.
#[derive(Debug)]
struct Run {
    /// Its exit code.
    code: Option<i32>,
    /// What the child printed about the loader.
    printed: String,
    /// What the stand-in executable wrote.
    log: String,
    /// The child's own complaints.
    #[expect(dead_code, reason = "read through Debug, in failure messages")]
    stderr: String,
}

impl Run {
    fn new(kind: &str) -> Run {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("title.log");
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "title_child", "--nocapture", "--test-threads=1"])
            .env("VICTORY_EXE_CHILD", kind)
            .env("VICTORY_EXE_CHILD_DIR", dir.path())
            .env("VICTORY_FIXTURE_LOG", &log)
            .output()
            .unwrap();
        Run {
            code: out.status.code(),
            printed: String::from_utf8_lossy(&out.stdout).into_owned(),
            log: std::fs::read_to_string(&log).unwrap_or_default(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// The title run as it is.
    fn plain() -> &'static Run {
        static RUN: OnceLock<Run> = OnceLock::new();
        RUN.get_or_init(|| Run::new("plain"))
    }

    /// The title run with a header that asks for a 64 KiB stack.
    fn small_stack() -> &'static Run {
        static RUN: OnceLock<Run> = OnceLock::new();
        RUN.get_or_init(|| Run::new("small-stack"))
    }
}

/// Every value of the lines `name=value` in `text`, in order.
fn values<'a>(text: &'a str, name: &str) -> Vec<&'a str> {
    text.lines()
        .filter_map(|line| line.trim_end().split_once('='))
        .filter(|(key, _)| *key == name)
        .map(|(_, value)| value)
        .collect()
}

/// The number of the one line `name=number` in `text`; `None` if there is
/// no such line, more than one, or no number in it.
fn number(text: &str, name: &str) -> Option<u64> {
    match values(text, name)[..] {
        [value] => value.parse().ok(),
        _ => None,
    }
}

/// The TLS callback's calls the stand-in logged: (reason, whether the third
/// argument was not null, thread id).
fn callbacks(log: &str) -> Vec<(u64, u64, u64)> {
    values(log, "callback")
        .iter()
        .map(|value| {
            let fields: Vec<u64> = value.split(',').map(|f| f.parse().unwrap()).collect();
            (fields[0], fields[1], fields[2])
        })
        .collect()
}

#[test]
fn exe_fixture_is_not_a_dll_and_has_tls() {
    let original = exe();
    let info = ImageInfo::parse(&original).unwrap();
    assert!(
        !info.is_dll(),
        "characteristics {:#x}",
        info.characteristics
    );
    assert_ne!(info.entry_rva, 0);
    assert_eq!(info.stack_reserve, STACK_RESERVE);
    assert!(
        info.has_relocations,
        "mapped as a DLL, it will not be at its base"
    );
    let tls = info.tls.as_ref().expect("a TLS directory");
    assert_eq!(tls.callback_rvas.len(), 1, "{tls:?}");
    let imports: Vec<_> = info
        .imports
        .iter()
        .flat_map(|m| {
            let dll = m.dll.to_ascii_lowercase();
            m.entries.iter().map(move |e| format!("{dll}!{}", e.symbol))
        })
        .collect();
    for wanted in [
        "xgameruntime.dll!XGameRuntimeInitialize",
        "kernel32.dll!CreateThread",
        "kernel32.dll!GetCurrentThreadStackLimits",
        // What it loads a DLL of its own with, and reads its package with.
        "kernel32.dll!LoadLibraryW",
        "kernel32.dll!GetProcAddress",
        "kernel32.dll!ReadFile",
        // What it asks where it runs with.
        "kernel32.dll!GetCurrentDirectoryW",
    ] {
        assert!(imports.iter().any(|i| i == wanted), "{wanted}: {imports:?}");
    }
    // The point of the fixture: its code reads the TLS array in the form
    // the redirect rewrites, whatever toolchain built it. Without one such
    // load, the tests of the redirect below would have nothing to see.
    let loads = tls_array_loads(&original, &info);
    assert!(
        !loads.is_empty(),
        "no `mov r64, gs:[0x58]` in the fixture's code"
    );
}

#[test]
fn redirect_patches_the_exe_tls_loads() {
    let original = exe();
    let info = ImageInfo::parse(&original).unwrap();
    let loads = tls_array_loads(&original, &info).len() as u64;
    assert_ne!(loads, 0, "the fixture has loads to redirect");

    let run = Run::plain();
    let printed = |name: &str| number(&run.printed, name);
    // The loader says it rewrote every load the file has, and the mapped
    // code agrees.
    assert_eq!(printed("redirected"), Some(loads), "{run:#?}");
    assert_eq!(printed("calls_in_memory"), Some(loads), "{run:#?}");
    assert_eq!(printed("index_in_image"), Some(0), "{run:#?}");

    // What the title read through those loads: on each thread a copy of
    // its own template, and what the thread itself wrote there afterwards.
    let seen = |name: &str| number(&run.log, name);
    assert_eq!(seen("main_initial"), Some(TEMPLATE_VALUE), "{run:#?}");
    assert_eq!(seen("main_guard"), Some(GUARD), "{run:#?}");
    assert_eq!(seen("main_last"), Some(TEMPLATE_VALUE + MAIN_ADDS));
    for (thread, adds) in ["a", "b"].iter().zip(WORKER_ADDS) {
        let seen = |name: &str| seen(&format!("{thread}_{name}"));
        assert_eq!(seen("initial"), Some(TEMPLATE_VALUE), "{thread}: {run:#?}");
        assert_eq!(seen("guard"), Some(GUARD), "{thread}: {run:#?}");
        assert_eq!(seen("last"), Some(TEMPLATE_VALUE + adds), "{thread}");
    }
    // Three threads alive at the same time, three blocks.
    let blocks = [seen("main_block"), seen("a_block"), seen("b_block")];
    assert!(blocks.iter().all(|b| b.is_some_and(|b| b != 0)), "{run:#?}");
    assert_ne!(blocks[0], blocks[1], "{run:#?}");
    assert_ne!(blocks[0], blocks[2], "{run:#?}");
    assert_ne!(blocks[1], blocks[2], "{run:#?}");
}

#[test]
fn bind_gives_the_exe_its_console_import_and_the_host_the_rest() {
    let run = Run::plain();
    let imports = values(&run.printed, "import");
    assert!(
        imports.contains(&"xgameruntime.dll!XGameRuntimeInitialize,hle"),
        "{run:#?}"
    );
    for host in [
        "kernel32.dll!CreateThread,host",
        "kernel32.dll!LoadLibraryW,host",
        "kernel32.dll!GetProcAddress,host",
        "kernel32.dll!ReadFile,host",
    ] {
        assert!(imports.contains(&host), "{host}: {run:#?}");
    }
    let missing: Vec<_> = imports.iter().filter(|i| i.ends_with(",missing")).collect();
    assert!(missing.is_empty(), "{missing:?}");
    assert_eq!(number(&run.printed, "missing"), Some(0), "{run:#?}");
    // The title called it and got the resolver's answer.
    assert_eq!(number(&run.log, "runtime"), Some(RUNTIME as u64));
}

/// The stand-in loads a DLL it ships and copies a file of its package to
/// its temporary drive (`victory run`'s tests, apps/victory/tests/run.rs).
/// Here the host answers those calls and has neither the DLL nor the
/// package: the title is told so, writes nothing, and goes on to its end.
#[test]
fn the_exe_goes_on_without_its_late_dll_and_its_package() {
    let run = Run::plain();
    let seen = |name: &str| number(&run.log, name);
    assert_eq!(seen("late_dll"), Some(0), "{run:#?}");
    assert_eq!(seen("late_chain"), Some(0), "{run:#?}");
    assert_eq!(seen("package_found"), Some(0), "{run:#?}");
    assert_eq!(seen("temp_written"), Some(0), "{run:#?}");
    assert_eq!(seen("temp_bytes"), Some(0), "{run:#?}");
    // It was told a current directory, and the file it looks for there by
    // name alone is not in it: nothing is written by a relative path.
    assert!(seen("cwd_units").is_some_and(|n| n > 0), "{run:#?}");
    assert_eq!(seen("relative_found"), Some(0), "{run:#?}");
    assert_eq!(seen("relative_written"), Some(0), "{run:#?}");
    let expected = EXIT_BASE + RUNTIME as u64;
    assert_eq!(run.code, Some(expected as i32), "{run:#?}");
}

#[test]
fn spawn_main_returns_the_entry_exit_code() {
    let run = Run::plain();
    let expected = EXIT_BASE + RUNTIME as u64;
    assert_eq!(
        number(&run.printed, "exit_code"),
        Some(expected),
        "{run:#?}"
    );
    // And the child passed it to `exit`, so it is the process's too.
    assert_eq!(run.code, Some(expected as i32), "{run:#?}");
}

#[test]
fn spawn_main_honours_the_stack_reserve() {
    let stack = |run: &Run| {
        let low = number(&run.log, "stack_low")?;
        number(&run.log, "stack_high")?.checked_sub(low)
    };
    let run = Run::plain();
    let seen = stack(run);
    assert!(
        seen.is_some_and(|s| s >= STACK_RESERVE),
        "{seen:?}: {run:#?}"
    );

    // A header that asks for less than 1 MiB gets 1 MiB, not the 6 MiB the
    // file says: the loader goes by the header values it was handed.
    let run = Run::small_stack();
    let seen = stack(run);
    assert!(
        seen.is_some_and(|s| (SMALLEST_STACK..STACK_RESERVE).contains(&s)),
        "{seen:?}: {run:#?}"
    );
    let expected = EXIT_BASE + RUNTIME as u64;
    assert_eq!(run.code, Some(expected as i32), "{run:#?}");
}

#[test]
fn the_exe_tls_callback_hears_of_start_threads_and_exit() {
    let run = Run::plain();
    let calls = callbacks(&run.log);
    let seen = |name: &str| number(&run.log, name);
    let (Some(main), Some(a), Some(b)) = (seen("main_thread"), seen("a_thread"), seen("b_thread"))
    else {
        panic!("the title did not say which threads it ran on: {run:#?}");
    };
    assert_eq!(seen("callback_count"), Some(calls.len() as u64), "{run:#?}");

    // First the start of the process, on the thread the entry point then
    // ran on, with a null third argument.
    assert_eq!(calls.first(), Some(&(DLL_PROCESS_ATTACH, 0, main)));
    // Each thread the title started, and its end; the main thread's end.
    for thread in [a, b] {
        assert!(calls.contains(&(DLL_THREAD_ATTACH, 0, thread)), "{calls:?}");
        assert!(calls.contains(&(DLL_THREAD_DETACH, 0, thread)), "{calls:?}");
    }
    assert!(calls.contains(&(DLL_THREAD_DETACH, 0, main)), "{calls:?}");
    // Last, once, the end of the process: the third argument says that it
    // is ending, as `ExitProcess` says it.
    let detaches: Vec<_> = calls
        .iter()
        .filter(|(reason, ..)| *reason == DLL_PROCESS_DETACH)
        .collect();
    assert_eq!(detaches.len(), 1, "{calls:?}");
    assert_eq!(calls.last().map(|c| (c.0, c.1)), Some((0, 1)), "{calls:?}");
    // The title wrote its log to the end from that call.
    assert_eq!(run.log.lines().last(), Some("detached=1"), "{run:#?}");
}

#[test]
fn spawn_main_without_entry_is_an_error() {
    let mut prepared = prepare(&exe()).unwrap();
    prepared.info.entry_rva = 0;
    let dir = tempfile::tempdir().unwrap();
    let image = Arc::new(victory_loader::load(&prepared, dir.path()).unwrap());
    // SAFETY: nothing is started: there is no entry point to run.
    let started = unsafe { image.clone().spawn_main(Vec::new()) };
    assert_eq!(
        started.map(|_| ()).map_err(|e| e.to_string()),
        Err("image has no entry point".to_owned())
    );
    let image = Arc::into_inner(image).expect("no thread holds the image");
    // SAFETY: none of its code ran or will.
    unsafe { image.unload() };
}
