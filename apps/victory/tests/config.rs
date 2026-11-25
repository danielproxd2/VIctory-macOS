//! `victory config` as scripts see it: its exit status.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Runs `victory config` with only these folders configured: no config
/// file (the working directory has none) and no other `VICTORY_*` paths.
fn config(game: &Path, cache: &Path, data: &Path) -> Output {
    let elsewhere = std::env::temp_dir();
    assert!(
        !elsewhere.join("victory.toml").exists(),
        "a victory.toml in {} would be read",
        elsewhere.display()
    );
    Command::new(env!("CARGO_BIN_EXE_victory"))
        .arg("config")
        .current_dir(&elsewhere)
        .env_remove("VICTORY_CONFIG")
        .env_remove("VICTORY_TEST_TITLES")
        .env("VICTORY_GAME_DIR", game)
        .env("VICTORY_CACHE_DIR", cache)
        .env("VICTORY_DATA_DIR", data)
        .output()
        .expect("victory starts")
}

/// A folder outside the repository; `config` creates nothing.
fn outside(name: &str) -> PathBuf {
    std::env::temp_dir().join("victory-config-test").join(name)
}

/// The source tree, as an absolute path of the host the test runs on.
fn repo_root() -> PathBuf {
    std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .expect("the tests run beside the source tree")
}

#[test]
fn config_with_paths_outside_the_repo_succeeds() {
    let out = config(&outside("game"), &outside("cache"), &outside("data"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("POLICY VIOLATION"), "{stdout}");
    assert_eq!(out.status.code(), Some(0), "{stdout}");
}

#[test]
fn config_with_a_cache_inside_the_repo_fails() {
    let inside = repo_root().join("cache").join("derived");
    let out = config(&outside("game"), &inside, &outside("data"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The report is still printed in full, then the status says it failed.
    assert!(stdout.contains("cache_dir   :"), "{stdout}");
    assert_eq!(stdout.matches("POLICY VIOLATION").count(), 1, "{stdout}");
    assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(
        stderr.contains("1 configured path(s) violate the asset policy"),
        "{stderr}"
    );
}

#[test]
fn config_counts_every_violating_path() {
    let root = repo_root();
    let out = config(&root.join("game"), &root.join("cache"), &root.join("data"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(stdout.matches("POLICY VIOLATION").count(), 3, "{stdout}");
    assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(stderr.contains("3 configured path(s)"), "{stderr}");
}
