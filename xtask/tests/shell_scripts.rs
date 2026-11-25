//! The shell scripts in tools/macos: that they parse, and the parts of
//! test-wine.sh, victory.sh and gnullvm-cc.sh that can run without Wine or
//! a compiler, with stand-ins for the programs they start.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace")
        .into()
}

/// An empty folder for the test `name`.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes the executable shell script `body` to `path`.
fn script(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Every script parses in the shell its first line names.
#[test]
fn shell_scripts_parse() {
    let root = root();
    let mut scripts = vec![root.join("setup.sh"), root.join(".githooks/pre-commit")];
    for entry in fs::read_dir(root.join("tools/macos")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "sh") {
            scripts.push(path);
        }
    }
    assert!(
        scripts
            .iter()
            .any(|s| s.ends_with("tools/macos/test-wine.sh"))
    );
    for path in scripts {
        let source = fs::read_to_string(&path).unwrap();
        let shell = source
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("#!"))
            .unwrap_or_else(|| panic!("{} does not name its shell", path.display()))
            .trim();
        let checked = Command::new(shell).arg("-n").arg(&path).output();
        let checked = checked.unwrap_or_else(|e| panic!("running {shell}: {e}"));
        assert!(
            checked.status.success(),
            "{shell} -n {}: {}",
            path.display(),
            text(&checked.stderr)
        );
    }
}

/// Runs gnullvm-cc.sh with a MinGW folder holding the GCC `versions` and a
/// stand-in clang that prints its arguments, one per line.
fn gnullvm_cc(name: &str, versions: &[&str]) -> Output {
    let dir = scratch(name);
    for version in versions {
        fs::create_dir_all(dir.join("mingw/lib/gcc/x86_64-w64-mingw32").join(version)).unwrap();
    }
    script(&dir.join("llvm/bin/clang"), r#"printf '%s\n' "$@""#);
    Command::new(root().join("tools/macos/gnullvm-cc.sh"))
        .args(["-lunwind", "probe.o"])
        .env("MINGW_PREFIX", dir.join("mingw"))
        .env("LLVM_PREFIX", dir.join("llvm"))
        .env("LLD_PREFIX", dir.join("lld"))
        .output()
        .unwrap()
}

/// 15.2.0 is newer than 9.3.0, though it sorts before it as text.
#[test]
fn gnullvm_cc_picks_the_newest_gcc_by_version() {
    let out = gnullvm_cc("gnullvm-cc-newest", &["9.3.0", "15.2.0", "10.1.0"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let args = text(&out.stdout);
    let gcc: Vec<_> = args
        .lines()
        .filter(|a| a.starts_with("-L") && a.contains("/lib/gcc/"))
        .collect();
    assert_eq!(gcc.len(), 1, "{args}");
    assert!(gcc[0].ends_with("/x86_64-w64-mingw32/15.2.0"), "{args}");
    // What it does for the linker besides: libunwind's stand-ins, and the
    // arguments it was given.
    let args: Vec<_> = args.lines().collect();
    assert!(!args.contains(&"-lunwind"), "{args:?}");
    assert!(
        args.contains(&"-lgcc_eh") && args.contains(&"probe.o"),
        "{args:?}"
    );
}

#[test]
fn gnullvm_cc_says_when_mingw_is_missing() {
    let out = gnullvm_cc("gnullvm-cc-missing", &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    assert_eq!(text(&out.stdout), "", "clang must not be started");
    let said = text(&out.stderr);
    assert!(
        said.contains("mingw-w64") && said.contains("MINGW_PREFIX"),
        "{said}"
    );
}

/// Runs test-wine.sh with stand-ins for wine, rustc and cargo. The stand-in
/// rustc answers the question about the target's library folder with
/// `libdir`; the stand-in cargo writes what it was started with to the file
/// the returned path names.
fn test_wine(name: &str, libdir: &Path, args: &[&str]) -> (Output, PathBuf) {
    test_wine_with(name, libdir, args, None)
}

/// The same, with `ROSETTA_ADVERTISE_AVX` as given (`None`: not set).
fn test_wine_with(
    name: &str,
    libdir: &Path,
    args: &[&str],
    rosetta: Option<&str>,
) -> (Output, PathBuf) {
    let dir = scratch(name);
    let log = dir.join("cargo.log");
    script(&dir.join("bin/wine"), "exit 0");
    script(
        &dir.join("bin/rustc"),
        &format!("echo '{}'", libdir.display()),
    );
    script(
        &dir.join("bin/cargo"),
        r#"{
    printf '%s\n' "cargo $*"
    printf '%s\n' "fixtures $VICTORY_FIXTURE_DIR"
    printf '%s\n' "runner $CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_RUNNER"
    printf '%s\n' "rosetta ${ROSETTA_ADVERTISE_AVX-<unset>}"
    printf '%s\n' "cpu check ${VICTORY_SKIP_CPU_CHECK-<unset>}"
    [ ! -x "$CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_RUNNER" ] || echo "wrapper present"
} >> "$STAND_IN_LOG""#,
    );
    let mut command = Command::new(root().join("tools/macos/test-wine.sh"));
    command
        .args(args)
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", dir.join("bin").display()),
        )
        .env("WINE", dir.join("bin/wine"))
        .env("STAND_IN_LOG", &log)
        .env_remove("WINEDLLOVERRIDES")
        .env_remove("ROSETTA_ADVERTISE_AVX")
        .env_remove("VICTORY_SKIP_CPU_CHECK");
    if let Some(rosetta) = rosetta {
        command.env("ROSETTA_ADVERTISE_AVX", rosetta);
    }
    (command.output().unwrap(), log)
}

/// The lines of the stand-in cargo's log that start with `key`, without it.
fn logged(log: &Path, key: &str) -> Vec<String> {
    let text = fs::read_to_string(log).unwrap();
    text.lines()
        .filter_map(|line| line.strip_prefix(key))
        .map(|rest| rest.trim().to_owned())
        .collect()
}

#[test]
fn test_wine_says_when_the_rust_target_is_missing() {
    let nowhere = scratch("test-wine-no-target-libdir").join("absent");
    let (out, log) = test_wine("test-wine-no-target", &nowhere, &["-p", "victory-pe"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    let said = text(&out.stderr);
    assert!(
        said.contains("rustup target add x86_64-pc-windows-gnullvm"),
        "{said}"
    );
    assert!(!log.exists(), "cargo must not be started");
}

/// The fixtures workspace is built once, in the tests' profile, and the
/// fixture that unwinds once more with the profile that goes with it; the
/// tests are told where they are in Wine's spelling, and the wrapper script
/// is gone afterwards.
#[test]
fn test_wine_builds_the_fixtures_and_removes_its_wrapper() {
    let libdir = scratch("test-wine-libdir");
    let target = "x86_64-pc-windows-gnullvm";
    let fixtures = format!(
        "Z:{}\\target\\fixtures\\{target}",
        root().display().to_string().replace('/', "\\")
    );
    for (name, args, profile) in [
        ("test-wine-debug", &["-p", "victory-pe"][..], "debug"),
        (
            "test-wine-release",
            &["--release", "-p", "victory-pe"][..],
            "release",
        ),
        // After `--` the arguments are the test program's, not cargo's.
        (
            "test-wine-after-dashes",
            &["-p", "victory-pe", "--", "--release"][..],
            "debug",
        ),
    ] {
        let (out, log) = test_wine(name, &libdir, args);
        assert!(out.status.success(), "{name}: {}", text(&out.stderr));
        let cargo = logged(&log, "cargo ");
        assert_eq!(cargo.len(), 3, "{name}: {cargo:?}");
        for build in &cargo[..2] {
            assert!(
                build.starts_with("build ")
                    && build.contains("--manifest-path tests/fixtures/Cargo.toml")
                    && build.contains("--target-dir target/fixtures")
                    && build.contains(&format!("--target {target}")),
                "{name}: {cargo:?}"
            );
        }
        // Everything a plain build makes, then title-unwind by name.
        assert!(!cargo[0].contains(" -p "), "{name}: {cargo:?}");
        let unwind = if profile == "release" {
            "unwind-release"
        } else {
            "unwind"
        };
        assert!(
            cargo[1].ends_with(&format!(" -p title-unwind --profile {unwind}")),
            "{name}: {cargo:?}"
        );
        assert_eq!(
            cargo[0].contains("--release"),
            profile == "release",
            "{name}: {cargo:?}"
        );
        assert_eq!(
            cargo[2],
            format!("test --target {target} {}", args.join(" ")),
            "{name}"
        );
        // Known to the test run; the fixture builds come before it.
        let dirs = logged(&log, "fixtures ");
        assert_eq!(dirs[2], format!("{fixtures}\\{profile}"), "{name}");
        let runners = logged(&log, "runner ");
        let runner = Path::new(&runners[2]);
        assert_eq!(
            logged(&log, "wrapper present").len(),
            3,
            "{name}: no wrapper"
        );
        assert!(
            !runner.exists(),
            "{name}: {} was left behind",
            runner.display()
        );
        assert!(
            !runner.parent().unwrap().exists(),
            "{name}: {} was left behind",
            runner.parent().unwrap().display()
        );
    }
}

/// `victory run --execute` refuses a CPU without AVX2, and under Rosetta 2
/// only a process started with `ROSETTA_ADVERTISE_AVX=1` sees AVX2. The
/// tests start titles through that check: the script sets Rosetta's
/// variable for them, as victory.sh does for a real run, and does not turn
/// the check off. Whoever sets the variable keeps their value.
#[test]
fn test_wine_has_rosetta_advertise_avx_and_leaves_the_cpu_check_on() {
    let libdir = scratch("test-wine-rosetta-libdir");
    for (name, given, expected) in [
        ("test-wine-rosetta-default", None, "1"),
        ("test-wine-rosetta-empty", Some(""), "1"),
        ("test-wine-rosetta-off", Some("0"), "0"),
        ("test-wine-rosetta-on", Some("1"), "1"),
    ] {
        let (out, log) = test_wine_with(name, &libdir, &["-p", "victory"], given);
        assert!(out.status.success(), "{name}: {}", text(&out.stderr));
        // The fixture builds, then the test run, which is the one that
        // starts Wine.
        let rosetta = logged(&log, "rosetta ");
        assert_eq!(rosetta.len(), 3, "{name}: {rosetta:?}");
        assert_eq!(rosetta[2], expected, "{name}");
        assert_eq!(
            logged(&log, "cpu check "),
            ["<unset>"; 3],
            "{name}: the check must stay on"
        );
    }
}

/// What victory.sh starts Wine with, from stand-ins for cargo and wine in a
/// home folder of the test's own.
struct Launched {
    out: Output,
    /// The stand-in wine's `VICTORY_DATA_DIR`, `None` when it had none.
    data_dir: Option<String>,
    /// The stand-in wine's `ROSETTA_ADVERTISE_AVX` and
    /// `VICTORY_SKIP_CPU_CHECK`, `<unset>` for one it did not have.
    cpu: [String; 2],
    /// The arguments the stand-in wine got.
    args: Vec<String>,
    /// The home folder as Wine's Z: drive shows it.
    home: String,
}

/// Runs victory.sh with `VICTORY_DATA_DIR` and `VICTORY_CONFIG` as given.
/// `config` is the text of a config file in the test's folder; the script
/// is told about it by `config_name`, in which `{}` stands for the file's
/// macOS path. (Without `VICTORY_CONFIG` the script would read the
/// victory.toml of whoever runs the tests.)
fn victory(name: &str, data_dir: Option<&str>, config: &str, config_name: &str) -> Launched {
    let dir = scratch(name);
    let log = dir.join("wine.log");
    let home = dir.join("home");
    fs::create_dir_all(dir.join("wine/lib/external/D3DMetal.framework")).unwrap();
    script(
        &dir.join("bin/cargo"),
        r#"echo "cargo $*" >> "$STAND_IN_LOG""#,
    );
    script(
        &dir.join("wine/bin/wine"),
        r#"{
    printf '%s\n' "data ${VICTORY_DATA_DIR-<unset>}"
    printf '%s\n' "rosetta ${ROSETTA_ADVERTISE_AVX-<unset>}"
    printf '%s\n' "cpu check ${VICTORY_SKIP_CPU_CHECK-<unset>}"
    printf 'arg %s\n' "$@"
} >> "$STAND_IN_LOG""#,
    );
    let file = dir.join("victory config.toml");
    fs::write(&file, config).unwrap();
    let windows = |path: &Path| format!("Z:{}", path.display().to_string().replace('/', "\\"));
    let config_name = config_name
        .replace("{Z}", &windows(&file))
        .replace("{}", &file.display().to_string());
    let mut command = Command::new(root().join("tools/macos/victory.sh"));
    command
        .args(["run", "--execute", "a title.exe"])
        .env("CARGO", dir.join("bin/cargo"))
        .env("WINE", dir.join("wine/bin/wine"))
        .env("HOME", &home)
        .env("STAND_IN_LOG", &log)
        .env("VICTORY_CONFIG", config_name)
        .env_remove("VICTORY_DATA_DIR")
        .env_remove("ROSETTA_ADVERTISE_AVX")
        .env_remove("VICTORY_SKIP_CPU_CHECK")
        .env_remove("WINEPREFIX");
    if let Some(data_dir) = data_dir {
        command.env("VICTORY_DATA_DIR", data_dir);
    }
    let out = command.output().unwrap();
    assert!(out.status.success(), "{name}: {}", text(&out.stderr));
    assert_eq!(logged(&log, "cargo ").len(), 1, "{name}: built once");
    let data = logged(&log, "data ");
    assert_eq!(data.len(), 1, "{name}: wine started once");
    Launched {
        out,
        data_dir: (data[0] != "<unset>").then(|| data[0].clone()),
        cpu: [
            logged(&log, "rosetta ").concat(),
            logged(&log, "cpu check ").concat(),
        ],
        args: logged(&log, "arg "),
        home: windows(&home),
    }
}

/// The launcher starts a title through the AVX2 check of `victory run`:
/// it has Rosetta say that AVX2 is there and leaves the check on.
#[test]
fn victory_sh_has_rosetta_advertise_avx_and_leaves_the_cpu_check_on() {
    let launched = victory("victory-sh-rosetta", None, "[paths]\n", "{}");
    assert_eq!(launched.cpu, ["1", "<unset>"]);
}

/// What titles save must not sit in the Wine prefix, which the launcher
/// keeps under ~/Library/Caches: with no data folder named anywhere, the
/// launcher names one under ~/Library/Application Support.
#[test]
fn victory_sh_keeps_saved_data_out_of_the_cache_folder() {
    const NO_DATA_DIR: &str = "[paths]\ngame_dir = 'Z:\\g'\n# data_dir = 'Z:\\commented\\out'\n";
    let expected =
        |l: &Launched| format!("{}\\Library\\Application Support\\victory\\data", l.home);

    // The config file named the way Wine needs it, and by its macOS path.
    for (name, config_name) in [
        ("victory-sh-default", "{Z}"),
        ("victory-sh-default-unix", "{}"),
    ] {
        let launched = victory(name, None, NO_DATA_DIR, config_name);
        assert_eq!(launched.data_dir, Some(expected(&launched)), "{name}");
        assert!(expected(&launched).starts_with("Z:\\"), "{name}");
        assert!(!expected(&launched).contains("Caches"), "{name}");
        assert_eq!(text(&launched.out.stderr), "", "{name}");
        // The title's arguments arrive whole, behind the program.
        assert!(launched.args[0].ends_with("/release/victory.exe"), "{name}");
        assert_eq!(launched.args[1..], ["run", "--execute", "a title.exe"]);
    }
    // An empty variable names nothing, to the launcher as to victory.exe.
    let launched = victory("victory-sh-empty", Some(""), NO_DATA_DIR, "{Z}");
    assert_eq!(launched.data_dir, Some(expected(&launched)));

    // The user's variable stays as it is.
    let launched = victory("victory-sh-env", Some("Z:\\mine"), NO_DATA_DIR, "{Z}");
    assert_eq!(launched.data_dir.as_deref(), Some("Z:\\mine"));

    // A variable would override the config file's data_dir: none is set.
    for (name, config) in [
        ("victory-sh-file", "[paths]\ndata_dir = 'Z:\\saves'\n"),
        (
            "victory-sh-file-indented",
            "[paths]\n  data_dir='Z:\\saves'\n",
        ),
        ("victory-sh-file-dotted", "paths.data_dir = 'Z:\\saves'\n"),
        (
            "victory-sh-file-inline",
            "paths = { data_dir = 'Z:\\saves' }\n",
        ),
    ] {
        let launched = victory(name, None, config, "{Z}");
        assert_eq!(launched.data_dir, None, "{name}");
        assert_eq!(text(&launched.out.stderr), "", "{name}");
    }

    // A config file macOS cannot read may name one too; the launcher says
    // where the data goes if it does not.
    let launched = victory(
        "victory-sh-unreadable",
        None,
        "",
        "C:\\elsewhere\\victory.toml",
    );
    assert_eq!(launched.data_dir, None);
    let said = text(&launched.out.stderr);
    assert!(
        said.contains("C:\\elsewhere\\victory.toml") && said.contains("Library/Caches"),
        "{said}"
    );
}

/// No game is started by default: without arguments the launcher says
/// what to pass and fails, and builds and starts nothing.
#[test]
fn victory_sh_without_arguments_says_what_to_pass_and_fails() {
    let dir = scratch("victory-sh-no-arguments");
    let log = dir.join("started.log");
    fs::create_dir_all(dir.join("wine/lib/external/D3DMetal.framework")).unwrap();
    for program in ["bin/cargo", "wine/bin/wine"] {
        script(
            &dir.join(program),
            &format!(r#"echo "{program} $*" >> "$STAND_IN_LOG""#),
        );
    }
    let out = Command::new(root().join("tools/macos/victory.sh"))
        .env("CARGO", dir.join("bin/cargo"))
        .env("WINE", dir.join("wine/bin/wine"))
        .env("HOME", dir.join("home"))
        .env("STAND_IN_LOG", &log)
        .env_remove("VICTORY_CONFIG")
        .env_remove("VICTORY_DATA_DIR")
        .env_remove("WINEPREFIX")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "");
    let said = text(&out.stderr);
    assert!(said.contains("no game is started by default"), "{said}");
    assert!(
        said.contains("tools/macos/victory.sh run --execute <path to the game's executable>"),
        "{said}"
    );
    assert!(said.contains("tools/macos/victory.sh config"), "{said}");
    let started = fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(started, "", "nothing is built or started");
}

/// The launcher ends as what it started under Wine ended: victory.exe ends
/// with the exit code of the title it ran, and whoever starts the launcher
/// reads that code as its status. (A status has eight bits here; Wine
/// hands on the low eight of a Windows exit code.)
#[test]
fn victory_sh_ends_with_the_exit_code_of_what_wine_ran() {
    for code in [7, 0, 255] {
        let dir = scratch(&format!("victory-sh-exit-{code}"));
        let log = dir.join("started.log");
        fs::create_dir_all(dir.join("wine/lib/external/D3DMetal.framework")).unwrap();
        script(
            &dir.join("bin/cargo"),
            r#"echo "cargo $*" >> "$STAND_IN_LOG""#,
        );
        // A stand-in for Wine running victory.exe on a title that returns
        // `code`: victory's last line, then the title's code.
        script(
            &dir.join("wine/bin/wine"),
            &format!(
                r#"echo "wine $*" >> "$STAND_IN_LOG"
echo "title exited with {code}"
exit {code}"#
            ),
        );
        // A config file of the test's own: without one named, the script
        // would read the victory.toml of whoever runs the tests.
        let config = dir.join("victory.toml");
        fs::write(&config, "").unwrap();
        let out = Command::new(root().join("tools/macos/victory.sh"))
            .args(["run", "--execute", "a title.exe"])
            .env("CARGO", dir.join("bin/cargo"))
            .env("WINE", dir.join("wine/bin/wine"))
            .env("HOME", dir.join("home"))
            .env("STAND_IN_LOG", &log)
            .env("VICTORY_CONFIG", &config)
            .env_remove("VICTORY_DATA_DIR")
            .env_remove("WINEPREFIX")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(code), "{}", text(&out.stderr));
        // What the program said arrives too, and the launcher adds nothing.
        assert_eq!(text(&out.stdout), format!("title exited with {code}\n"));
        assert_eq!(text(&out.stderr), "", "code {code}");
        // Built once, then Wine started once, with the title's arguments.
        let started = fs::read_to_string(&log).unwrap();
        let started: Vec<&str> = started.lines().collect();
        let [cargo, wine] = started[..] else {
            panic!("code {code}: {started:?}");
        };
        assert!(cargo.starts_with("cargo build "), "{cargo}");
        assert!(wine.starts_with("wine "), "{wine}");
        assert!(
            wine.ends_with("/release/victory.exe run --execute a title.exe"),
            "{wine}"
        );
    }
}

/// Runs check-setup.sh, the last step of setup.sh, with a stand-in cargo
/// and the stand-in wine `wine` (a shell script's body; it stands for Wine
/// running victory.exe with the arguments after the program's path).
fn check_setup(name: &str, wine: &str) -> Output {
    let dir = scratch(name);
    fs::create_dir_all(dir.join("wine/lib/external/D3DMetal.framework")).unwrap();
    script(&dir.join("bin/cargo"), "exit 0");
    script(&dir.join("wine/bin/wine"), &format!("shift\n{wine}"));
    // A config file of the test's own: without one named, the launcher
    // would read the victory.toml of whoever runs the tests.
    let config = dir.join("victory.toml");
    fs::write(&config, "").unwrap();
    Command::new(root().join("tools/macos/check-setup.sh"))
        .env("CARGO", dir.join("bin/cargo"))
        .env("WINE", dir.join("wine/bin/wine"))
        .env("HOME", dir.join("home"))
        .env("VICTORY_CONFIG", &config)
        .env_remove("VICTORY_DATA_DIR")
        .env_remove("WINEPREFIX")
        .output()
        .unwrap()
}

/// What victory.exe says for `--version`, and for `config` with no game
/// configured: the state victory.example.toml ships in.
const STARTS: &str = r#"case "$1" in
    --version) echo "victory 0.1.0"; exit 0 ;;
esac"#;

/// A fresh setup has no game configured. The setup's last step proves that
/// VIctory starts under Wine by its version, which needs no game, and
/// shows the configuration.
#[test]
fn check_setup_passes_on_a_fresh_setup_with_no_game_configured() {
    let out = check_setup(
        "check-setup-fresh",
        &format!(
            r#"{STARTS}
echo "game_dir    : (not set)"
echo "NOTE: game_dir is not set: no game is configured yet."
exit 0"#
        ),
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let said = text(&out.stdout);
    assert!(
        said.contains("VIctory starts under Wine: victory 0.1.0"),
        "{said}"
    );
    assert!(said.contains("NOTE: game_dir is not set"), "{said}");
    assert_eq!(text(&out.stderr), "");
}

/// A configuration VIctory refuses is the user's file to correct, not a
/// setup that failed: `config` exits 1 for a game_dir that is set and is
/// not there, and the setup's last step still passes, showing the report
/// and saying what to change. (Before, the setup required `config` to
/// succeed and died with "VIctory did not start under Wine".)
#[test]
fn check_setup_reports_a_refused_configuration_and_still_passes() {
    let out = check_setup(
        "check-setup-refused-config",
        &format!(
            r#"{STARTS}
echo "game_dir    : D:\\victory-data\\game  (does not exist)"
echo "MISSING: game_dir D:\\victory-data\\game is not a directory"
echo "Error: the configured game_dir D:\\victory-data\\game is not a directory" >&2
exit 1"#
        ),
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let said = text(&out.stdout);
    assert!(said.contains("VIctory starts under Wine"), "{said}");
    assert!(said.contains("MISSING: game_dir"), "{said}");
    assert!(said.contains("Error: the configured game_dir"), "{said}");
    let advice = text(&out.stderr);
    assert!(!advice.contains("did not start"), "{advice}");
    assert!(
        advice.contains("its configuration cannot be used as it is"),
        "{advice}"
    );
    assert!(advice.contains("remove the game_dir line"), "{advice}");
}

/// Where VIctory does not start, the step fails and shows what was said.
#[test]
fn check_setup_fails_when_victory_does_not_start() {
    for (name, wine, shown) in [
        (
            "check-setup-no-start",
            r#"echo "wine: could not load kernel32.dll" >&2
exit 1"#,
            "wine: could not load kernel32.dll",
        ),
        // Status 0 without the version is no start either.
        ("check-setup-no-version", "echo nothing", "nothing"),
    ] {
        let out = check_setup(name, wine);
        assert_eq!(out.status.code(), Some(1), "{name}: {}", text(&out.stdout));
        assert_eq!(text(&out.stdout), "", "{name}");
        let said = text(&out.stderr);
        assert!(
            said.contains("VIctory did not start under Wine"),
            "{name}: {said}"
        );
        assert!(said.contains(shown), "{name}: {said}");
    }
}

/// setup.sh ends on that step, and no longer on `config` succeeding; the
/// file it copies for a new setup configures no game.
#[test]
fn setup_ends_on_the_check_that_needs_no_game() {
    let setup = fs::read_to_string(root().join("setup.sh")).unwrap();
    let last_step = setup
        .split_once("# 7. VIctory itself.")
        .expect("setup.sh has its seventh step")
        .1;
    assert!(
        last_step.contains("\ntools/macos/check-setup.sh || die "),
        "{last_step}"
    );
    assert!(!last_step.contains("victory.sh config"), "{last_step}");
    assert!(
        last_step.contains("cp victory.example.toml victory.toml"),
        "{last_step}"
    );
    let example = fs::read_to_string(root().join("victory.example.toml")).unwrap();
    let set: Vec<&str> = example
        .lines()
        .filter(|l| l.trim_start().starts_with("game_dir"))
        .collect();
    assert_eq!(set, [""; 0], "the example sets no game_dir");
}

/// The variable the launcher sets is the one the configuration reads.
#[test]
fn victory_sh_sets_the_variable_the_config_reads() {
    let config = fs::read_to_string(root().join("crates/victory-core/src/config.rs")).unwrap();
    assert!(config.contains(r#"env_path("VICTORY_DATA_DIR")"#));
    let launcher = fs::read_to_string(root().join("tools/macos/victory.sh")).unwrap();
    assert!(launcher.contains(r#"export VICTORY_DATA_DIR="Z:"#));
}
