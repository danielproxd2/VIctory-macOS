//! `victory run` as a user starts it: the binary, its standard output and
//! its exit status. The title is the stand-in executable of the fixtures
//! workspace (tests/fixtures/hle-probe-exe), written by us; it needs the
//! Windows loader, so it runs on Windows and under Wine. What `run` does
//! on another host, and without a title, is tested on every host.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[cfg(windows)]
#[path = "../../../crates/victory-loader/tests/common/mod.rs"]
mod common;

/// The folders of one test: the title's own (`title`, the package the
/// title sees as `G:`), the two `victory` writes to (`cache`, `data`; made
/// on first use), and an empty one to start it in (`cwd`: no victory.toml).
struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Home {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["title", "cwd"] {
            std::fs::create_dir(dir.path().join(sub)).unwrap();
        }
        Home { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// Where the stand-in title writes what it saw.
    fn title_log(&self) -> PathBuf {
        self.path("title.log")
    }

    /// Runs `victory` with nothing configured but this test's folders.
    fn victory(&self, args: &[&std::ffi::OsStr]) -> Run {
        self.victory_with_data(&self.path("data"), args)
    }

    /// The same, with what titles save under `data`.
    fn victory_with_data(&self, data: &Path, args: &[&std::ffi::OsStr]) -> Run {
        let out = Command::new(env!("CARGO_BIN_EXE_victory"))
            .args(args)
            .current_dir(self.path("cwd"))
            .env_remove("VICTORY_CONFIG")
            .env_remove("VICTORY_GAME_DIR")
            .env_remove("VICTORY_TEST_TITLES")
            .env("VICTORY_CACHE_DIR", self.path("cache"))
            .env("VICTORY_DATA_DIR", data)
            .env("VICTORY_FIXTURE_LOG", self.title_log())
            .output()
            .expect("victory starts");
        Run::of(out)
    }
}

/// How one run of `victory` ended and what it said.
#[derive(Debug)]
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    fn of(out: Output) -> Run {
        Run {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }
}

/// `run` takes no title from the configuration: without one on the command
/// line nothing is started, and the message names what is missing.
#[test]
fn run_without_a_title_names_the_missing_argument() {
    let home = Home::new();
    for args in [&["run"][..], &["run", "--execute"]] {
        let args: Vec<&std::ffi::OsStr> = args.iter().map(|a| a.as_ref()).collect();
        let run = home.victory(&args);
        assert_eq!(run.code, Some(2), "{run:#?}");
        assert_eq!(run.stdout, "", "{run:#?}");
        assert!(run.stderr.contains("<EXE>"), "{run:#?}");
        assert!(run.stderr.contains("Usage: "), "{run:#?}");
        assert!(!home.path("data").exists(), "nothing was made: {run:#?}");
    }
}

/// The loader is the Windows loader.
#[cfg(not(windows))]
#[test]
fn run_fails_by_name_on_a_host_that_is_not_windows() {
    let home = Home::new();
    let title = home.path("title").join("title.exe");
    let run = home.victory(&["run".as_ref(), "--execute".as_ref(), title.as_os_str()]);
    assert_eq!(run.code, Some(1), "{run:#?}");
    assert_eq!(run.stdout, "", "{run:#?}");
    assert!(
        run.stderr.contains("running titles requires Windows"),
        "{run:#?}"
    );
    assert!(!home.path("data").exists(), "nothing was made: {run:#?}");
}

#[cfg(windows)]
mod with_the_stand_in_title {
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};

    use super::Home;
    use super::common::fixture;

    /// What tests/fixtures/hle-probe-exe answers with (the constants at the
    /// top of its main.rs): its exit code is this plus what
    /// `XGameRuntimeInitialize` returned, which is `S_OK` from our runtime.
    const EXIT_BASE: i32 = 7;
    /// What it returns when it finds no log to write to.
    const FAILED_NO_LOG: i32 = 0xE001;
    const TEMPLATE_VALUE: u64 = 41;
    const WORKER_ADDS: [u64; 2] = [100, 200];

    /// A synthetic manifest, in the two elements our reader takes; the
    /// title id is made up.
    const MANIFEST: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
        <Game configVersion=\"1\">\n  <TitleId>0A0B0C0D</TitleId>\n  \
        <PersistentLocalStorage><SizeMB>16</SizeMB></PersistentLocalStorage>\n</Game>\n";

    /// Copies the fixture `file` into the title's folder: the fixtures are
    /// built inside the source tree, and a title's folder is not there.
    fn install(home: &Home, file: &str) -> PathBuf {
        let to = home.path("title").join(file);
        std::fs::copy(fixture(file), &to).unwrap();
        to
    }

    /// The one line of `text` that starts with `start`, without it.
    #[track_caller]
    fn line<'a>(text: &'a str, start: &str) -> &'a str {
        let found: Vec<&str> = text.lines().filter_map(|l| l.strip_prefix(start)).collect();
        match found[..] {
            [line] => line.trim_end(),
            _ => panic!("{} lines start with {start:?} in:\n{text}", found.len()),
        }
    }

    /// The four numbers of `bound <image>: a hle, b host, c title, d missing`.
    #[track_caller]
    fn bound(text: &str, image: &str) -> [u64; 4] {
        let counts = line(text, &format!("bound          {image}: "));
        let numbers: Vec<u64> = counts
            .split(", ")
            .zip(["hle", "host", "title", "missing"])
            .map(|(part, who)| {
                let (number, name) = part.split_once(' ').expect("a count and who");
                assert_eq!(name, who, "{counts}");
                number.parse().expect("a count")
            })
            .collect();
        numbers.try_into().expect("four counts")
    }

    /// The `dll!function` names under `missing`.
    fn missing(text: &str) -> Vec<&str> {
        text.lines()
            .filter_map(|l| l.strip_prefix("  missing      "))
            .map(str::trim_end)
            .collect()
    }

    /// The folder the report says the title's storage is in.
    #[track_caller]
    fn storage(text: &str, id: &str) -> (PathBuf, String) {
        let said = line(text, &format!("title          {id}, storage "));
        let (path, size) = said.rsplit_once(" (").expect("a path and a size");
        (PathBuf::from(path), size.trim_end_matches(')').to_owned())
    }

    /// The source tree, as this host names it.
    fn repo_root() -> PathBuf {
        std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
            .expect("the tests run beside the source tree")
    }

    /// The number of the one line `name=number` in the title's log.
    fn seen(log: &str, name: &str) -> Option<u64> {
        let mut values = log
            .lines()
            .filter_map(|l| l.split_once('='))
            .filter(|(key, _)| *key == name);
        match (values.next(), values.next()) {
            (Some((_, value)), None) => value.trim().parse().ok(),
            _ => None,
        }
    }

    /// The names of what lies in `dir`, sorted.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn run_binds_the_title_without_starting_it() {
        let home = Home::new();
        let title = install(&home, "hle-probe-exe.exe");
        let run = home.victory(&["run".as_ref(), title.as_os_str()]);
        assert_eq!(run.code, Some(0), "{run:#?}");

        // The build is not one we know; it is named by its hash.
        let build = line(&run.stdout, "build          ");
        let hash = build
            .strip_prefix("unknown (")
            .and_then(|b| b.strip_suffix(')'))
            .unwrap_or_else(|| panic!("{build}"));
        assert_eq!(hash.len(), 64, "{build}");
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()), "{build}");
        assert!(line(&run.stdout, "mapped at      ").starts_with("0x"));
        assert_eq!(line(&run.stdout, "console        "), "Xbox Series X");

        // By property, not by number: the console import is ours, the
        // host has the rest, and nothing is left over.
        let [hle, host, title_dlls, none] = bound(&run.stdout, "hle-probe-exe.exe");
        assert!(hle >= 1 && host >= 1, "{run:#?}");
        assert_eq!((title_dlls, none), (0, 0), "{run:#?}");
        assert_eq!(missing(&run.stdout), [""; 0], "{run:#?}");

        // Without a manifest the title has id 0 and no quota.
        let (_, size) = storage(&run.stdout, "00000000");
        assert_eq!(size, "0 MB");

        // It stopped there, and said how to go on.
        assert_eq!(
            run.stdout.lines().last(),
            Some("not executing (pass --execute to start the title)"),
            "{run:#?}"
        );
        assert!(!run.stdout.contains("title exited"), "{run:#?}");
        assert!(!home.title_log().exists(), "the title ran: {run:#?}");
    }

    #[test]
    fn run_execute_runs_the_title_to_its_end() {
        let home = Home::new();
        let title = install(&home, "hle-probe-exe.exe");
        std::fs::write(home.path("title").join("MicrosoftGame.config"), MANIFEST).unwrap();
        let run = home.victory(&["run".as_ref(), "--execute".as_ref(), title.as_os_str()]);

        // What the title's entry point returned is how `victory` ends, as
        // it is for a process whose main function returns.
        assert_eq!(run.code, Some(EXIT_BASE), "{run:#?}");

        // The report: what was bound, then how the title ended.
        assert_eq!(
            run.stdout.lines().last(),
            Some(format!("title exited with {EXIT_BASE}").as_str()),
            "{run:#?}"
        );
        let [hle, _, _, none] = bound(&run.stdout, "hle-probe-exe.exe");
        assert!(hle >= 1, "{run:#?}");
        assert_eq!(none, 0, "{run:#?}");
        let bound_at = run.stdout.find("bound          ").unwrap();
        let ended_at = run.stdout.find("title exited").unwrap();
        assert!(bound_at < ended_at, "{run:#?}");

        // The title's own account: it called our runtime and got `S_OK`,
        // its threads each had their copy of its thread-local data, and
        // it was told that the process ends, last of all.
        let log = std::fs::read_to_string(home.title_log())
            .unwrap_or_else(|e| panic!("the title left no log ({e}): {run:#?}"));
        assert_eq!(seen(&log, "runtime"), Some(0), "{log}");
        assert_eq!(seen(&log, "main_initial"), Some(TEMPLATE_VALUE), "{log}");
        assert_eq!(seen(&log, "main_last"), Some(TEMPLATE_VALUE + 1), "{log}");
        for (thread, adds) in ["a", "b"].iter().zip(WORKER_ADDS) {
            let last = seen(&log, &format!("{thread}_last"));
            assert_eq!(last, Some(TEMPLATE_VALUE + adds), "{log}");
        }
        assert_eq!(log.lines().last(), Some("detached=1"), "{log}");

        // The manifest's id names the title's folders, which lie under
        // the data folder and outside the source tree.
        let data = home.path("data");
        let (storage, size) = storage(&run.stdout, "0A0B0C0D");
        assert_eq!(size, "16 MB");
        assert!(storage.is_dir(), "{}", storage.display());
        assert_eq!(storage, data.join("0A0B0C0D").join("storage"));
        assert!(data.join("0A0B0C0D").join("temp").is_dir(), "{run:#?}");
        let policy = victory_core::ensure_outside_repo(&storage);
        assert_eq!(policy.map_err(|e| e.to_string()), Ok(()));
        // Both in the host's own spelling: a folder inside the tree would
        // start with its root.
        let spelled = canonical(&storage);
        assert!(!spelled.starts_with(repo_root()), "{}", spelled.display());

        // The copy of the executable that is mapped lies in the cache, and
        // nothing was written beside the title.
        let images = names(&home.path("cache").join("images"));
        assert!(
            images
                .iter()
                .any(|n| n.starts_with("victory-image-") && n.ends_with(".dll")),
            "{images:?}"
        );
        assert_eq!(
            names(&home.path("title")),
            ["MicrosoftGame.config", "hle-probe-exe.exe"]
        );
    }

    /// `--strict`: nothing passes through to the host, and the report
    /// names each import that is then nobody's.
    #[test]
    fn run_strict_names_the_host_imports_as_missing() {
        let home = Home::new();
        let title = install(&home, "hle-probe-exe.exe");
        let run = home.victory(&["run".as_ref(), "--strict".as_ref(), title.as_os_str()]);
        assert_eq!(run.code, Some(0), "{run:#?}");
        let missing = missing(&run.stdout);
        for import in [
            "kernel32.dll!CreateThread",
            "kernel32.dll!GetCurrentThreadStackLimits",
        ] {
            assert!(missing.contains(&import), "{import}: {run:#?}");
        }
        // What our runtime implements stays bound.
        assert!(
            !missing.iter().any(|m| m.contains("XGameRuntimeInitialize")),
            "{run:#?}"
        );
        let [hle, host, _, none] = bound(&run.stdout, "hle-probe-exe.exe");
        assert!(hle >= 1, "{run:#?}");
        assert_eq!(host, 0, "{run:#?}");
        assert_eq!(none as usize, missing.len(), "{run:#?}");
        assert!(!home.title_log().exists(), "the title ran: {run:#?}");
    }

    /// The default for an import nobody has: the first call of one ends
    /// the run, by name, on the screen and in the crash log.
    #[test]
    fn run_execute_stops_at_the_first_call_of_a_missing_import() {
        let home = Home::new();
        let title = install(&home, "hle-probe-exe.exe");
        let args: [&OsStr; 4] = [
            "run".as_ref(),
            "--strict".as_ref(),
            "--execute".as_ref(),
            title.as_os_str(),
        ];
        let run = home.victory(&args);
        assert!(run.code != Some(0), "{run:#?}");
        assert!(!run.stdout.contains("title exited"), "{run:#?}");
        let said = run
            .stderr
            .lines()
            .find_map(|l| l.strip_prefix("VIctory: unimplemented import "))
            .unwrap_or_else(|| panic!("no import is named: {run:#?}"));
        let (import, _) = said.split_once(' ').expect("the import, then `called`");
        assert!(import.starts_with("kernel32.dll!"), "{said}");
        assert!(missing(&run.stdout).contains(&import), "{import}: {run:#?}");
        // The crash log is beside the title's folders, under the data
        // folder.
        let crashes = home.path("data").join("00000000").join("crashes.log");
        let crashes = std::fs::read_to_string(&crashes)
            .unwrap_or_else(|e| panic!("{}: {e}: {run:#?}", crashes.display()));
        assert!(
            crashes.contains(&format!("VIctory: unimplemented import {import} called")),
            "{crashes}"
        );
        assert!(!home.title_log().exists(), "{run:#?}");
    }

    /// `--unimplemented zero`: a missing import returns 0 and the title
    /// goes on. The stand-in then finds no name for its log (the call that
    /// reads the environment answers 0) and returns its code for that.
    #[test]
    fn run_unimplemented_zero_lets_a_missing_import_return() {
        let home = Home::new();
        let title = install(&home, "hle-probe-exe.exe");
        let args: [&OsStr; 6] = [
            "run".as_ref(),
            "--strict".as_ref(),
            "--unimplemented".as_ref(),
            "zero".as_ref(),
            "--execute".as_ref(),
            title.as_os_str(),
        ];
        let run = home.victory(&args);
        // A code that does not fit a byte arrives whole.
        assert_eq!(run.code, Some(FAILED_NO_LOG), "{run:#?}");
        assert_eq!(
            run.stdout.lines().last(),
            Some(format!("title exited with {FAILED_NO_LOG}").as_str()),
            "{run:#?}"
        );
        assert!(
            !run.stderr.contains("VIctory: unimplemented import"),
            "{run:#?}"
        );
        assert!(!home.title_log().exists(), "{run:#?}");
    }

    /// The DLLs the stand-in title loads itself when they are beside it:
    /// `title_b.dll`, the title DLLs that one needs, and nothing else.
    const LATE_DLLS: [&str; 3] = ["title_a.dll", "title_quiet.dll", "title_b.dll"];
    /// What `title_b_chain` returns: its own mark over what `title_a.dll`'s
    /// `title_a_named` answered (tests/fixtures/title-a and title-b).
    const LATE_CHAIN: u64 = 0x0B00_0000 | 0x0A00_0001;
    /// The file of its package the stand-in reads (`G:\standin-package.txt`)
    /// and the temporary file it copies it to (`T:\standin-temp.txt`).
    const PACKAGE_FILE: &str = "standin-package.txt";
    const TEMP_FILE: &str = "standin-temp.txt";
    const PACKAGE_TEXT: &[u8] = b"a file of the stand-in title's package\r\n";

    /// The stand-in's checksum (FNV-1a, 32 bits; `sum` in its main.rs).
    fn sum(bytes: &[u8]) -> u64 {
        let sum = bytes.iter().fold(0x811C_9DC5u32, |sum, byte| {
            (sum ^ u32::from(*byte)).wrapping_mul(0x0100_0193)
        });
        sum.into()
    }

    /// The lines `<dll> <reason>` the title DLLs' `DllMain`s wrote to the
    /// title's log, in order.
    fn dll_notices(log: &str) -> Vec<(&str, u32)> {
        log.lines()
            .filter_map(|l| l.trim_end().split_once(' '))
            .filter_map(|(dll, reason)| Some((dll, reason.parse().ok()?)))
            .collect()
    }

    /// `path` as the host names it, links and short names resolved, which
    /// is the form [`repo_root`] has.
    #[track_caller]
    fn canonical(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    /// A title loads a DLL it ships that nothing imports, and uses the
    /// console's drives: its package is `G:`, its temporary files `T:`.
    #[test]
    fn run_execute_lets_the_title_load_a_dll_and_use_its_drives() {
        const DLL_PROCESS_ATTACH: u32 = 1;
        const DLL_THREAD_ATTACH: u32 = 2;
        let home = Home::new();
        let title = install(&home, "hle-probe-exe.exe");
        for dll in LATE_DLLS {
            install(&home, dll);
        }
        std::fs::write(home.path("title").join("MicrosoftGame.config"), MANIFEST).unwrap();
        std::fs::write(home.path("title").join(PACKAGE_FILE), PACKAGE_TEXT).unwrap();
        let beside_the_title = names(&home.path("title"));
        let run = home.victory(&["run".as_ref(), "--execute".as_ref(), title.as_os_str()]);
        assert_eq!(run.code, Some(EXIT_BASE), "{run:#?}");
        let log = std::fs::read_to_string(home.title_log())
            .unwrap_or_else(|e| panic!("the title left no log ({e}): {run:#?}"));

        // Nothing imports the three DLLs: none is mapped before the title
        // starts. The title's `LoadLibraryW` maps the one it names and
        // the two that one needs, those first, and the report says who
        // asked.
        let mapped: Vec<(&str, usize)> = run
            .stdout
            .match_indices("title dll      ")
            .map(|(at, start)| {
                let line = run.stdout[at + start.len()..].lines().next().unwrap();
                (line.trim_end(), at)
            })
            .collect();
        let started_at = run.stdout.find("bound          hle-probe-exe.exe").unwrap();
        let ended_at = run.stdout.find("title exited").unwrap();
        for dll in LATE_DLLS {
            let lines: Vec<_> = mapped
                .iter()
                .filter(|(line, _)| line.starts_with(&format!("{dll} at 0x")))
                .collect();
            let [(line, at)] = lines[..] else {
                panic!("{dll}: {lines:?} in {run:#?}");
            };
            assert!(line.ends_with(" (loaded by the title)"), "{line}");
            assert!(started_at < *at && *at < ended_at, "{dll}: {run:#?}");
        }
        let last = mapped.iter().max_by_key(|(_, at)| at).map(|(line, _)| line);
        assert!(
            last.is_some_and(|line| line.starts_with("title_b.dll at ")),
            "what the title named comes after what it needs: {run:#?}"
        );
        // title_b.dll's imports from title_a.dll are bound to it: no
        // import from a title DLL is left over. (The two DLLs carry
        // Rust's standard library, which imports from a host DLL the
        // host policy does not pass through; that import is reported,
        // and nothing here calls it.)
        let [_, _, from_title, _] = bound(&run.stdout, "title_b.dll");
        assert!(from_title >= 1, "{run:#?}");
        let left_over = missing(&run.stdout);
        assert!(
            !left_over
                .iter()
                .any(|m| m.to_ascii_lowercase().starts_with("title_")),
            "{left_over:?}"
        );

        // The title's account: it got a module, the same one when it
        // asked again, and its call went through title_b.dll into
        // title_a.dll.
        assert_eq!(seen(&log, "late_dll"), Some(1), "{log}");
        assert_eq!(seen(&log, "late_again"), Some(1), "{log}");
        assert_eq!(seen(&log, "late_chain"), Some(LATE_CHAIN), "{log}");
        // Each DLL was told that it was loaded, once, title_a.dll before
        // title_b.dll, which calls into it; and both heard of the two
        // threads the title started afterwards.
        let notices = dll_notices(&log);
        let attached: Vec<&str> = notices
            .iter()
            .filter(|(_, reason)| *reason == DLL_PROCESS_ATTACH)
            .map(|(dll, _)| *dll)
            .collect();
        assert_eq!(attached, ["title_a", "title_b"], "{log}");
        for dll in ["title_a", "title_b"] {
            let threads = notices
                .iter()
                .filter(|n| **n == (dll, DLL_THREAD_ATTACH))
                .count();
            assert!(threads >= 2, "{dll} heard of {threads} threads: {log}");
        }

        // The package: the title read the file beside its executable as
        // `G:\standin-package.txt`.
        let len = PACKAGE_TEXT.len() as u64;
        assert_eq!(seen(&log, "package_found"), Some(1), "{log}");
        assert_eq!(seen(&log, "package_bytes"), Some(len), "{log}");
        assert_eq!(seen(&log, "package_sum"), Some(sum(PACKAGE_TEXT)), "{log}");
        // The temporary drive: what it wrote to `T:\standin-temp.txt` it
        // read back, and the file is in the title's temp folder under the
        // data folder, with those bytes.
        assert_eq!(seen(&log, "temp_written"), Some(len), "{log}");
        assert_eq!(seen(&log, "temp_bytes"), Some(len), "{log}");
        assert_eq!(seen(&log, "temp_sum"), Some(sum(PACKAGE_TEXT)), "{log}");
        let temp = home.path("data").join("0A0B0C0D").join("temp");
        assert_eq!(names(&temp), [TEMP_FILE], "{run:#?}");
        let written = std::fs::read(temp.join(TEMP_FILE)).unwrap();
        assert_eq!(written, PACKAGE_TEXT);

        // Everything the run wrote lies outside the source tree: the
        // temporary file, the copies of the images that were mapped, and
        // the title's log. Both sides are the host's own spelling of the
        // path, so a path inside would start with the root.
        let root = repo_root();
        let images = home.path("cache").join("images");
        assert!(!names(&images).is_empty(), "{run:#?}");
        for made in [temp.join(TEMP_FILE), images, home.title_log()] {
            let policy = victory_core::ensure_outside_repo(&made);
            assert_eq!(policy.map_err(|e| e.to_string()), Ok(()));
            let made = canonical(&made);
            assert!(!made.starts_with(&root), "{}", made.display());
        }
        // The package is read, not written: beside the title lies what
        // was put there, and the folder victory was started in is empty.
        assert_eq!(names(&home.path("title")), beside_the_title);
        assert_eq!(names(&home.path("cwd")), [""; 0]);
    }

    /// Without the DLL and the package file beside it, the title is told
    /// that neither is there, and nothing is written to its drives.
    #[test]
    fn run_execute_tells_the_title_what_its_folder_does_not_have() {
        let home = Home::new();
        let title = install(&home, "hle-probe-exe.exe");
        let run = home.victory(&["run".as_ref(), "--execute".as_ref(), title.as_os_str()]);
        assert_eq!(run.code, Some(EXIT_BASE), "{run:#?}");
        let log = std::fs::read_to_string(home.title_log())
            .unwrap_or_else(|e| panic!("the title left no log ({e}): {run:#?}"));
        assert_eq!(seen(&log, "late_dll"), Some(0), "{log}");
        assert_eq!(seen(&log, "late_chain"), Some(0), "{log}");
        assert!(!run.stdout.contains("title dll "), "{run:#?}");
        assert!(dll_notices(&log).is_empty(), "{log}");
        assert_eq!(seen(&log, "package_found"), Some(0), "{log}");
        assert_eq!(seen(&log, "temp_written"), Some(0), "{log}");
        let temp = home.path("data").join("00000000").join("temp");
        assert_eq!(names(&temp), [""; 0], "{run:#?}");
        assert_eq!(names(&home.path("title")), ["hle-probe-exe.exe"]);
    }

    /// The stand-in title that is a DLL, with the DLLs it ships beside it.
    fn install_the_probe(home: &Home) -> PathBuf {
        for dll in ["title_a.dll", "title_b.dll", "title_quiet.dll"] {
            install(home, dll);
        }
        install(home, "hle_probe.dll")
    }

    /// A DLL has no entry point of a program: it is not started, and
    /// nothing is mapped or made for the attempt.
    #[test]
    fn run_execute_refuses_a_dll() {
        let home = Home::new();
        let probe = install_the_probe(&home);
        let run = home.victory(&["run".as_ref(), "--execute".as_ref(), probe.as_os_str()]);
        assert_eq!(run.code, Some(1), "{run:#?}");
        assert_eq!(run.stdout, "", "{run:#?}");
        let said = run.stderr.lines().find(|l| l.starts_with("Error: "));
        let said = said.unwrap_or_else(|| panic!("no error: {run:#?}"));
        assert!(said.contains("hle_probe.dll is a DLL"), "{said}");
        assert!(said.contains("cannot be started as a title"), "{said}");
        assert!(!home.path("data").exists(), "{run:#?}");
        assert!(!home.path("cache").exists(), "{run:#?}");
    }

    /// Without `--execute` a DLL is mapped and bound like any image, with
    /// the title DLLs it imports from.
    #[test]
    fn run_binds_a_dll_and_the_title_dlls_it_imports() {
        let home = Home::new();
        let probe = install_the_probe(&home);
        let run = home.victory(&["run".as_ref(), probe.as_os_str()]);
        assert_eq!(run.code, Some(0), "{run:#?}");
        assert!(
            line(&run.stdout, "title dll      title_a.dll at ").starts_with("0x"),
            "{run:#?}"
        );
        let [hle, host, from_title, none] = bound(&run.stdout, "hle_probe.dll");
        assert!(hle >= 1 && host >= 1 && from_title >= 1, "{run:#?}");
        // The import from a DLL nobody has is named.
        let missing = missing(&run.stdout);
        let nobodys = "victory-probe-missing.dll!VIctoryProbeNotImplemented";
        assert!(missing.contains(&nobodys), "{run:#?}");
        assert!(none >= 1, "{run:#?}");
        for ours in ["XGameRuntimeInitialize", "title_a_named"] {
            assert!(
                !missing.iter().any(|m| m.contains(ours)),
                "{ours}: {run:#?}"
            );
        }
        assert!(!run.stdout.contains("title exited"), "{run:#?}");
    }

    #[test]
    fn run_execute_fails_on_a_title_that_is_not_there() {
        let home = Home::new();
        let absent = home.path("title").join("absent.exe");
        let run = home.victory(&["run".as_ref(), "--execute".as_ref(), absent.as_os_str()]);
        assert_eq!(run.code, Some(1), "{run:#?}");
        assert_eq!(run.stdout, "", "{run:#?}");
        assert!(run.stderr.contains("Error: reading "), "{run:#?}");
        assert!(run.stderr.contains("absent.exe"), "{run:#?}");
        // Nothing was made for a title that is not there.
        assert!(!home.path("data").exists(), "{run:#?}");
        assert!(!home.path("cache").exists(), "{run:#?}");
    }

    #[test]
    fn run_execute_fails_on_a_file_that_is_no_image() {
        let home = Home::new();
        let notes = home.path("title").join("notes.exe");
        std::fs::write(&notes, b"not an image").unwrap();
        let run = home.victory(&["run".as_ref(), "--execute".as_ref(), notes.as_os_str()]);
        assert_eq!(run.code, Some(1), "{run:#?}");
        assert_eq!(run.stdout, "", "{run:#?}");
        assert!(run.stderr.contains("Error: malformed image"), "{run:#?}");
        assert!(!home.path("data").exists(), "{run:#?}");
        assert!(!home.path("cache").exists(), "{run:#?}");
    }

    /// What a title saves is never put inside the source tree: the run
    /// ends before the title starts, and the folder is not made.
    #[test]
    fn run_refuses_a_data_folder_inside_the_repository() {
        let home = Home::new();
        let title = install(&home, "hle-probe-exe.exe");
        let inside = repo_root().join("target").join("victory-run-test-data");
        assert!(!inside.exists(), "{} is in the way", inside.display());
        let args: [&OsStr; 3] = ["run".as_ref(), "--execute".as_ref(), title.as_os_str()];
        let run = home.victory_with_data(&inside, &args);
        assert_eq!(run.code, Some(1), "{run:#?}");
        assert!(
            run.stderr.contains("it is inside the VIctory source tree"),
            "{run:#?}"
        );
        assert!(!run.stdout.contains("bound "), "{run:#?}");
        assert!(!run.stdout.contains("title exited"), "{run:#?}");
        assert!(!home.title_log().exists(), "the title ran: {run:#?}");
        assert!(!inside.exists(), "{} was made", inside.display());
    }
}
