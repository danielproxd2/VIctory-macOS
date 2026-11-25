//! The end of the process: a title calls `exit`, and the loader tells the
//! title's images, DLLs in reverse order of their initialisation and then
//! the executable, before the process ends; if one of them does not come
//! back, the process ends anyway.
//!
//! The title runs in a child process, as in load_exe.rs. It is the
//! stand-in executable (tests/fixtures/hle-probe-exe) with two DLLs of a
//! title beside it (title-a and title-b), which all write what they are
//! told to the file `VICTORY_FIXTURE_LOG` names.
#![cfg(windows)]

use std::ffi::CString;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use victory_loader::{
    ImportResolver, Provenance, Resolution, UnimplementedPolicy, WithTitleDlls, prepare,
};
use victory_pe::ImportSymbol;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

mod common;
use common::fixture;

/// What the stand-in executable returns from its entry point: 7 plus what
/// `XGameRuntimeInitialize` answered (the constants at the top of its
/// main.rs), which here is this.
const RUNTIME: i32 = 70;
const EXIT_CODE: i32 = 7 + RUNTIME;

/// How long the loader gives the title's images to hear of the end of the
/// process (`DETACH_DEADLINE` in the loader's threads.rs), and the exit
/// code of a process it ended because they took longer.
const DETACH_DEADLINE: Duration = Duration::from_secs(5);
const ENDED_BY_THE_LOADER: i32 = 1;

unsafe extern "C" {
    /// The C runtime's `exit`, which is how a title ends the process.
    fn exit(code: i32) -> !;
}

extern "system" fn fake_xgameruntime_initialize() -> i32 {
    RUNTIME
}

/// The one console function from here, everything else from the host.
struct Runtime;

impl ImportResolver for Runtime {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        let ImportSymbol::Name { name, .. } = symbol else {
            return Resolution::Missing;
        };
        if dll.eq_ignore_ascii_case("xgameruntime.dll") && name == "XGameRuntimeInitialize" {
            return Resolution::Found {
                address: fake_xgameruntime_initialize as *const () as usize,
                provenance: Provenance::Hle,
            };
        }
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

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

/// In a child process of the tests below: maps the stand-in executable and
/// the two DLLs, runs the executable to the end of its entry point with
/// the DLLs attached, and ends the process as a title does.
#[test]
fn detach_child() {
    let Some(dir) = std::env::var_os("VICTORY_DETACH_CHILD_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    // The harness has written "test detach_child ... " without ending the
    // line; what is printed here starts on lines of its own.
    println!();
    let (title, cache) = (dir.join("title"), dir.join("cache"));
    std::fs::create_dir(&title).unwrap();
    for file in ["title_a.dll", "title_b.dll"] {
        std::fs::copy(fixture(file), title.join(file)).unwrap();
    }
    let original = std::fs::read(fixture("hle-probe-exe.exe")).unwrap();
    let image = victory_loader::load(&prepare(&original).unwrap(), &cache).unwrap();
    // The executable does not import them; a title's own would.
    let dlls = victory_loader::load_title_dll("title_b.dll", &title, &cache, &|_| true).unwrap();
    let names: Vec<_> = dlls.iter().map(|(name, _)| name.as_str()).collect();
    println!("attach_order={}", names.join(","));

    let resolver = WithTitleDlls {
        inner: &Runtime,
        dlls: &dlls,
    };
    // title_b.dll delay-loads a DLL this folder does not have, and never
    // calls it here.
    for (_, dll) in &dlls {
        dll.bind_imports(&resolver, UnimplementedPolicy::ReturnZero)
            .unwrap();
    }
    let report = image
        .bind_imports(&resolver, UnimplementedPolicy::ReturnZero)
        .unwrap();
    println!("missing={}", report.missing.len());

    // SAFETY: every image's imports are bound, and the DLLs are given in
    // the order they depend on each other.
    let main = unsafe { Arc::new(image).spawn_main(dlls.clone()) }.unwrap();
    let code = main.join().unwrap().unwrap();
    println!("exit_code={code}");
    println!("exiting_at={}", now_ms());
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
    /// What the title's images wrote, line by line.
    log: Vec<String>,
    /// From the child's call of `exit` to the end of the process.
    exit_took: Option<Duration>,
    /// The child's own complaints.
    #[expect(dead_code, reason = "read through Debug, in failure messages")]
    stderr: String,
}

impl Run {
    fn new(stuck: bool) -> Run {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("title.log");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", "detach_child", "--nocapture", "--test-threads=1"])
            .env("VICTORY_DETACH_CHILD_DIR", dir.path())
            .env("VICTORY_FIXTURE_LOG", &log);
        if stuck {
            child.env("VICTORY_FIXTURE_STUCK", "1");
        }
        let out = child.output().unwrap();
        let ended = now_ms();
        let printed = String::from_utf8_lossy(&out.stdout).into_owned();
        let exiting_at = printed
            .lines()
            .find_map(|l| l.trim_end().strip_prefix("exiting_at="))
            .and_then(|at| at.parse::<u128>().ok());
        let log = std::fs::read_to_string(&log).unwrap_or_default();
        Run {
            code: out.status.code(),
            exit_took: exiting_at
                .map(|at| Duration::from_millis(ended.saturating_sub(at).try_into().unwrap())),
            printed,
            log: log.lines().map(|l| l.trim_end().to_owned()).collect(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// The title run to its end.
    fn plain() -> &'static Run {
        static RUN: OnceLock<Run> = OnceLock::new();
        RUN.get_or_init(|| Run::new(false))
    }

    /// Where in the log each line equal to `line` is.
    fn at(&self, line: &str) -> Vec<usize> {
        let found = self.log.iter().enumerate().filter(|(_, l)| *l == line);
        found.map(|(i, _)| i).collect()
    }

    /// Where the one line equal to `line` is.
    fn once(&self, line: &str) -> usize {
        match self.at(line)[..] {
            [at] => at,
            ref other => panic!("{line:?} is in the log {} times: {self:#?}", other.len()),
        }
    }

    /// The executable's TLS callback calls, as it wrote them at its end:
    /// (reason, whether the third argument was not null, line).
    fn exe_callbacks(&self) -> Vec<(u64, u64, usize)> {
        let lines = self.log.iter().enumerate();
        lines
            .filter_map(|(i, l)| Some((i, l.strip_prefix("callback=")?)))
            .map(|(i, value)| {
                let fields: Vec<u64> = value.split(',').map(|f| f.parse().unwrap()).collect();
                (fields[0], fields[1], i)
            })
            .collect()
    }
}

#[test]
fn exit_detaches_dlls_in_reverse_then_the_exe() {
    let run = Run::plain();
    assert!(
        run.printed
            .contains("\nattach_order=title_a.dll,title_b.dll"),
        "{run:#?}"
    );
    assert!(run.printed.contains("\nmissing=0"), "{run:#?}");
    assert_eq!(run.code, Some(EXIT_CODE), "{run:#?}");

    // Initialised in the order they depend on each other, each once.
    let (a_start, b_start) = (run.once("title_a 1"), run.once("title_b 1"));
    assert!(a_start < b_start, "{run:#?}");
    // Told of the end in the reverse order, each once: title_b.dll may
    // still call into title_a.dll while it cleans up.
    let (b_end, a_end) = (run.once("title_b 0"), run.once("title_a 0"));
    assert!(b_start < b_end && b_end < a_end, "{run:#?}");

    // Then the executable, whose TLS callback is told once that the
    // process ends, with the third argument that says so; it writes what
    // it was called with from that call, so its lines come last.
    let callbacks = run.exe_callbacks();
    let ends: Vec<_> = callbacks.iter().filter(|c| c.0 == 0).collect();
    assert_eq!(ends.len(), 1, "{run:#?}");
    assert_eq!(callbacks.last().map(|c| (c.0, c.1)), Some((0, 1)));
    assert!(callbacks.iter().all(|c| a_end < c.2), "{run:#?}");
    // The executable's start was its first notice, with a null argument.
    assert_eq!(callbacks.first().map(|c| (c.0, c.1)), Some((1, 0)));
    assert_eq!(run.log.last().map(String::as_str), Some("detached=1"));

    // Nothing kept the process: it ended well inside the deadline.
    let took = run.exit_took.expect("the child said when it called exit");
    assert!(took < DETACH_DEADLINE, "{took:?}: {run:#?}");
}

/// Once the process is ending, the title's images hear of no more threads:
/// not of the loader's own watchdog thread, which starts then, and not of
/// the threads that are ended. What is seen here is the order of the log;
/// it does not tell the loader's two reasons for it apart (the images are
/// no longer attached, and notices are off while the process ends).
#[test]
fn thread_notices_after_exit_started_are_ignored() {
    let run = Run::plain();
    let thread_notice = |line: &str| {
        ["title_a ", "title_b "]
            .iter()
            .any(|dll| line.strip_prefix(dll).is_some_and(|r| r == "2" || r == "3"))
    };
    // The fixture does log thread notices: the executable started two
    // threads and let them end, and both DLLs heard of each.
    for line in ["title_a 2", "title_b 2", "title_a 3", "title_b 3"] {
        assert!(run.at(line).len() >= 2, "{line}: {run:#?}");
    }
    let exit_started = run.once("title_b 0");
    let late: Vec<_> = run.log[exit_started..]
        .iter()
        .filter(|l| thread_notice(l))
        .collect();
    assert_eq!(late, [""; 0], "{run:#?}");
    // Before that, each start reached the DLLs in the order they were
    // initialised: title_a.dll first.
    let first = run.log.iter().position(|l| thread_notice(l)).unwrap();
    assert_eq!(run.log[first], "title_a 2", "{run:#?}");
}

/// A DLL that never comes back from the notice of the process's end (it
/// waits for a lock that a thread held when it was ended) does not keep the
/// process: the loader ends it at the deadline.
#[test]
fn stuck_detach_ends_the_process_at_the_deadline() {
    let run = Run::new(true);
    assert!(run.printed.contains("\nexit_code=77"), "{run:#?}");
    // title_b.dll was told and came back; title_a.dll was told and did not;
    // the executable was never told.
    let (b_end, a_end) = (run.once("title_b 0"), run.once("title_a 0"));
    assert!(b_end < a_end, "{run:#?}");
    assert_eq!(run.at("detached=1"), [0usize; 0], "{run:#?}");
    assert!(run.exe_callbacks().is_empty(), "{run:#?}");

    // Not the title's exit code: the loader ended the process.
    assert_eq!(run.code, Some(ENDED_BY_THE_LOADER), "{run:#?}");
    let took = run.exit_took.expect("the child said when it called exit");
    // At the deadline, not before; and not long after, with room for a
    // machine that is busy with other tests.
    assert!(
        took >= DETACH_DEADLINE - Duration::from_millis(250),
        "{took:?}"
    );
    assert!(took <= DETACH_DEADLINE + Duration::from_secs(5), "{took:?}");
}
