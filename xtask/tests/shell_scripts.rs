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

/// An empty folder for the test `name` outside every VIctory source tree,
/// by its real path: the scratch folders above are in the tree's target
/// folder, and where the launcher is started decides what it does.
fn outside(name: &str) -> PathBuf {
    let base = std::env::temp_dir().join("victory-xtask-tests");
    fs::create_dir_all(&base).unwrap();
    let dir = base.canonicalize().unwrap().join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    for above in dir.ancestors() {
        assert!(
            !above.join(".victory-root").exists() && above != root(),
            "{} is inside a source tree",
            dir.display()
        );
    }
    dir
}

/// `path` as Wine names it: it shows `/` as `Z:`.
fn windows(path: &Path) -> String {
    format!("Z:{}", path.display().to_string().replace('/', "\\"))
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

/// Every `VICTORY_REQUIRE_*` switch the Rust sources of the workspace and
/// of the fixtures read, sorted. The tests of a crate read a switch by its
/// name in a string, so every name in a source file counts.
fn require_switches() -> Vec<String> {
    fn collect(dir: &Path, found: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n != "target") {
                    collect(&path, found);
                }
            } else if path.extension().is_some_and(|e| e == "rs") {
                let source = fs::read_to_string(&path).unwrap();
                for (at, _) in source.match_indices("VICTORY_REQUIRE_") {
                    let name: String = source[at..]
                        .chars()
                        .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
                        .collect();
                    found.push(name);
                }
            }
        }
    }
    let mut found = Vec::new();
    for part in ["crates", "apps", "tests"] {
        collect(&root().join(part), &mut found);
    }
    found.sort();
    found.dedup();
    // A name cut off at a line's end, or the bare prefix, is no switch.
    found.retain(|name| name != "VICTORY_REQUIRE_");
    found
}

/// A switch that turns a skipped test into a failed one is of use only if
/// whoever runs the tests can find it and it reaches the tests: every one
/// the sources read is in test-wine.sh's list, is handed on by it, and is
/// described in docs/macos.md, with the proving command that sets the ones
/// a Mac can meet.
#[test]
fn test_wine_lists_and_hands_on_every_require_switch() {
    let switches = require_switches();
    for known in [
        "VICTORY_REQUIRE_GPU",
        "VICTORY_REQUIRE_DXIL",
        "VICTORY_REQUIRE_CLAMP",
        "VICTORY_REQUIRE_HLSL",
        "VICTORY_REQUIRE_NTFS",
        "VICTORY_REQUIRE_IDLE",
        "VICTORY_REQUIRE_PC_GPU",
    ] {
        assert!(
            switches.iter().any(|s| s == known),
            "{known} is not found in the sources any more: {switches:?}"
        );
    }
    let runner = fs::read_to_string(root().join("tools/macos/test-wine.sh")).unwrap();
    let doc = fs::read_to_string(root().join("docs/macos.md")).unwrap();
    let section = doc
        .split_once("## What a test run did not check")
        .expect("docs/macos.md has the section")
        .1;
    let section = section.split_once("\n## ").map_or(section, |(s, _)| s);
    for switch in &switches {
        assert!(
            runner.contains(&format!("#   {switch}=1 ")),
            "test-wine.sh does not list {switch}"
        );
        assert!(
            runner.contains(&format!("[ -z \"${{{switch}:-}}\" ] || export {switch}\n")),
            "test-wine.sh does not hand on {switch}"
        );
        assert!(
            section.contains(&format!("- `{switch}=1`: ")),
            "docs/macos.md does not describe {switch}"
        );
    }
    // The command a proving run on a Mac uses, in both places.
    for text in [runner.as_str(), section] {
        for part in [
            "VICTORY_REQUIRE_DXIL=1 VICTORY_REQUIRE_GPU=1 VICTORY_REQUIRE_CLAMP=1",
            "tools/macos/test-wine.sh --workspace --no-fail-fast",
        ] {
            assert!(text.contains(part), "{part} is missing");
        }
    }

    // And they arrive: with every switch set, the stand-in cargo that runs
    // the tests has each of them in its environment; with none set, none.
    let libdir = scratch("test-wine-switches-libdir");
    let dir = scratch("test-wine-switches");
    let log = dir.join("cargo.log");
    script(&dir.join("bin/wine"), "exit 0");
    script(
        &dir.join("bin/rustc"),
        &format!("echo '{}'", libdir.display()),
    );
    script(
        &dir.join("bin/cargo"),
        r#"env | sed -n 's/^\(VICTORY_REQUIRE_[A-Z0-9_]*\)=1$/has \1/p' >> "$STAND_IN_LOG""#,
    );
    for set in [true, false] {
        let _ = fs::remove_file(&log);
        let mut command = Command::new(root().join("tools/macos/test-wine.sh"));
        command
            .args(["-p", "victory-pe"])
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", dir.join("bin").display()),
            )
            .env("WINE", dir.join("bin/wine"))
            .env("STAND_IN_LOG", &log)
            .env_remove("WINEDLLOVERRIDES");
        for switch in &switches {
            if set {
                command.env(switch, "1");
            } else {
                command.env_remove(switch);
            }
        }
        let out = command.output().unwrap();
        assert!(out.status.success(), "{}", text(&out.stderr));
        let mut has = if log.exists() {
            logged(&log, "has ")
        } else {
            Vec::new()
        };
        has.sort();
        has.dedup();
        let expected = if set { switches.clone() } else { Vec::new() };
        assert_eq!(has, expected, "set: {set}");
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
    /// The directory the stand-in wine was started in.
    cwd: PathBuf,
    /// The stand-in wine's `VICTORY_CONFIG`, `<unset>` when it had none.
    config: String,
    /// How often the stand-in cargo was started.
    builds: usize,
}

/// The stand-in wine of the launcher's tests: it writes what it was started
/// with, and where, to the file `STAND_IN_LOG` names.
const WINE_LOG: &str = r#"{
    printf '%s\n' "data ${VICTORY_DATA_DIR-<unset>}"
    printf '%s\n' "rosetta ${ROSETTA_ADVERTISE_AVX-<unset>}"
    printf '%s\n' "cpu check ${VICTORY_SKIP_CPU_CHECK-<unset>}"
    printf '%s\n' "config ${VICTORY_CONFIG-<unset>}"
    printf '%s\n' "cwd $(pwd -P)"
    printf 'arg %s\n' "$@"
} >> "$STAND_IN_LOG""#;

/// Runs victory.sh with `VICTORY_DATA_DIR` and `VICTORY_CONFIG` as given.
/// `config` is the text of a config file in the test's folder; the script
/// is told about it by `config_name`, in which `{}` stands for the file's
/// macOS path. (Without `VICTORY_CONFIG` the script would read the
/// victory.toml of whoever runs the tests.) The launcher is started in a
/// folder outside the source tree, as a user starts it.
fn victory(name: &str, data_dir: Option<&str>, config: &str, config_name: &str) -> Launched {
    // Not the folder of the stand-ins, which `launch` empties.
    let dir = scratch(&format!("{name}-config"));
    let file = dir.join("victory config.toml");
    fs::write(&file, config).unwrap();
    let config_name = config_name
        .replace("{Z}", &windows(&file))
        .replace("{}", &file.display().to_string());
    let launched = launch(
        name,
        &outside(name),
        &["run", "--execute", "a title.exe"],
        Some(&config_name),
        data_dir,
    );
    let out = &launched.out;
    assert!(out.status.success(), "{name}: {}", text(&out.stderr));
    launched
}

/// Runs victory.sh in the directory `started_in` with `args`, with
/// stand-ins for cargo and wine in a home folder of the test's own, and
/// with `VICTORY_CONFIG` and `VICTORY_DATA_DIR` as given (`None`: not set).
/// Where the launcher starts Wine, it built once and started it once.
fn launch(
    name: &str,
    started_in: &Path,
    args: &[&str],
    config: Option<&str>,
    data_dir: Option<&str>,
) -> Launched {
    let dir = scratch(name);
    let log = dir.join("wine.log");
    let home = dir.join("home");
    fs::create_dir_all(dir.join("wine/lib/external/D3DMetal.framework")).unwrap();
    script(
        &dir.join("bin/cargo"),
        r#"echo "cargo $* in $(pwd -P)" >> "$STAND_IN_LOG""#,
    );
    script(&dir.join("wine/bin/wine"), WINE_LOG);
    let mut command = Command::new(root().join("tools/macos/victory.sh"));
    command
        .args(args)
        .current_dir(started_in)
        .env("CARGO", dir.join("bin/cargo"))
        .env("WINE", dir.join("wine/bin/wine"))
        .env("HOME", &home)
        .env("STAND_IN_LOG", &log)
        .env_remove("VICTORY_CONFIG")
        .env_remove("VICTORY_DATA_DIR")
        .env_remove("ROSETTA_ADVERTISE_AVX")
        .env_remove("VICTORY_SKIP_CPU_CHECK")
        .env_remove("WINEPREFIX");
    if let Some(config) = config {
        command.env("VICTORY_CONFIG", config);
    }
    if let Some(data_dir) = data_dir {
        command.env("VICTORY_DATA_DIR", data_dir);
    }
    let out = command.output().unwrap();
    let logged = |key: &str| {
        if log.exists() {
            logged(&log, key)
        } else {
            Vec::new()
        }
    };
    let data = logged("data ");
    if out.status.success() {
        // The build is the one thing done in the source tree.
        let built = logged("cargo ");
        assert_eq!(built.len(), 1, "{name}: built once");
        assert!(
            built[0].ends_with(&format!(" in {}", root().canonicalize().unwrap().display())),
            "{name}: {built:?}"
        );
        assert_eq!(data.len(), 1, "{name}: wine started once");
    }
    Launched {
        data_dir: data
            .first()
            .filter(|data| *data != "<unset>")
            .map(String::clone),
        cpu: [logged("rosetta ").concat(), logged("cpu check ").concat()],
        args: logged("arg "),
        home: windows(&home),
        cwd: PathBuf::from(logged("cwd ").concat()),
        config: logged("config ").concat(),
        builds: logged("cargo ").len(),
        out,
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

/// VIctory runs in the directory the launcher was started in, as it does on
/// Windows, where the user starts it in theirs: a path on the command line
/// is taken from there, by victory.exe itself, and it is the title's current
/// directory. (Before, the launcher changed into the repository first: a
/// relative path was looked for among the sources, and the title read and
/// wrote its relative paths there.)
#[test]
fn victory_sh_runs_in_the_directory_it_was_started_in() {
    let config = scratch("victory-sh-cwd-config").join("victory.toml");
    fs::write(&config, "").unwrap();
    let config = config.display().to_string();
    for (name, args) in [
        // A title by its file name alone, from its own folder.
        ("victory-sh-cwd-run", &["run", "--execute", "game.exe"][..]),
        ("victory-sh-cwd-bind", &["run", "sub folder/game.exe"][..]),
        ("victory-sh-cwd-inspect", &["inspect", "game.exe"][..]),
    ] {
        let here = outside(name);
        let launched = launch(name, &here, args, Some(&config), None);
        assert!(launched.out.status.success(), "{name}");
        assert_eq!(launched.cwd, here, "{name}");
        // The arguments are the user's, untouched.
        assert_eq!(launched.args[1..], *args, "{name}");
        assert_eq!(text(&launched.out.stderr), "", "{name}");
    }
}

/// The one directory a title never runs in is a VIctory source tree: what
/// it writes by a relative path would land among the sources. Started from
/// inside the tree, `run` starts the title in the title's own folder (what
/// a start by file name from that folder gives) and says so; the executable
/// is the one the path named from where the launcher was started.
#[test]
fn victory_sh_never_runs_a_title_inside_the_source_tree() {
    let config = scratch("victory-sh-tree-config").join("victory.toml");
    fs::write(&config, "").unwrap();
    let config = config.display().to_string();
    let game = outside("victory-sh-tree-game");
    fs::create_dir_all(game.join("the game")).unwrap();
    let exe = game.join("the game/game.exe");
    let exe_windows = windows(&exe);
    let relative = |from: &Path| {
        let ups = from.components().count() - 1;
        format!("{}{}", "../".repeat(ups), &exe.display().to_string()[1..])
    };
    let inside = root().canonicalize().unwrap().join("tools");
    // Every way of naming the executable from inside the tree, and the
    // options of `run` around it: those with a value do not hide it, and
    // what follows `--` is the title's and stays as it is.
    for (name, args) in [
        (
            "victory-sh-tree-absolute",
            vec!["run", "--execute", exe.to_str().unwrap()],
        ),
        (
            "victory-sh-tree-z",
            vec!["run", &exe_windows, "--execute", "--strict"],
        ),
        (
            "victory-sh-tree-relative",
            vec!["run", "--execute", &relative(&inside)],
        ),
        (
            "victory-sh-tree-options",
            vec![
                "run",
                "--console",
                "series-s",
                "--unimplemented",
                "zero",
                "--console=series-x",
                exe.to_str().unwrap(),
                "--",
                "title.exe",
                "run",
            ],
        ),
    ] {
        let launched = launch(name, &inside, &args, Some(&config), None);
        assert!(launched.out.status.success(), "{name}");
        assert_eq!(launched.cwd, game.join("the game"), "{name}");
        // Only the executable's argument changed: to its full path, which
        // names the same file from the title's folder.
        let expected: Vec<&str> = args
            .iter()
            .map(|a| {
                let is_exe = *a == exe.to_str().unwrap() || *a == relative(&inside);
                if is_exe { exe_windows.as_str() } else { a }
            })
            .collect();
        assert_eq!(launched.args[1..], expected, "{name}");
        let said = text(&launched.out.stderr);
        assert!(
            said.contains("the title runs in its own folder")
                && said.contains(&game.join("the game").display().to_string()),
            "{name}: {said}"
        );
    }
    // A command that runs no title stays where the user is.
    let launched = launch(
        "victory-sh-tree-inspect",
        &inside,
        &["inspect", "victory.sh"],
        Some(&config),
        None,
    );
    assert_eq!(launched.cwd, inside);
    assert_eq!(launched.args[1..], ["inspect", "victory.sh"]);
    assert_eq!(text(&launched.out.stderr), "");
    // `run` without an executable is victory.exe's to refuse.
    let launched = launch(
        "victory-sh-tree-no-exe",
        &inside,
        &["run", "--execute", "--", "game.exe"],
        Some(&config),
        None,
    );
    assert_eq!(launched.cwd, inside);
    assert_eq!(launched.args[1..], ["run", "--execute", "--", "game.exe"]);
}

/// Where the title's folder is inside a source tree as well, there is no
/// directory to run it in: nothing is built and nothing is started. A
/// source tree is this repository or any folder that holds its marker
/// file, as for victory_core::ensure_outside_repo.
#[test]
fn victory_sh_refuses_a_title_that_lies_inside_a_source_tree() {
    let config = scratch("victory-sh-refuse-config").join("victory.toml");
    fs::write(&config, "").unwrap();
    let config = config.display().to_string();
    let inside = root().canonicalize().unwrap().join("tools/macos");
    // Another copy of the sources, with a game dropped into it.
    let copy = outside("victory-sh-refuse-copy");
    fs::write(copy.join(".victory-root"), "").unwrap();
    fs::create_dir_all(copy.join("game")).unwrap();
    let in_copy = copy.join("game/game.exe");
    for (name, started_in, exe, shown) in [
        (
            "victory-sh-refuse-here",
            &inside,
            "victory.sh",
            inside.clone(),
        ),
        (
            "victory-sh-refuse-copy-from-tree",
            &inside,
            in_copy.to_str().unwrap(),
            copy.join("game"),
        ),
        (
            "victory-sh-refuse-copy-from-copy",
            &copy,
            "game/game.exe",
            copy.join("game"),
        ),
    ] {
        let launched = launch(
            name,
            started_in,
            &["run", "--execute", exe],
            Some(&config),
            None,
        );
        assert_eq!(launched.out.status.code(), Some(1), "{name}");
        assert_eq!(launched.args, [""; 0], "{name}: nothing is started");
        assert_eq!(launched.builds, 0, "{name}: nothing is built");
        let said = text(&launched.out.stderr);
        assert!(
            said.contains(&format!(
                "refusing to run a title in {}: it is inside a VIctory source tree",
                shown.display()
            )),
            "{name}: {said}"
        );
    }
    // A path macOS cannot follow to the title's folder, and a folder that
    // is not there: refused by name as well.
    for (name, exe, shown) in [
        (
            "victory-sh-refuse-drive",
            "D:\\games\\game.exe",
            "is not a path macOS can follow",
        ),
        (
            "victory-sh-refuse-absent",
            "/victory-no-such-folder/game.exe",
            "no folder at /victory-no-such-folder",
        ),
    ] {
        let launched = launch(
            name,
            &inside,
            &["run", "--execute", exe],
            Some(&config),
            None,
        );
        assert_eq!(launched.out.status.code(), Some(1), "{name}");
        assert_eq!(launched.args, [""; 0], "{name}: nothing is started");
        assert_eq!(launched.builds, 0, "{name}: nothing is built");
        let said = text(&launched.out.stderr);
        assert!(said.contains(shown), "{name}: {said}");
    }
}

/// The config file is the user's `VICTORY_CONFIG`, a relative name taken
/// from the directory the launcher was started in; else victory.toml in
/// that directory; else the repository's, where setup.sh puts it. What the
/// launcher found, and a relative name, reach victory.exe as full paths,
/// since it may be started in another directory.
#[test]
fn victory_sh_finds_the_config_file_from_the_users_directory() {
    let here = outside("victory-sh-config-here");
    fs::write(here.join("victory.toml"), "").unwrap();
    fs::write(here.join("mine.toml"), "").unwrap();
    let args = ["run", "game.exe"];
    let launched = launch("victory-sh-config-here", &here, &args, None, None);
    assert_eq!(launched.config, windows(&here.join("victory.toml")));
    let launched = launch(
        "victory-sh-config-relative",
        &here,
        &args,
        Some("mine.toml"),
        None,
    );
    assert_eq!(launched.config, windows(&here.join("mine.toml")));
    // A full path stays as the user wrote it.
    let named = here.join("mine.toml").display().to_string();
    let launched = launch("victory-sh-config-full", &here, &args, Some(&named), None);
    assert_eq!(launched.config, named);
    // No file in the user's directory: the repository's, if it has one
    // (it is git-ignored, and whoever runs the tests may have none).
    let empty = outside("victory-sh-config-none");
    let launched = launch("victory-sh-config-none", &empty, &args, None, None);
    let of_the_repository = root().canonicalize().unwrap().join("victory.toml");
    if of_the_repository.is_file() {
        assert_eq!(launched.config, windows(&of_the_repository));
    } else {
        assert_eq!(launched.config, "<unset>");
    }
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
            .current_dir(outside(&format!("victory-sh-exit-{code}")))
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
