//! `victory inspect` and `victory census` on the stand-in title
//! (tests/fixtures/hle-probe), as a user runs them: the binary, its
//! standard output and its exit status. The probe is a Windows DLL, so
//! these run on Windows and under Wine; the same reports are tested on
//! every host with a synthetic image, beside the commands.
#![cfg(windows)]

use std::path::Path;
use std::process::{Command, Output};

#[path = "../../../crates/victory-loader/tests/common/mod.rs"]
mod common;

fn victory(args: &[&std::ffi::OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_victory"))
        .args(args)
        .output()
        .expect("victory starts")
}

/// The report of a run that must succeed.
fn report(out: &Output) -> String {
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    stdout
}

/// The lines under `heading`, up to the next empty line.
fn section<'a>(text: &'a str, heading: &str) -> Vec<&'a str> {
    text.lines()
        .skip_while(|l| !l.starts_with(heading))
        .skip(1)
        .take_while(|l| !l.is_empty())
        .collect()
}

/// The functions listed under the DLL `dll` in a section of `inspect
/// --imports`: the lines indented below its own.
fn functions<'a>(section: &[&'a str], dll: &str) -> Vec<&'a str> {
    section
        .iter()
        .skip_while(|l| {
            !l.trim_start()
                .to_ascii_lowercase()
                .starts_with(&format!("{dll} "))
        })
        .skip(1)
        .take_while(|l| l.starts_with("      "))
        .map(|l| l.trim())
        .collect()
}

#[test]
fn inspect_reports_the_probes_imports() {
    let probe = common::fixture("hle_probe.dll");
    let out = victory(&["inspect".as_ref(), probe.as_os_str(), "--imports".as_ref()]);
    let text = report(&out);
    assert!(text.contains("\nkind           dll\n"), "{text}");

    // By name, not by count: the probe grows with the tests that use it.
    let imports = section(&text, "imports ");
    for (dll, function) in [
        ("xgameruntime.dll", "XGameRuntimeInitialize"),
        ("victory-probe-missing.dll", "VIctoryProbeNotImplemented"),
        ("title_a.dll", "title_a_named"),
        ("kernel32.dll", "GetTickCount"),
        ("kernel32.dll", "GetCurrentProcessId"),
        ("kernel32.dll", "CreateFileW"),
        ("kernel32.dll", "LoadLibraryExA"),
        ("xmem.dll", "XMemAlloc"),
        ("xmem.dll", "XMemVirtualAlloc"),
        ("ws2_32.dll", "connect"),
    ] {
        assert!(
            functions(&imports, dll).contains(&function),
            "{dll}!{function}\n{text}"
        );
    }
    // The delay-loaded DLL is in a table of its own.
    let delayed = section(&text, "delay-load imports");
    assert!(
        functions(&delayed, "victory-probe-delayed.dll").contains(&"VIctoryProbeDelayed"),
        "{text}"
    );
    assert!(
        functions(&imports, "victory-probe-delayed.dll").is_empty(),
        "{text}"
    );

    // Without --imports the functions are not listed.
    let text = report(&victory(&["inspect".as_ref(), probe.as_os_str()]));
    assert!(!text.contains("XGameRuntimeInitialize"), "{text}");

    // --exports lists what the probe offers the tests.
    let out = victory(&["inspect".as_ref(), probe.as_os_str(), "--exports".as_ref()]);
    let text = report(&out);
    for export in [
        "probe_xgameruntime_init",
        "probe_missing",
        "probe_delayed",
        "probe_xmem_alloc",
        "probe_connect_ordinal",
        "probe_create",
        "probe_query_api",
        // Data: the table of allocation hooks a title's image may export.
        "__XMemAllocationHooks__",
    ] {
        assert!(
            text.lines().any(|l| l.contains(&format!("  {export}  0x"))),
            "{export}\n{text}"
        );
    }
}

#[test]
fn inspect_fails_on_a_file_that_is_no_image() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("notes.txt");
    std::fs::write(&file, b"not an image").unwrap();
    let out = victory(&["inspect".as_ref(), file.as_os_str()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("parsing "), "{stderr}");
}

/// A title's folder holding a copy of the probe and nothing else.
fn probe_alone() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let probe = common::fixture("hle_probe.dll");
    std::fs::copy(probe, dir.path().join("hle_probe.dll")).unwrap();
    dir
}

/// The `dll!function` names `census --missing` lists.
fn missing(dir: &Path, strict: bool) -> Vec<String> {
    let mut args = vec!["census".as_ref(), dir.as_os_str(), "--missing".as_ref()];
    if strict {
        args.push("--strict".as_ref());
    }
    let text = report(&victory(&args));
    assert!(
        text.starts_with("1 images scanned, 0 unreadable\n"),
        "{text}"
    );
    let (_, list) = text
        .split_once("\nmissing (by number of images using it)\n")
        .unwrap_or_else(|| panic!("no list of missing imports in\n{text}"));
    list.lines()
        .map(|l| {
            let (users, name) = l.trim().split_once("  ").expect("a count and a name");
            assert_eq!(users, "1", "{l}");
            name.to_owned()
        })
        .collect()
}

#[test]
fn census_reports_covered_and_missing_imports() {
    let dir = probe_alone();
    let covered = [
        // Implemented by our runtime.
        "xgameruntime.dll!XGameRuntimeInitialize",
        "xmem.dll!XMemAlloc",
        // The host's, overridden by our runtime.
        "kernel32.dll!CreateFileW",
        "ws2_32.dll!connect",
        // Passed through to the host.
        "kernel32.dll!GetCurrentProcessId",
    ];
    let nobodys = [
        // A DLL nobody has, on purpose.
        "victory-probe-missing.dll!VIctoryProbeNotImplemented",
        // Census maps no title DLLs, so what one would export is missing
        // here; `run` binds it.
        "title_a.dll!title_a_named",
        "victory-probe-delayed.dll!VIctoryProbeDelayed",
    ];
    let list = missing(dir.path(), false);
    for name in nobodys {
        assert!(list.iter().any(|m| m == name), "{name} in {list:?}");
    }
    for name in covered {
        assert!(!list.iter().any(|m| m == name), "{name} in {list:?}");
    }

    // --strict: nothing passes through to the host any more; what our
    // runtime implements stays covered.
    let list = missing(dir.path(), true);
    for name in nobodys {
        assert!(list.iter().any(|m| m == name), "{name} in {list:?}");
    }
    for ours in [
        "xgameruntime.dll!XGameRuntimeInitialize",
        "xmem.dll!XMemAlloc",
        "kernel32.dll!CreateFileW",
        "ws2_32.dll!connect",
    ] {
        assert!(!list.iter().any(|m| m == ours), "{ours} in {list:?}");
    }
}
