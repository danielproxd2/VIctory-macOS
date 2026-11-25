//! Formatting for the fork's own files only.
//!
//! Upstream does not keep its tree rustfmt-clean, so a file that exists
//! upstream keeps upstream's layout here: reformatting it would turn every
//! touched line into a conflict at the next merge.
//!
//! Which upstream commit was last merged is written down, in
//! `xtask/upstream-merged.txt`, and not worked out from the shape of the
//! history. A file is upstream's when that commit has it, at its path or at
//! the path the fork moved it from; every other Rust file is the fork's
//! own. The commit is in the history of any full clone, so no remote and no
//! fetch is needed. The record is updated in the same commit as each merge
//! of upstream; where a remote-tracking ref `upstream/main` is present, a
//! record that was forgotten is noticed and nothing is formatted.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};

/// The file that holds the full hash of the upstream commit last merged.
pub const RECORD: &str = "xtask/upstream-merged.txt";

/// The remote-tracking ref that, where present, shows a forgotten record.
const UPSTREAM: &str = "refs/remotes/upstream/main";

/// The Rust files to format, or why they cannot be told apart.
#[derive(Debug, PartialEq, Eq)]
pub enum Owned {
    /// Fork-owned Rust files, as paths from the root with `/`, sorted.
    Files(Vec<String>),
    /// The recorded commit cannot be read here; the text says why.
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

/// The hash the record holds: its one line that is not a `#` comment, a
/// full commit hash.
fn recorded(root: &Path) -> Result<String> {
    let text = std::fs::read_to_string(root.join(RECORD)).with_context(|| {
        format!("reading {RECORD}, the record of the upstream commit last merged")
    })?;
    let mut hashes = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'));
    let hash = hashes.next().unwrap_or_default();
    ensure!(
        hashes.next().is_none()
            && matches!(hash.len(), 40 | 64)
            && hash.bytes().all(|b| b.is_ascii_hexdigit()),
        "{RECORD} must hold one line with the full hash of the upstream commit last merged \
         (and `#` comment lines)"
    );
    Ok(hash.to_ascii_lowercase())
}

/// The upstream commit last merged, as the record names it, once the record
/// is seen to hold: the commit is in the history of `HEAD` (or of the merge
/// under way), and what `HEAD` shares with `upstream/main`, where that ref
/// is here, is nothing newer. The inner error says why the commit cannot be
/// read in this clone; the outer one that the record is wrong.
pub fn merged_upstream(root: &Path) -> Result<std::result::Result<String, String>> {
    let commit = recorded(root)?;
    if !git_ok(root, &["cat-file", "-e", &format!("{commit}^{{commit}}")])? {
        return Ok(Err(format!(
            "the upstream commit last merged ({commit}, from {RECORD}) is not in this clone (a \
             shallow clone?), so the fork's own files cannot be told from upstream's.\nTo get \
             it: git fetch --unshallow"
        )));
    }
    let within = |tip: &str| git_ok(root, &["merge-base", "--is-ancestor", &commit, tip]);
    if !within("HEAD")? && !within("MERGE_HEAD")? {
        let shallow = git(root, &["rev-parse", "--is-shallow-repository"])?
            .is_some_and(|out| lines(&out) == ["true"]);
        if shallow {
            return Ok(Err(format!(
                "this clone is shallow, so it cannot be seen whether the upstream commit last \
                 merged ({commit}, from {RECORD}) is in HEAD's history.\nTo get the history: \
                 git fetch --unshallow"
            )));
        }
        bail!(
            "{RECORD} names {commit}, which is not in HEAD's history: it must hold the full hash \
             of the upstream commit last merged into this branch"
        );
    }
    if git_ok(root, &["rev-parse", "--verify", "--quiet", UPSTREAM])? {
        let bases = git(root, &["merge-base", "--all", "HEAD", UPSTREAM])?.unwrap_or_default();
        for base in lines(&bases) {
            ensure!(
                git_ok(root, &["merge-base", "--is-ancestor", &base, &commit])?,
                "{RECORD} is out of date: it names {commit}, but HEAD already has upstream's \
                 {base} (what it shares with upstream/main). Update {RECORD} to the full hash of \
                 the upstream commit last merged; it is updated in the same commit as each \
                 upstream merge"
            );
        }
    }
    Ok(Ok(commit))
}

/// The Rust files here that are upstream's under another name: `commit`
/// has a file that is gone here, and a file here that `commit` lacks is at
/// least half the same (git's rename detection, without its limit on the
/// number of candidates). Files that are new and not added yet are not
/// seen by git and so are not found.
fn renamed_from(root: &Path, commit: &str) -> Result<Vec<String>> {
    let out = git(
        root,
        &[
            "diff",
            "--find-renames=50%",
            "-l0",
            "--diff-filter=R",
            "--name-status",
            "-z",
            commit,
            "--",
            "*.rs",
        ],
    )?
    .with_context(|| format!("git cannot compare the files here with {commit}"))?;
    // Each rename is three fields: R and the likeness, the old name, the new.
    let fields: Vec<String> = names(&out).collect();
    Ok(fields.chunks_exact(3).map(|r| r[2].clone()).collect())
}

/// The Rust files under `root` that are not upstream's (see the module's
/// text): tracked ones and new ones that are not ignored.
pub fn fork_owned(root: &Path) -> Result<Owned> {
    let commit = match merged_upstream(root)? {
        Ok(commit) => commit,
        Err(why) => return Ok(Owned::Unknown(why)),
    };
    let tree = git(root, &["ls-tree", "-r", "-z", "--name-only", &commit])?
        .with_context(|| format!("git cannot list the files of {commit}"))?;
    let mut upstream: HashSet<String> = names(&tree).collect();
    upstream.extend(renamed_from(root, &commit)?);
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
        // Nothing is formatted on a guess: every file would look like the
        // fork's own.
        Owned::Unknown(why) if write => bail!("nothing formatted: {why}"),
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
    use super::merged_upstream;
    #[cfg(unix)]
    use super::{Owned, RECORD, fork_owned, run, unformatted};

    /// The record of the real repository holds: the commit it names is in
    /// `HEAD`'s history, and where `upstream/main` is here, nothing newer
    /// of upstream's has been merged. A merge of upstream that leaves the
    /// record as it was fails here. In a clone without the commit (a
    /// shallow one, as CI's) there is nothing to compare.
    #[test]
    fn the_record_names_the_upstream_commit_last_merged() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap();
        // Under Wine there is no git to start, as for the policy test of
        // the repository: the same line and the same switch.
        use crate::policy::tests::{no_git_under_wine, under_wine, unmet};
        use std::io::Write;
        let git = std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(drop)
            .map_err(|e| e.kind());
        if no_git_under_wine(under_wine(), git) {
            let var = "VICTORY_REQUIRE_POLICY";
            let thread = std::thread::current();
            match unmet(
                var,
                std::env::var(var).ok().as_deref(),
                "Wine starts no git: the record of the upstream commit last merged is not checked",
                thread.name().unwrap_or("?"),
            ) {
                Ok(line) => {
                    let _ = writeln!(std::io::stderr(), "{line}");
                }
                Err(why) => panic!("{why}"),
            }
            return;
        }
        match merged_upstream(root) {
            Ok(Ok(_)) => {}
            Ok(Err(why)) => println!("skipped: {why}"),
            Err(wrong) => panic!("{wrong:#}"),
        }
    }

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

    /// Writes the record in `dir`: the full hash of `commit` under a
    /// comment line.
    #[cfg(unix)]
    fn record(dir: &std::path::Path, commit: &str) {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "--verify", commit])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git rev-parse {commit}");
        let hash = String::from_utf8(out.stdout).unwrap();
        std::fs::create_dir_all(dir.join(RECORD).parent().unwrap()).unwrap();
        std::fs::write(dir.join(RECORD), format!("# upstream, last merged\n{hash}")).unwrap();
    }

    /// A repository whose first commit is upstream's (branch `upstream`):
    /// `upstream.rs` and `shared.rs`, both badly laid out. The fork's own
    /// commit on top records that commit, changes `shared.rs` and adds
    /// `fork.rs` with the text `fork`. There is no `upstream/main`.
    #[cfg(unix)]
    fn repository(name: &str, fork: &str) -> std::path::PathBuf {
        let dir = scratch(name);
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("upstream.rs"), UGLY).unwrap();
        std::fs::write(dir.join("shared.rs"), UGLY).unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-q", "-m", "upstream"]);
        git(&dir, &["branch", "upstream"]);
        record(&dir, "upstream");
        std::fs::write(dir.join("shared.rs"), format!("{UGLY}fn  more( ){{}}\n")).unwrap();
        std::fs::write(dir.join("fork.rs"), fork).unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-q", "-m", "the fork"]);
        dir
    }

    /// Makes `upstream/main` name what the branch `upstream` names, as a
    /// fetch of the remote would.
    #[cfg(unix)]
    fn fetch(dir: &std::path::Path) {
        git(
            dir,
            &["update-ref", "refs/remotes/upstream/main", "upstream"],
        );
    }

    /// Only the file the recorded commit does not have is the fork's: not
    /// upstream's own file, and not the one the fork changed. A new file
    /// that is not committed yet counts, an ignored one and one that is
    /// not Rust do not. The record is right, and an `upstream/main` that
    /// agrees with it changes nothing.
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
        fetch(&dir);
        let fetched = fork_owned(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        let own = Owned::Files(vec!["fork.rs".into(), "sub/new.rs".into()]);
        assert_eq!(owned.unwrap(), own);
        assert_eq!(fetched.unwrap(), own);
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
    /// `fmt` formats it and nothing else, and then the check passes. No
    /// remote and no `upstream/main` is needed: the record is enough.
    #[test]
    #[cfg(unix)]
    fn a_fork_file_is_checked_and_formatted_without_an_upstream_remote() {
        let dir = repository("fork-file", UGLY);
        let Owned::Files(files) = fork_owned(&dir).unwrap() else {
            panic!("the recorded commit is in the repository");
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

    /// Upstream goes on: its branch gets `newer.rs`, badly laid out, and
    /// the fork merges it with a merge commit. The record is not touched.
    #[cfg(unix)]
    fn merge_newer_upstream(dir: &std::path::Path) {
        git(dir, &["checkout", "-q", "upstream"]);
        std::fs::write(dir.join("newer.rs"), UGLY).unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", "upstream goes on"]);
        git(dir, &["checkout", "-q", "-"]);
        git(
            dir,
            &["merge", "-q", "--no-ff", "-m", "merge upstream", "upstream"],
        );
    }

    /// Upstream was merged and the record still names the commit before:
    /// with `upstream/main` here that is noticed, the check fails saying
    /// to update the record, and `fmt` formats nothing (the file upstream
    /// added would look like the fork's own). With the record updated the
    /// file is upstream's and is left alone.
    #[test]
    #[cfg(unix)]
    fn a_record_older_than_the_merged_upstream_is_refused() {
        let dir = repository("stale", "fn main() {}\n");
        merge_newer_upstream(&dir);
        fetch(&dir);
        let stale = fork_owned(&dir);
        let checked = run(&dir, false);
        let written = run(&dir, true);
        let newer = std::fs::read_to_string(dir.join("newer.rs")).unwrap();
        record(&dir, "upstream");
        let current = fork_owned(&dir);
        let rewritten = run(&dir, true);
        let after = std::fs::read_to_string(dir.join("newer.rs")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let stale = format!("{:#}", stale.unwrap_err());
        assert!(stale.contains("out of date"), "{stale}");
        assert!(stale.contains(&format!("Update {RECORD}")), "{stale}");
        checked.unwrap_err();
        written.unwrap_err();
        assert_eq!(newer, UGLY, "a file was formatted on an old record");
        assert_eq!(current.unwrap(), Owned::Files(vec!["fork.rs".into()]));
        rewritten.unwrap();
        assert_eq!(after, UGLY, "upstream's newer file was reformatted");
    }

    /// Merge commits that bring in branches of the fork's own say nothing
    /// about upstream, whatever the branches hold and whenever they were
    /// cut: their files stay the fork's and upstream's stay upstream's,
    /// with `upstream/main` and without it.
    #[test]
    #[cfg(unix)]
    fn merged_branches_of_the_fork_change_nothing() {
        let dir = repository("own-branch", "fn main() {}\n");
        // A branch cut before the merge of upstream, merged after it.
        git(&dir, &["checkout", "-q", "-b", "early"]);
        std::fs::write(dir.join("early.rs"), UGLY).unwrap();
        git(&dir, &["add", "-A"]);
        git(
            &dir,
            &["commit", "-q", "-m", "an early feature of the fork"],
        );
        git(&dir, &["checkout", "-q", "-"]);
        merge_newer_upstream(&dir);
        record(&dir, "upstream");
        git(&dir, &["commit", "-q", "-a", "--amend", "--no-edit"]);
        git(
            &dir,
            &["merge", "-q", "--no-ff", "-m", "merge early", "early"],
        );
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
        let without = fork_owned(&dir);
        fetch(&dir);
        let with = fork_owned(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        let own: Vec<String> = ["early.rs", "feature.rs", "fork.rs"].map(Into::into).into();
        assert_eq!(without.unwrap(), Owned::Files(own.clone()));
        assert_eq!(with.unwrap(), Owned::Files(own));
    }

    /// Enough lines that a file moved and then changed a little is still
    /// more than half the same; laid out as rustfmt would not.
    #[cfg(unix)]
    fn long_ugly() -> String {
        (0..20)
            .map(|i| format!("fn  f{i}( ){{let x={i};\n    let _ = x ;}}\n"))
            .collect()
    }

    /// An upstream file the fork moves (and changes a little) is still
    /// upstream's under its new name, committed or only added: it is not
    /// checked and `fmt` does not reformat it. A new file with other text
    /// beside it is the fork's.
    #[test]
    #[cfg(unix)]
    fn a_renamed_upstream_file_stays_upstreams() {
        let dir = scratch("renamed");
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("list.rs"), long_ugly()).unwrap();
        std::fs::write(dir.join("other.rs"), long_ugly().replace("f", "g")).unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-q", "-m", "upstream"]);
        record(&dir, "HEAD");
        std::fs::create_dir_all(dir.join("moved")).unwrap();
        git(&dir, &["mv", "list.rs", "moved/list2.rs"]);
        let moved = format!("{}fn  more( ){{}}\n", long_ugly());
        std::fs::write(dir.join("moved/list2.rs"), &moved).unwrap();
        std::fs::write(dir.join("fork.rs"), "fn main() {}\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-q", "-m", "the fork moves a file"]);
        let committed = fork_owned(&dir);
        git(&dir, &["mv", "other.rs", "moved/other2.rs"]);
        let added = fork_owned(&dir);
        let written = run(&dir, true);
        let after = std::fs::read_to_string(dir.join("moved/list2.rs")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(committed.unwrap(), Owned::Files(vec!["fork.rs".into()]));
        assert_eq!(added.unwrap(), Owned::Files(vec!["fork.rs".into()]));
        written.unwrap();
        assert_eq!(after, moved, "the moved upstream file was reformatted");
    }

    /// The recorded commit is not in the clone (a shallow one): the check
    /// checks nothing and succeeds, and `fmt` refuses and formats nothing,
    /// since every file would look like the fork's own.
    #[test]
    #[cfg(unix)]
    fn without_the_recorded_commit_nothing_is_formatted() {
        let dir = repository("shallow", UGLY);
        let hash = "0123456789abcdef0123456789abcdef01234567";
        std::fs::write(dir.join(RECORD), format!("# not here\n{hash}\n")).unwrap();
        let owned = fork_owned(&dir);
        let checked = run(&dir, false);
        let written = run(&dir, true);
        let fork = std::fs::read_to_string(dir.join("fork.rs")).unwrap();
        let upstream = std::fs::read_to_string(dir.join("upstream.rs")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let Owned::Unknown(why) = owned.unwrap() else {
            panic!("files were listed without the recorded commit");
        };
        assert!(why.contains(hash) && why.contains("shallow"), "{why}");
        checked.unwrap();
        let refused = written.unwrap_err().to_string();
        assert!(refused.contains("nothing formatted"), "{refused}");
        assert_eq!(fork, UGLY, "a file was formatted on a guess");
        assert_eq!(upstream, UGLY, "upstream's file was reformatted");
    }

    /// A record that is missing, holds no full hash, or names a commit
    /// that is not in `HEAD`'s history is an error for both commands, and
    /// nothing is formatted.
    #[test]
    #[cfg(unix)]
    fn a_wrong_record_is_an_error() {
        let dir = repository("wrong", UGLY);
        git(&dir, &["branch", "fork"]);
        git(&dir, &["checkout", "-q", "--orphan", "elsewhere"]);
        git(&dir, &["commit", "-q", "-m", "not in the fork's history"]);
        git(&dir, &["checkout", "-q", "fork"]);
        record(&dir, "elsewhere");
        let foreign = run(&dir, true);
        std::fs::write(dir.join(RECORD), "# short\n13f91ca\n").unwrap();
        let short = run(&dir, true);
        std::fs::remove_file(dir.join(RECORD)).unwrap();
        let missing = (run(&dir, false), run(&dir, true));
        let fork = std::fs::read_to_string(dir.join("fork.rs")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let foreign = foreign.unwrap_err().to_string();
        assert!(foreign.contains("not in HEAD's history"), "{foreign}");
        let short = short.unwrap_err().to_string();
        assert!(short.contains("full hash"), "{short}");
        missing.0.unwrap_err();
        missing.1.unwrap_err();
        assert_eq!(fork, UGLY, "a file was formatted on a wrong record");
    }
}
