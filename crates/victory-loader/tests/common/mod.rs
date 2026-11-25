//! Where the tests find the stand-in title and the DLLs it ships: the
//! fixtures workspace in tests/fixtures, built for Windows. This is the one
//! place that knows; victory-hle's tests include this file too.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// The built fixture `file` (`hle_probe.dll`, `hle-probe-exe.exe`,
/// `title_a.dll`, `title_b.dll`, `title_loop_a.dll`, `title_loop_b.dll` or
/// `title_quiet.dll`).
///
/// `VICTORY_FIXTURE_DIR` names the folder that holds them, built
/// beforehand: a cross build runs the tests under Wine, where cargo cannot
/// be started. `VICTORY_HLE_PROBE`, the older variable that names the probe
/// itself, still works: the others are beside it. With neither set, the
/// fixtures workspace is built here, once per test program.
pub fn fixture(file: &str) -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    let path = DIR.get_or_init(fixture_dir).join(file);
    assert!(path.is_file(), "no fixture at {}", path.display());
    path
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
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/Cargo.toml");
    let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("fixtures");
    let status = Command::new(env!("CARGO"))
        .args(["build", "--quiet", "--locked", "--manifest-path"])
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&target)
        .status()
        .expect("running cargo");
    assert!(status.success(), "building tests/fixtures failed");
    target.join("debug")
}
