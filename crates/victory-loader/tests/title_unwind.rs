//! Unwinding, stack walks and crash reports through title code that the
//! Windows loader mapped as a prepared image and moved away from the base
//! it was linked for. The title is tests/fixtures/title-unwind, a DLL
//! written by us that panics and catches the panic, calls back, faults and
//! throws.
//!
//! All of it rests on what the loader's first step promises: the prepared
//! copy is a real module, so Windows relocates it and registers its unwind
//! tables at the address it ended up at.
#![cfg(windows)]

use std::ffi::{CString, c_void};
use std::path::Path;
use std::sync::OnceLock;

use victory_loader::{
    ImportResolver, LoadedImage, Provenance, Resolution, UnimplementedPolicy, locate, prepare,
};
use victory_pe::ImportSymbol;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Diagnostics::Debug::{
    RtlCaptureStackBackTrace, RtlLookupFunctionEntry,
};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
use windows_sys::Win32::System::Memory::{
    MEM_FREE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_NOACCESS, VirtualAlloc, VirtualQuery,
};
use windows_sys::Win32::System::Threading::{CreateThread, INFINITE, WaitForSingleObject};

mod common;
use common::fixture;

/// What tests/fixtures/title-unwind answers with.
const TARGET: u32 = 0x5EED_0001;
const CALLED: u32 = 0xCA11_BAC0;
const THROWN_INT: u32 = 0x8007_0057;

/// Every import from the host: the fixture is an ordinary Rust DLL.
struct Host;

impl ImportResolver for Host {
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

/// Why the image could not be mapped at the base it was linked for.
#[derive(Debug)]
enum BaseTaken {
    /// This test reserved the range before the image was mapped.
    ReservedHere,
    /// Something was there already; what `VirtualQuery` said of its start.
    InUse { state: u32 },
}

/// The fixture as a prepared image: mapped, bound and attached.
struct Title {
    image: LoadedImage,
    /// The prepared copy's file name, which is what a module is known by.
    module: String,
    /// The base the fixture was linked for.
    preferred: usize,
    taken: BaseTaken,
}

impl Title {
    /// Mapped once for the whole test program, and never unmapped.
    fn get() -> &'static Title {
        static TITLE: OnceLock<Title> = OnceLock::new();
        TITLE.get_or_init(|| {
            let prepared = prepare(&std::fs::read(fixture("title_unwind.dll")).unwrap()).unwrap();
            let preferred = usize::try_from(prepared.info.image_base).unwrap();
            let size = prepared.info.size_of_image as usize;
            // The base it was linked for is taken before it is mapped, so
            // that Windows has to move it wherever it would have put it.
            // SAFETY: reserves address space and touches no memory.
            let reserved = unsafe {
                VirtualAlloc(preferred as *const c_void, size, MEM_RESERVE, PAGE_NOACCESS)
            };
            let taken = if reserved.is_null() {
                // SAFETY: a zeroed MEMORY_BASIC_INFORMATION is valid, and
                // the query reads the address space's bookkeeping only.
                let state = unsafe {
                    let mut info: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
                    VirtualQuery(preferred as *const c_void, &mut info, size_of_val(&info));
                    info.State
                };
                BaseTaken::InUse { state }
            } else {
                assert_eq!(reserved as usize, preferred);
                BaseTaken::ReservedHere
            };
            // The folder is kept: the image stays mapped from it.
            let cache = tempfile::tempdir().unwrap().keep();
            let image = victory_loader::load(&prepared, &cache).unwrap();
            let report = image
                .bind_imports(&Host, UnimplementedPolicy::Trap)
                .unwrap();
            assert_eq!(report.missing, [], "the host has all it imports");
            // Its C runtime's start, as for any DLL of a title.
            // SAFETY: its imports are bound and it depends on no title DLL.
            unsafe { image.attach() }.unwrap();
            Title {
                image,
                module: prepared.file_name(),
                preferred,
                taken,
            }
        })
    }

    fn export(&self, name: &str) -> usize {
        self.image
            .export(name)
            .unwrap_or_else(|| panic!("no export {name}"))
    }

    fn contains(&self, address: usize) -> bool {
        let base = self.image.base();
        (base..base + self.image.info().size_of_image as usize).contains(&address)
    }

    /// Whether `located` (what `locate` said of an address) is in this
    /// image; its offset there.
    fn offset_of(&self, located: &str) -> Option<usize> {
        let offset = located.strip_prefix(&self.module)?.strip_prefix("+0x")?;
        usize::from_str_radix(offset, 16).ok()
    }
}

#[test]
fn the_image_is_rebased_and_its_pointers_follow() {
    let title = Title::get();
    let base = title.image.base();
    assert!(title.image.info().has_relocations);
    // Its base was taken, by this test or by something before it; either
    // way Windows could not use it.
    if let BaseTaken::InUse { state } = title.taken {
        assert_ne!(state, MEM_FREE, "{:#x} was free", title.preferred);
    }
    assert_ne!(
        base, title.preferred,
        "mapped at its preferred base, which was taken: {:?}",
        title.taken
    );

    // SAFETY: the exports have the signatures declared in the fixture.
    let (pointer, target) = unsafe {
        let pointer: extern "system" fn() -> *const *const u32 =
            std::mem::transmute(title.export("unwind_pointer"));
        let target: extern "system" fn() -> *const u32 =
            std::mem::transmute(title.export("unwind_target"));
        (pointer(), target())
    };
    assert!(title.contains(pointer as usize), "{pointer:?}");
    assert!(title.contains(target as usize), "{target:?}");
    // SAFETY: both are constants of the mapped image.
    let (stored, value) = unsafe { (pointer.read(), target.read()) };
    // An address kept as data was moved with the image: it is where the
    // code finds the constant, not where the linker had put it.
    assert_eq!(stored, target);
    assert_ne!(
        stored as usize,
        title.preferred + (target as usize - base),
        "the address the file holds"
    );
    assert_eq!(value, TARGET);
}

#[test]
fn a_panic_unwinds_and_is_caught_inside_the_prepared_image() {
    let title = Title::get();
    // SAFETY: the exports have the signatures declared in the fixture.
    let (strategy, catch) = unsafe {
        let strategy: extern "system" fn() -> u32 =
            std::mem::transmute(title.export("unwind_strategy"));
        let catch: extern "system" fn(u32, u32) -> u64 =
            std::mem::transmute(title.export("unwind_catch"));
        (strategy, catch)
    };
    // Built to abort, its panic would end this process instead.
    assert_eq!(strategy(), 1, "title_unwind.dll was not built to unwind");
    // The panic went up through every frame (each one's destructor ran)
    // to the handler, which got what was thrown.
    for (depth, payload) in [(0, 0xC0DE_0001_u32), (5, 0xC0DE_0002), (40, 0xC0DE_0003)] {
        let caught = catch(depth, payload);
        assert_eq!(
            (caught >> 32, caught as u32),
            (u64::from(depth) + 1, payload),
            "depth {depth}"
        );
    }
    // On a thread that started in the test, too.
    let caught = std::thread::spawn(move || catch(3, 0xC0DE_0004))
        .join()
        .unwrap();
    assert_eq!((caught >> 32, caught as u32), (4, 0xC0DE_0004));
}

/// Called by the title: the return addresses on this thread's stack, as
/// the system walks it.
extern "system-unwind" fn capture(frames: *mut c_void) {
    // SAFETY: the test passes its vector, and waits for the call.
    let frames = unsafe { &mut *frames.cast::<Vec<usize>>() };
    let mut walked = [std::ptr::null_mut(); 62];
    // SAFETY: the buffer has room for the frames asked for.
    let n = unsafe {
        RtlCaptureStackBackTrace(
            0,
            walked.len() as u32,
            walked.as_mut_ptr(),
            std::ptr::null_mut(),
        )
    };
    frames.extend(walked[..usize::from(n)].iter().map(|f| *f as usize));
}

#[test]
fn a_frame_in_title_code_is_named_by_locate() {
    let title = Title::get();
    let export = title.export("unwind_callback");
    type Callback = extern "system-unwind" fn(*mut c_void);
    // SAFETY: the export has the signature declared in the fixture.
    let call: extern "system-unwind" fn(Callback, *mut c_void) -> u32 =
        unsafe { std::mem::transmute(export) };
    let mut frames: Vec<usize> = Vec::new();
    assert_eq!(call(capture, (&raw mut frames).cast()), CALLED);

    let located: Vec<String> = frames.iter().map(|f| locate(*f)).collect();
    // One frame is the title's, named by the prepared copy's file and the
    // offset in it.
    let in_title: Vec<usize> = (0..frames.len())
        .filter(|i| title.offset_of(&located[*i]).is_some())
        .collect();
    let &[at] = &in_title[..] else {
        panic!("frames in {}: {in_title:?} of {located:#?}", title.module);
    };
    assert_eq!(
        title.offset_of(&located[at]),
        Some(frames[at] - title.image.base())
    );
    // It is the function that called back: the unwind tables Windows has
    // for that address are this image's, at the base it was mapped at.
    let mut image = 0;
    // SAFETY: looks up the function table entry of an address.
    let entry =
        unsafe { RtlLookupFunctionEntry(frames[at] as u64, &mut image, std::ptr::null_mut()) };
    assert!(!entry.is_null(), "no unwind data for {}", located[at]);
    assert_eq!(image as usize, title.image.base());
    // SAFETY: an entry starts with the offset of its function's start.
    let begin = unsafe { entry.cast::<u32>().read() } as usize;
    assert_eq!(title.image.base() + begin, export, "{}", located[at]);
    // The walk got through it, to this test.
    let here = locate(a_frame_in_title_code_is_named_by_locate as fn() as usize);
    let test_program = here.rsplit_once('+').unwrap().0;
    assert!(
        located[at + 1..].iter().any(|l| l
            .rsplit_once('+')
            .is_some_and(|(module, _)| module == test_program)),
        "no frame of {test_program} above the title's: {located:#?}"
    );
}

/// In a child process of the tests below: the crash handlers as the
/// runtime installs them, then what `VICTORY_CRASH_CHILD` names.
#[test]
fn crash_child() {
    let (Some(what), Some(log)) = (
        std::env::var_os("VICTORY_CRASH_CHILD"),
        std::env::var_os("VICTORY_CRASH_CHILD_LOG"),
    ) else {
        return;
    };
    // The harness has written "test crash_child ... " without ending the
    // line; what is printed here starts on lines of its own.
    println!();
    victory_loader::log_crashes_to(log.into());
    victory_loader::report_crashes();
    victory_loader::note_title_output("boom reason");
    let (export, argument) = match what.to_str().unwrap() {
        "panic" => panic!("boom in our code"),
        "fault" => ("unwind_fault", 0usize),
        "throw-int" => ("unwind_throw", 0),
        "throw-exception" => ("unwind_throw", 1),
        other => panic!("VICTORY_CRASH_CHILD={other}"),
    };
    let title = Title::get();
    // SAFETY: the export has the signature declared in the fixture.
    let target = unsafe {
        let target: extern "system" fn() -> *const u32 =
            std::mem::transmute(title.export("unwind_target"));
        target()
    };
    println!("module={}", title.module);
    println!("base={:#x}", title.image.base());
    println!("target={:#x}", target as usize);
    println!("starting");
    // A thread that starts in title code, as a title's own do: between the
    // system's thread start and the title there is no frame of ours (and
    // none of the test harness, which catches what it can).
    // SAFETY: the export is a thread start routine; see the fixture.
    unsafe {
        let start: unsafe extern "system" fn(*mut c_void) -> u32 =
            std::mem::transmute(title.export(export));
        let thread = CreateThread(
            std::ptr::null(),
            0,
            Some(start),
            argument as *const c_void,
            0,
            std::ptr::null_mut(),
        );
        assert!(!thread.is_null());
        WaitForSingleObject(thread, INFINITE);
        CloseHandle(thread);
    }
    println!("returned");
}

/// What a child process left behind.
#[derive(Debug)]
struct Crash {
    code: Option<i32>,
    printed: String,
    stderr: String,
    /// The crash log.
    log: String,
}

impl Crash {
    fn of(what: &str) -> Crash {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("crashes.log");
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crash_child", "--nocapture"])
            .env("VICTORY_CRASH_CHILD", what)
            .env("VICTORY_CRASH_CHILD_LOG", &log)
            .output()
            .unwrap();
        Crash {
            code: out.status.code(),
            printed: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            log: read_log(&log),
        }
    }

    /// What the child printed as `name=value`.
    fn said(&self, name: &str) -> &str {
        let mut lines = self.printed.lines();
        lines
            .find_map(|l| l.trim_end().strip_prefix(name)?.strip_prefix('='))
            .unwrap_or_else(|| panic!("the child printed no {name}: {self:#?}"))
    }

    fn address(&self, name: &str) -> usize {
        let value = self.said(name).strip_prefix("0x").unwrap();
        usize::from_str_radix(value, 16).unwrap()
    }

    /// The crash log's lines.
    fn lines(&self) -> Vec<&str> {
        self.log.lines().map(str::trim_end).collect()
    }

    /// The frames of the report that follows the log line `headline`:
    /// what comes after each `#n`.
    fn frames_after(&self, headline: usize) -> Vec<&str> {
        let lines = self.lines();
        let frames = lines[headline + 1..]
            .iter()
            .skip_while(|l| !l.starts_with("  #"))
            .map_while(|l| l.strip_prefix("  #"))
            .enumerate();
        frames
            .map(|(i, l)| {
                let rest = l.strip_prefix(&i.to_string());
                rest.unwrap_or_else(|| panic!("frame {i} is {l:?}: {self:#?}"))
                    .trim_start()
            })
            .collect()
    }

    /// The title ran until it crashed, and the process ended with it.
    fn assert_ended_in_the_title(&self) {
        assert!(self.printed.contains("\nstarting"), "{self:#?}");
        assert!(!self.printed.contains("returned"), "{self:#?}");
        assert_ne!(self.code, Some(0), "{self:#?}");
    }

    /// Where the one log line that starts with `prefix` is.
    fn line_starting(&self, prefix: &str) -> usize {
        let lines = self.lines();
        let found: Vec<_> = (0..lines.len())
            .filter(|i| lines[*i].starts_with(prefix))
            .collect();
        match found[..] {
            [at] => at,
            _ => panic!("{} lines start with {prefix:?}: {self:#?}", found.len()),
        }
    }
}

fn read_log(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// A write to a constant of the title's, in title code on a thread of the
/// title's: the crash log names the module and the offset of the store,
/// what was written to, the frames, and what the title last said.
#[test]
fn a_fault_in_title_code_is_reported() {
    let crash = Crash::of("fault");
    crash.assert_ended_in_the_title();
    let module = crash.said("module");
    let target = crash.address("target");
    let target_offset = target - crash.address("base");

    let headline = format!("VIctory: unhandled exception 0xc0000005 at {module}+0x");
    let at = crash.line_starting(&headline);
    let lines = crash.lines();
    let detail = format!(" (write {target:#x}: read-only, in {module}+{target_offset:#x})");
    assert!(lines[at].ends_with(&detail), "{detail}: {crash:#?}");
    // The store and the function that called it are the title's; the
    // first frame is the faulting instruction itself.
    let frames = crash.frames_after(at);
    let faulting = lines[at][..lines[at].len() - detail.len()]
        .rsplit_once(" at ")
        .unwrap()
        .1;
    assert_eq!(frames.first().copied(), Some(faulting), "{crash:#?}");
    let in_title = |frame: &&str| frame.starts_with(&format!("{module}+0x"));
    assert!(
        frames.len() > 2 && frames[..2].iter().all(in_title),
        "{crash:#?}"
    );
    // Below them, the system's thread start; no frame is lost in between.
    assert!(!in_title(&frames[2]), "{crash:#?}");
    assert!(lines.contains(&"  title said: boom reason"), "{crash:#?}");
    // It was printed as it happened, too.
    let first_chance = format!("VIctory: first-chance exception 0xc0000005 at {module}+0x");
    assert!(crash.stderr.contains(&first_chance), "{crash:#?}");
    assert!(crash.stderr.contains(&headline), "{crash:#?}");
}

/// A C++ `throw` nobody catches, as MSVC's runtime raises it, with the
/// tables in the title's image: the report decodes the type and the value.
#[test]
fn an_uncaught_cpp_throw_from_title_code_is_decoded() {
    for (what, thrown) in [
        ("throw-int", format!(".H = {THROWN_INT:#010x}")),
        (
            "throw-exception",
            ".?AVruntime_error@std@@: boom thrown".to_owned(),
        ),
    ] {
        let crash = Crash::of(what);
        crash.assert_ended_in_the_title();
        let module = crash.said("module");
        let at = crash.line_starting("VIctory: unhandled exception 0xe06d7363 at ");
        let lines = crash.lines();
        let detail = format!(" (C++ {thrown} thrown)");
        assert!(lines[at].ends_with(&detail), "{what}: {crash:#?}");
        // Raised inside the system, called from the title.
        let frames = crash.frames_after(at);
        let in_title: Vec<_> = frames
            .iter()
            .filter(|f| f.starts_with(&format!("{module}+0x")))
            .collect();
        assert!(!in_title.is_empty(), "{what}: {crash:#?}");
        assert!(!frames[0].starts_with(module), "{what}: {crash:#?}");
        assert!(
            lines.contains(&"  title said: boom reason"),
            "{what}: {crash:#?}"
        );
    }
}

/// A panic of ours, on any thread, is in the crash log with what the title
/// last said.
#[test]
fn a_panic_in_our_code_reaches_the_crash_log() {
    let crash = Crash::of("panic");
    assert_ne!(crash.code, Some(0), "{crash:#?}");
    let at = crash.line_starting("VIctory: panic on thread ");
    let lines = crash.lines();
    assert!(lines[at + 1..].contains(&"boom in our code"), "{crash:#?}");
    assert!(lines.contains(&"  title said: boom reason"), "{crash:#?}");
}
