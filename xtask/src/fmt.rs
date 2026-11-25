//! Formatting for the fork's own files only.
//!
//! Upstream does not keep its tree rustfmt-clean, so a file that exists
//! upstream keeps upstream's layout here: reformatting it would turn every
//! touched line into a conflict at the next merge.
//!
//! A file is upstream's when the newest upstream commits merged here have
//! it. Those commits are found two ways, and both count: the merge base of
//! `HEAD` and the remote-tracking ref `upstream/main`, and the second
//! parents of the merge commits on `HEAD`'s first-parent history. The second way does not need
//! the ref to be there or to be current: a ref older than the last merge
//! has an older merge base, and the files upstream added since must not
//! look like the fork's. Every other Rust file is the fork's own.
//!
//! A merge commit can also bring in a branch of the fork, whose files are
//! not upstream's. Such a commit is known by having this very file, which
//! upstream does not have, and is left out.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};

/// The remote-tracking ref whose history says which files are upstream's.
const UPSTREAM: &str = "refs/remotes/upstream/main";

/// A file only the fork has (this one): a commit that has it is the
/// fork's, whatever merged it.
const FORK_ONLY: &str = "xtask/src/fmt.rs";

/// The Rust files to format, or why they cannot be told apart.
#[derive(Debug, PartialEq, Eq)]
pub enum Owned {
    /// Fork-owned Rust files, as paths from the root with `/`, sorted.
    Files(Vec<String>),
    /// There is nothing to compare with; the text says what is missing.
    Unknown(String),
}

/// Runs git in `root`; its output when it succeeded.
fn git(root: &Path, args: &[&str]) -> Result<Option<Vec<u8>>> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .stderr(Stdio::null())
        .output()
        .context("running git")?;
    Ok(out.status.success().then_some(out.stdout))
}

/// The NUL-separated names in git's `-z` output.
fn names(out: &[u8]) -> impl Iterator<Item = String> + '_ {
    out.split(|b| *b == 0)
        .filter(|n| !n.is_empty())
        .map(|n| String::from_utf8_lossy(n).into_owned())
}

/// Whether git succeeds in `root`.
fn git_ok(root: &Path, args: &[&str]) -> Result<bool> {
    Ok(git(root, args)?.is_some())
}

/// The lines of git's output.
fn lines(out: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(out)
        .lines()
        .map(|l| l.trim().to_owned())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Whether the remote-tracking ref is here.
fn has_ref(root: &Path) -> Result<bool> {
    git_ok(root, &["rev-parse", "--verify", "--quiet", UPSTREAM])
}

/// The newest upstream commits merged into `HEAD` (one, unless upstream's
/// history forks), or why none is known.
fn merged_upstream(root: &Path) -> Result<std::result::Result<Vec<String>, String>> {
    let base = match has_ref(root)? {
        true => git(root, &["merge-base", "HEAD", UPSTREAM])?.and_then(|b| lines(&b).pop()),
        false => None,
    };
    // The commits that merges brought in: every parent but the first.
    let merges = git(
        root,
        &[
            "rev-list",
            "--first-parent",
            "--merges",
            "--parents",
            "HEAD",
        ],
    )?
    .unwrap_or_default();
    let brought: Vec<String> = lines(&merges)
        .iter()
        .flat_map(|l| l.split_whitespace().skip(2).map(str::to_owned))
        .collect();
    // Not the fork's own branches: a commit with this command is no
    // upstream commit.
    let mut merged = Vec::new();
    for commit in brought {
        if !git_ok(root, &["cat-file", "-e", &format!("{commit}:{FORK_ONLY}")])? {
            merged.push(commit);
        }
    }
    merged.extend(base);
    if merged.is_empty() {
        return Ok(Err(match has_ref(root)? {
            false => "there is no upstream/main here (a plain clone has no `upstream` remote) \
                      and no merge of upstream in HEAD's history, so the fork's own files cannot be \
                      told from upstream's.\nTo check formatting: git remote add upstream \
                      https://github.com/Marvisak/VIctory.git && git fetch upstream"
                .into(),
            true => "HEAD and upstream/main have no common commit here (a shallow clone?) and \
                     HEAD's history has no merge of upstream, so the fork's own files cannot be told \
                     from upstream's."
                .into(),
        }));
    }
    // Of these, the ones no other one has in its history.
    let mut args = vec!["merge-base", "--independent"];
    args.extend(merged.iter().map(String::as_str));
    let newest = git(root, &args)?.context("git cannot compare the merged commits")?;
    Ok(Ok(lines(&newest)))
}

/// The Rust files under `root` that are not upstream's (see the module's
/// text): tracked ones and new ones that are not ignored.
pub fn fork_owned(root: &Path) -> Result<Owned> {
    let commits = match merged_upstream(root)? {
        Ok(commits) => commits,
        Err(why) => return Ok(Owned::Unknown(why)),
    };
    let mut upstream = HashSet::new();
    for commit in &commits {
        let tree = git(root, &["ls-tree", "-r", "-z", "--name-only", commit])?
            .with_context(|| format!("git cannot list the files of {commit}"))?;
        upstream.extend(names(&tree));
    }
    let here = git(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            "*.rs",
        ],
    )?
    .context("git cannot list the files here")?;
    let mut files: Vec<String> = names(&here)
        .filter(|f| !upstream.contains(f) && root.join(f).is_file())
        .collect();
    files.sort();
    files.dedup();
    Ok(Owned::Files(files))
}

/// The edition the workspace's manifest names; rustfmt does not read it.
fn edition(root: &Path) -> String {
    std::fs::read_to_string(root.join("Cargo.toml"))
        .ok()
        .and_then(|manifest| {
            manifest.lines().find_map(|line| {
                let value = line.trim().strip_prefix("edition")?.trim_start();
                let value = value.strip_prefix('=')?.trim();
                Some(value.trim_matches('"').to_owned())
            })
        })
        .unwrap_or_else(|| "2024".into())
}

/// `source` as rustfmt lays it out. It is handed over on standard input,
/// which keeps rustfmt to this one file: given a path it would also format
/// the modules the file declares, and those may be upstream's.
fn formatted(root: &Path, edition: &str, file: &str, source: &[u8]) -> Result<Vec<u8>> {
    let mut child = Command::new("rustfmt")
        .args(["--edition", edition, "--emit", "stdout"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running rustfmt (rustup component add rustfmt)")?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let source = source.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&source));
    let out = child.wait_with_output().context("running rustfmt")?;
    let _ = writer.join();
    ensure!(
        out.status.success(),
        "rustfmt cannot format {file}:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(out.stdout)
}

/// The files among `files` that rustfmt would change; with `write`, they
/// are changed.
pub fn unformatted(root: &Path, files: &[String], write: bool) -> Result<Vec<String>> {
    let edition = edition(root);
    let mut changed = Vec::new();
    for file in files {
        let path = root.join(file);
        let source = std::fs::read(&path).with_context(|| format!("reading {file}"))?;
        let wanted = formatted(root, &edition, file, &source)?;
        if wanted != source {
            if write {
                std::fs::write(&path, wanted).with_context(|| format!("writing {file}"))?;
            }
            changed.push(file.clone());
        }
    }
    Ok(changed)
}

/// `cargo xtask fmt` (`write`) and `cargo xtask fmt-check`.
pub fn run(root: &Path, write: bool) -> Result<()> {
    let files = match fork_owned(root)? {
        Owned::Files(files) => files,
        Owned::Unknown(why) => {
            println!("fmt: nothing checked: {why}");
            return Ok(());
        }
    };
    let changed = unformatted(root, &files, write)?;
    let count = files.len();
    if write {
        for file in &changed {
            println!("formatted {file}");
        }
        println!(
            "fmt: {count} fork-owned files, {} reformatted; files that exist upstream are left as they are",
            changed.len()
        );
        return Ok(());
    }
    if changed.is_empty() {
        println!(
            "fmt: {count} fork-owned files are formatted; files that exist upstream are not checked"
        );
        return Ok(());
    }
    for file in &changed {
        eprintln!("not formatted: {file}");
    }
    bail!(
        "{} of {count} fork-owned files are not formatted: run `cargo xtask fmt`",
        changed.len()
    )
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::{Owned, fork_owned, run, unformatted};

    /// Runs git in `dir`, with an identity and without the user's hooks,
    /// so that commits work anywhere.
    #[cfg(unix)]
    fn git(dir: &std::path::Path, args: &[&str]) {
        let st = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
            ])
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?}");
    }

    /// An empty folder of this test's own, outside the repository.
    #[cfg(unix)]
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("victory-xtask-fmt-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Layout rustfmt would change.
    #[cfg(unix)]
    const UGLY: &str = "fn  main( ){let x=1;\n    let _ = x ;}\n";

    /// A repository whose `upstream/main` holds `upstream.rs` and
    /// `shared.rs`, both badly laid out, and whose own commit on top
    /// changes `shared.rs` and adds `fork.rs` with the text `fork`.
    #[cfg(unix)]
    fn repository(name: &str, fork: &str) -> std::path::PathBuf {
        let dir = scratch(name);
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("upstream.rs"), UGLY).unwrap();
        std::fs::write(dir.join("shared.rs"), UGLY).unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-q", "-m", "upstream"]);
        git(&dir, &["update-ref", "refs/remotes/upstream/main", "HEAD"]);
        std::fs::write(dir.join("shared.rs"), format!("{UGLY}fn  more( ){{}}\n")).unwrap();
        std::fs::write(dir.join("fork.rs"), fork).unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-q", "-m", "the fork"]);
        dir
    }

    /// Only the file upstream does not have is the fork's: not upstream's
    /// own file, and not the one the fork changed. A new file that is not
    /// committed yet counts, an ignored one and one that is not Rust do
    /// not.
    #[test]
    #[cfg(unix)]
    fn only_files_missing_upstream_are_fork_owned() {
        let dir = repository("owned", "fn main() {}\n");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/new.rs"), UGLY).unwrap();
        std::fs::write(dir.join("notes.txt"), "text").unwrap();
        std::fs::write(dir.join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(dir.join("ignored.rs"), UGLY).unwrap();
        let owned = fork_owned(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            owned.unwrap(),
            Owned::Files(vec!["fork.rs".into(), "sub/new.rs".into()])
        );
    }

    /// Upstream's layout passes the check, in its own file and in the one
    /// the fork changed, as long as the fork's own file is formatted.
    #[test]
    #[cfg(unix)]
    fn upstream_layout_passes_the_check() {
        let dir = repository("passes", "fn main() {}\n");
        let checked = run(&dir, false);
        let _ = std::fs::remove_dir_all(&dir);
        checked.unwrap();
    }

    /// A badly laid out file of the fork fails the check and is named;
    /// `fmt` formats it and nothing else, and then the check passes.
    #[test]
    #[cfg(unix)]
    fn a_fork_file_is_checked_and_formatted() {
        let dir = repository("fork-file", UGLY);
        let Owned::Files(files) = fork_owned(&dir).unwrap() else {
            panic!("the repository has an upstream/main");
        };
        let named = unformatted(&dir, &files, false).unwrap();
        let failed = run(&dir, false);
        let untouched = std::fs::read_to_string(dir.join("fork.rs")).unwrap();
        let written = run(&dir, true);
        let fork = std::fs::read_to_string(dir.join("fork.rs")).unwrap();
        let upstream = std::fs::read_to_string(dir.join("upstream.rs")).unwrap();
        let shared = std::fs::read_to_string(dir.join("shared.rs")).unwrap();
        let after = run(&dir, false);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(named, ["fork.rs"]);
        let failed = failed.unwrap_err().to_string();
        assert!(failed.contains("cargo xtask fmt"), "{failed}");
        assert_eq!(untouched, UGLY, "the check wrote to the file");
        written.unwrap();
        assert_eq!(fork, "fn main() {\n    let x = 1;\n    let _ = x;\n}\n");
        assert_eq!(upstream, UGLY, "upstream's file was reformatted");
        assert!(shared.starts_with(UGLY), "the shared file was reformatted");
        after.unwrap();
    }

    /// A file is formatted alone: the module it declares is upstream's and
    /// keeps its layout.
    #[test]
    #[cfg(unix)]
    fn a_declared_module_is_not_followed() {
        let dir = repository("module", "mod  upstream ;\n");
        let written = run(&dir, true);
        let fork = std::fs::read_to_string(dir.join("fork.rs")).unwrap();
        let upstream = std::fs::read_to_string(dir.join("upstream.rs")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        written.unwrap();
        assert_eq!(fork, "mod upstream;\n");
        assert_eq!(upstream, UGLY);
    }

    /// Upstream goes on from the commit `upstream/main` names: a branch
    /// `newer` with `newer.rs`, badly laid out, which the ref does not
    /// follow. The fork merges it with a merge commit.
    #[cfg(unix)]
    fn merge_newer_upstream(dir: &std::path::Path) {
        git(
            dir,
            &[
                "checkout",
                "-q",
                "-b",
                "newer",
                "refs/remotes/upstream/main",
            ],
        );
        std::fs::write(dir.join("newer.rs"), UGLY).unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", "upstream goes on"]);
        git(dir, &["checkout", "-q", "-"]);
        git(
            dir,
            &["merge", "-q", "--no-ff", "-m", "merge upstream", "newer"],
        );
    }

    /// `upstream/main` is older than the upstream commit last merged (the
    /// remote was not fetched), or is gone: the file upstream added since
    /// is still upstream's, by the merge commit, and `fmt` leaves it alone.
    /// Before, it counted as the fork's own and was reformatted.
    #[test]
    #[cfg(unix)]
    fn a_stale_or_missing_upstream_ref_keeps_merged_files_upstreams() {
        let dir = repository("stale", "fn main() {}\n");
        merge_newer_upstream(&dir);
        let stale = fork_owned(&dir);
        let written = run(&dir, true);
        let newer = std::fs::read_to_string(dir.join("newer.rs")).unwrap();
        git(&dir, &["update-ref", "-d", "refs/remotes/upstream/main"]);
        let missing = fork_owned(&dir);
        let checked = run(&dir, false);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(stale.unwrap(), Owned::Files(vec!["fork.rs".into()]));
        written.unwrap();
        assert_eq!(newer, UGLY, "upstream's newer file was reformatted");
        assert_eq!(missing.unwrap(), Owned::Files(vec!["fork.rs".into()]));
        checked.unwrap();
    }

    /// A merge commit that brings in a branch of the fork's own (it has
    /// this command's file) says nothing about upstream: the branch's
    /// files stay the fork's, with the ref and without it.
    #[test]
    #[cfg(unix)]
    fn a_merged_branch_of_the_fork_stays_the_forks() {
        let dir = repository("own-branch", "fn main() {}\n");
        merge_newer_upstream(&dir);
        std::fs::create_dir_all(dir.join("xtask/src")).unwrap();
        std::fs::write(dir.join("xtask/src/fmt.rs"), "fn main() {}\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-q", "-m", "the fork's format command"]);
        git(&dir, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.join("feature.rs"), UGLY).unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-q", "-m", "a feature of the fork"]);
        git(&dir, &["checkout", "-q", "-"]);
        git(
            &dir,
            &[
                "merge",
                "-q",
                "--no-ff",
                "-m",
                "merge the feature",
                "feature",
            ],
        );
        let with = fork_owned(&dir);
        git(&dir, &["update-ref", "-d", "refs/remotes/upstream/main"]);
        let without = fork_owned(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        let own: Vec<String> = ["feature.rs", "fork.rs", "xtask/src/fmt.rs"]
            .map(Into::into)
            .into();
        assert_eq!(with.unwrap(), Owned::Files(own.clone()));
        assert_eq!(without.unwrap(), Owned::Files(own));
    }

    /// Without an upstream to compare with (a plain clone) nothing is
    /// checked and nothing fails, whatever the layout; the reason names
    /// the missing remote.
    #[test]
    #[cfg(unix)]
    fn without_upstream_nothing_is_checked() {
        let dir = scratch("plain");
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("fork.rs"), UGLY).unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-q", "-m", "a plain clone"]);
        let owned = fork_owned(&dir);
        let checked = run(&dir, false);
        let written = run(&dir, true);
        let fork = std::fs::read_to_string(dir.join("fork.rs")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let Owned::Unknown(why) = owned.unwrap() else {
            panic!("files were listed without an upstream");
        };
        assert!(why.contains("git remote add upstream"), "{why}");
        checked.unwrap();
        written.unwrap();
        assert_eq!(fork, UGLY, "a file was formatted without an upstream");
    }
}
