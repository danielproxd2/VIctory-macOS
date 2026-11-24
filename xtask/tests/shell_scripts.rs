//! The shell scripts in tools/macos: that they parse, and the parts of
//! gnullvm-cc.sh that can run without a compiler, with stand-ins for the
//! programs it starts.
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
