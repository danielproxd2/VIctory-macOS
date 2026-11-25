//! Where the tests find the stand-in title and the DLLs it ships: the
//! fixtures workspace in tests/fixtures, built for Windows. This is the one
//! place that knows; victory-hle's tests include this file too.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// The built fixture `file` (`hle_probe.dll`, `hle-probe-exe.exe`,
/// `title_a.dll`, `title_b.dll`, `title_draw.dll`, `title_loop_a.dll`,
/// `title_loop_b.dll`, `title_quiet.dll` or `title_unwind.dll`).
///
/// `VICTORY_FIXTURE_DIR` names the folder that holds them, built
/// beforehand: a cross build runs the tests under Wine, where cargo cannot
/// be started. `VICTORY_HLE_PROBE`, the older variable that names the probe
/// itself, still works: the others are beside it. With neither set, the
/// fixtures workspace is built here, once per test program.
///
/// `title_unwind.dll` is the one fixture built to unwind, which Cargo does
/// by profile: it lies in the folder beside that one, named after its
/// profile (`unwind` beside `debug`, `unwind-release` beside `release`).
/// Where a folder was named and that DLL was not built with the others, it
/// is built here, if this is the machine the tests were built on.
pub fn fixture(file: &str) -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    let dir = DIR.get_or_init(fixture_dir);
    if file != UNWINDING {
        let path = dir.join(file);
        assert!(path.is_file(), "no fixture at {}", path.display());
        return path;
    }
    let profile = match dir.file_name() {
        Some(name) if name == "release" => "unwind-release",
        _ => "unwind",
    };
    let path = dir.with_file_name(profile).join(file);
    // A folder straight under a target folder holds a build for this
    // machine; a cross build's has the target's name in between, and its
    // cargo is not a program this one can start.
    let native = dir
        .file_name()
        .is_some_and(|n| n == "debug" || n == "release")
        && Path::new(env!("CARGO"))
            .extension()
            .is_some_and(|e| e == "exe");
    if !path.is_file() && native {
        static BUILT: OnceLock<bool> = OnceLock::new();
        let target = dir
            .parent()
            .expect("a profile folder is in a target folder");
        assert!(
            *BUILT.get_or_init(|| build(target, &["-p", "title-unwind", "--profile", profile])),
            "building title-unwind failed"
        );
    }
    assert!(
        path.is_file(),
        "no fixture at {}; build it with: cargo build --manifest-path tests/fixtures/Cargo.toml \
         --target-dir <the fixtures' target folder> -p title-unwind --profile {profile}",
        path.display()
    );
    path
}

/// The fixture that is built with a profile of its own.
const UNWINDING: &str = "title_unwind.dll";

/// Builds the fixtures workspace into the target folder `target`, with
/// `more` arguments; whether that worked.
fn build(target: &Path, more: &[&str]) -> bool {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/Cargo.toml");
    let status = Command::new(env!("CARGO"))
        .args(["build", "--quiet", "--locked", "--manifest-path"])
        .arg(&manifest)
        .arg("--target-dir")
        .arg(target)
        .args(more)
        .status()
        .expect("running cargo");
    status.success()
}

fn fixture_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("VICTORY_FIXTURE_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(probe) = std::env::var_os("VICTORY_HLE_PROBE") {
        let probe = PathBuf::from(probe);
        return probe
            .parent()
            .unwrap_or_else(|| panic!("VICTORY_HLE_PROBE={} names no folder", probe.display()))
            .to_owned();
    }
    // Into a target folder of its own (the outer cargo holds the lock on
    // the main one while tests run), in the outer one's scratch folder for
    // tests, so that it follows `CARGO_TARGET_DIR` like the rest of the
    // build.
    let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("fixtures");
    // Everything but title-unwind, then title-unwind with its profile.
    for more in [&[][..], &["-p", "title-unwind", "--profile", "unwind"]] {
        assert!(build(&target, more), "building tests/fixtures failed");
    }
    target.join("debug")
}
