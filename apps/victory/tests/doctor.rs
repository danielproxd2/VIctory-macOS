//! `victory doctor` as its user sees it: the real program, its lines and
//! its exit status. Natively it must end cleanly and say what it could not
//! ask; under Wine (tools/macos/test-wine.sh) it reports the real device.

use std::path::Path;
use std::process::{Command, Output};

/// `victory doctor`, started where there is no victory.toml, with no
/// variable of the caller's shell and with the cache and data folders in
/// `home`.
fn doctor_command(home: &Path) -> Command {
    let elsewhere = std::env::temp_dir();
    assert!(
        !elsewhere.join("victory.toml").exists(),
        "a victory.toml in {} would be read",
        elsewhere.display()
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_victory"));
    command
        .arg("doctor")
        .current_dir(&elsewhere)
        .env_remove("VICTORY_CONFIG")
        .env_remove("VICTORY_TEST_TITLES")
        .env_remove("VICTORY_FIDELITYFX_DIR")
        .env_remove("VICTORY_GAME_DIR")
        .env("VICTORY_CACHE_DIR", home.join("cache"))
        .env("VICTORY_DATA_DIR", home.join("data"));
    command
}

/// Runs `victory doctor` with no config file (the working directory has
/// none). `game` is the configured game directory, if any.
fn doctor(home: &Path, game: Option<&str>) -> Output {
    let mut command = doctor_command(home);
    if let Some(game) = game {
        command.env("VICTORY_GAME_DIR", game);
    }
    command.output().expect("victory starts")
}

/// Kept for as long as a child lives that may make a device: on Windows
/// and under Wine the doctor makes one where there is one, and the device
/// tests of this binary must not run beside it. Unlike
/// `test_support::gpu` it asks for no device, so the test runs on every
/// host. The native build makes no device and takes no lock.
#[cfg(windows)]
fn child_may_make_a_device() -> victory_gpu::test_support::Serial {
    victory_gpu::test_support::serial()
}

#[cfg(not(windows))]
fn child_may_make_a_device() {}

/// The report's one line about `name`: its mark and what it says.
#[track_caller]
fn item<'a>(report: &'a str, name: &str) -> (&'a str, &'a str) {
    let marks = ["ok", "warning", "PROBLEM", "not checked"];
    let mut found = report.lines().filter_map(|line| {
        let mark = marks.iter().find(|m| line.starts_with(**m))?;
        let said = line[mark.len()..]
            .trim_start()
            .strip_prefix(name)?
            .strip_prefix(": ")?;
        Some((*mark, said))
    });
    match (found.next(), found.next()) {
        (Some(item), None) => item,
        (None, _) => panic!("no line about {name:?} in:\n{report}"),
        _ => panic!("several lines about {name:?} in:\n{report}"),
    }
}

/// The exit status says whether a line is marked PROBLEM, and nothing else.
#[track_caller]
fn status_follows_the_marks(out: &Output) -> String {
    let report = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let problems = report.lines().filter(|l| l.starts_with("PROBLEM")).count();
    assert_eq!(
        out.status.code(),
        Some(i32::from(problems > 0)),
        "{report}\n{stderr}"
    );
    // Its findings are lines of the report, never an error of the command.
    assert!(!stderr.contains("Error:"), "{stderr}");
    assert!(
        report.contains(&format!("{problems} PROBLEM")),
        "the summary counts the problems:\n{report}"
    );
    report
}

/// The native build asks Windows nothing and says, item by item, how to
/// have it asked. It still checks what it has: the prebuilt shaders.
#[cfg(not(windows))]
#[test]
fn doctor_runs_natively_and_says_what_it_did_not_check() {
    let home = tempfile::tempdir().unwrap();
    let out = doctor(home.path(), None);
    let report = status_follows_the_marks(&out);
    const WINE: &str = "run under Wine via tools/macos/victory.sh doctor";
    for name in [
        "cpu",
        "d3d12 device",
        "adapter",
        "shader model",
        "mesh shaders",
        "64-bit atomics",
        "castable formats",
        "cache_dir",
        "data_dir",
        "dxil validator",
    ] {
        assert_eq!(item(&report, name), ("not checked", WINE), "{name}");
    }
    assert_eq!(item(&report, "host").0, "not checked");
    assert_eq!(item(&report, "clamp bytecode").0, "ok");
    assert_eq!(item(&report, "depth bytecode").0, "ok");
    assert_eq!(out.status.code(), Some(0), "{report}");
    // Nothing was made in the folders it did not check.
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
}

/// A victory.toml copied from the old example names a folder on a drive
/// nobody has. `victory config` fails on it; the doctor says what it is,
/// goes on to every other item and does not count it as a problem.
#[test]
fn doctor_names_the_example_game_dir_and_goes_on() {
    let _device = child_may_make_a_device();
    let home = tempfile::tempdir().unwrap();
    let out = doctor(home.path(), Some(r"D:\victory-data\game"));
    let report = status_follows_the_marks(&out);
    let (mark, said) = item(&report, "game_dir");
    assert_eq!(mark, "warning", "{report}");
    assert!(
        said.starts_with(r"still the example value D:\victory-data\game"),
        "{said}"
    );
    // The items after it are all there.
    item(&report, "data_dir");
    item(&report, "depth bytecode");
}

/// Under Wine with D3DMetal, or on Windows: the lines about the device are
/// measurements of the real one.
#[cfg(windows)]
#[test]
fn doctor_reports_the_host_device() {
    use victory_gpu::test_support;
    // Kept to the end: the doctor makes its devices in a child process.
    let Some(_gpu) = test_support::gpu() else {
        return;
    };
    let home = tempfile::tempdir().unwrap();
    let out = doctor(home.path(), None);
    let report = status_follows_the_marks(&out);

    let host = item(&report, "host");
    assert_eq!(host.0, "ok");
    assert_eq!(
        host.1.starts_with("Wine"),
        test_support::under_wine(),
        "{host:?}"
    );
    // The tests are started the way the launcher starts a title.
    assert_eq!(item(&report, "cpu").0, "ok", "{report}");
    assert_eq!(item(&report, "d3d12 device").0, "ok", "{report}");
    let (mark, adapter) = item(&report, "adapter");
    assert_eq!(mark, "ok");
    assert!(!adapter.trim().is_empty());
    // The device tests of this tree need both; so does a title.
    let (mark, model) = item(&report, "shader model");
    assert_eq!(mark, "ok", "{model}");
    assert!(model.ends_with("(6.6 is required)"), "{model}");
    assert_eq!(item(&report, "mesh shaders").0, "ok", "{report}");

    // The two probes always come back with a measurement.
    let (mark, atomics) = item(&report, "64-bit atomics");
    assert!(mark == "ok" || mark == "warning", "{mark}: {atomics}");
    let (mark, casts) = item(&report, "castable formats");
    assert!(mark == "ok" || mark == "warning", "{mark}: {casts}");
    assert!(
        casts.starts_with("measured on 4 by 4 textures: "),
        "{casts}"
    );
    // What D3DMetal was measured to do (docs/macos.md, "Open differences
    // under D3DMetal"); a D3DMetal that has closed either fails here, as
    // the tests of victory-gpu that pin the same two do.
    if test_support::under_wine() && adapter == "AMD Compatibility Mode" {
        assert!(
            atomics.contains("wrote nothing (measured: texel 0x0, stored 0x0)"),
            "{atomics}"
        );
        for refused in [
            "R10G10B10_XR_BIAS_A2_UNORM (89)",
            "R8G8_B8G8_UNORM (68)",
            "G8R8_G8B8_UNORM (69)",
        ] {
            assert!(casts.contains(refused), "{casts}");
        }
        assert!(casts.contains("takes the other 11 asked"), "{casts}");
    }

    // The folders of the test were made and can be written to.
    for name in ["cache_dir", "data_dir"] {
        let (mark, said) = item(&report, name);
        assert_eq!(mark, "ok", "{said}");
        assert!(said.ends_with("can be written to"), "{said}");
    }
    assert!(home.path().join("data").is_dir());
    // The validator is the one the run names, if it names one.
    let (mark, validator) = item(&report, "dxil validator");
    match std::env::var_os("VICTORY_DXIL") {
        Some(_) => assert_eq!(mark, "ok", "{validator}"),
        None => assert_ne!(mark, "PROBLEM", "{validator}"),
    }
    assert_eq!(item(&report, "clamp bytecode").0, "ok", "{report}");
    assert_eq!(item(&report, "depth bytecode").0, "ok", "{report}");
    assert_eq!(out.status.code(), Some(0), "{report}");
}
