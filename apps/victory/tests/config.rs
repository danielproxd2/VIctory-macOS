//! `victory config` as scripts see it: its exit status.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Runs `victory config` with only these folders configured: no config
/// file (the working directory has none) and no other `VICTORY_*` paths.
fn config(game: &Path, cache: &Path, data: &Path) -> Output {
    config_of(Some(game), cache, data)
}

/// The same, with no game directory configured for `None`.
fn config_of(game: Option<&Path>, cache: &Path, data: &Path) -> Output {
    let elsewhere = std::env::temp_dir();
    assert!(
        !elsewhere.join("victory.toml").exists(),
        "a victory.toml in {} would be read",
        elsewhere.display()
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_victory"));
    command
        .arg("config")
        .current_dir(&elsewhere)
        .env_remove("VICTORY_CONFIG")
        .env_remove("VICTORY_TEST_TITLES")
        .env_remove("VICTORY_FIDELITYFX_DIR")
        .env_remove("VICTORY_GAME_DIR")
        .env("VICTORY_CACHE_DIR", cache)
        .env("VICTORY_DATA_DIR", data);
    if let Some(game) = game {
        command.env("VICTORY_GAME_DIR", game);
    }
    command.output().expect("victory starts")
}

/// A folder outside the repository that does not exist; `config` creates
/// nothing.
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
    // The game directory must be there; the others are made on first use.
    let game = tempfile::tempdir().unwrap();
    let out = config(game.path(), &outside("cache"), &outside("data"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stdout.contains("POLICY VIOLATION"), "{stdout}");
    assert!(!stdout.contains("MISSING"), "{stdout}");
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
}

/// A fresh setup has no game configured yet: `config` says so and
/// succeeds, which is what setup.sh and a first look at the setup rely on.
#[test]
fn config_with_no_game_dir_says_so_and_succeeds() {
    let out = config_of(None, &outside("cache"), &outside("data"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.contains("game_dir    : (not set)"), "{stdout}");
    assert!(stdout.contains("NOTE: game_dir is not set"), "{stdout}");
    assert!(!stdout.contains("MISSING"), "{stdout}");
    assert!(!stdout.contains("POLICY VIOLATION"), "{stdout}");
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
}

#[test]
fn config_with_a_game_dir_that_is_not_there_fails() {
    let game = outside("no-game-here");
    assert!(!game.exists(), "{} must not exist", game.display());
    let out = config(&game, &outside("cache"), &outside("data"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The report is still printed in full, then the status says it failed.
    assert!(stdout.contains("data_dir    :"), "{stdout}");
    assert!(stdout.contains("(does not exist)"), "{stdout}");
    assert!(!stdout.contains("POLICY VIOLATION"), "{stdout}");
    assert!(!stdout.contains("game_dir is not set"), "{stdout}");
    assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(stderr.contains("the configured game_dir"), "{stderr}");
    assert!(stderr.contains("is not a directory"), "{stderr}");
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
