//! Rule #1, statically: the repository only ever contains small text files
//! that we wrote. See docs/asset-policy.md.
//!
//! This is an allowlist, not a blocklist: a file passes only if its name is
//! on the list, it is not too big, it is text, and it does not smuggle data
//! inside text (long hex/base64 runs).

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

const MAX_FILE_SIZE: usize = 512 * 1024;
const MAX_ENCODED_RUN: usize = 512;

const ALLOWED_EXTENSIONS: &[&str] = &[
    "rs", "toml", "md", "txt", "lock", "yml", "yaml", "json", "hlsl", "hlsli", "ps1", "cmd", "sh",
];
const ALLOWED_NAMES: &[&str] = &[
    ".gitignore",
    ".gitattributes",
    ".editorconfig",
    ".victory-root",
    "pre-commit",
    "LICENSE",
    "LICENSE-MIT",
    "LICENSE-APACHE",
];

/// Paths explicitly exempted, with a reason. Keep this empty if at all possible.
const EXCEPTIONS: &[(&str, &str)] = &[(
    "crates/victory-formats/src/bcpack-tables.bin",
    "the BCPack format's constant tables: nothing decodes without them and they cannot be derived (owner's decision, 2026-11-23; docs/asset-policy.md)",
)];

/// Signatures of formats that must never be committed.
const FORBIDDEN_MAGIC: &[(&[u8], &str)] = &[
    (b"MZ", "PE executable"),
    (b"DXBC", "shader container"),
    (b"DDS ", "DDS texture"),
    (b"RIFF", "RIFF (audio/video)"),
    (b"BC\xC0\xDE", "LLVM bitcode"),
    (b"Microsoft C/C++ MSF", "PDB"),
    (b"!<arch>", "library archive"),
    (b"PK\x03\x04", "zip"),
    (b"7z\xBC\xAF", "7-zip"),
    (b"MSCF", "cabinet"),
    (b"\xD0\xCF\x11\xE0", "MSI/OLE"),
];

pub fn run(root: &Path, staged: bool) -> Result<()> {
    let files = list_files(root, staged)?;
    let mut violations = Vec::new();
    for path in &files {
        if EXCEPTIONS.iter().any(|(p, _)| p == path) {
            continue;
        }
        let content = if staged {
            let out = Command::new("git")
                .args(["show", &format!(":{path}")])
                .current_dir(root)
                .output()?;
            if !out.status.success() {
                // A submodule is staged as a link to a commit: no content.
                if is_gitlink(root, path)? {
                    continue;
                }
                // `--diff-filter=ACMRT` lists no deletions, so any other
                // failure is an error, never a reason to skip the file.
                bail!("git cannot read staged file {path:?}");
            }
            out.stdout
        } else {
            let file = root.join(path);
            // A nested repository or submodule: git lists the folder itself.
            if file.is_dir() {
                continue;
            }
            match std::fs::read(&file) {
                Ok(c) => c,
                // Listed by git but deleted in the working tree.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e).with_context(|| format!("reading {path}")),
            }
        };
        for problem in check(path, &content) {
            violations.push(format!("{path}: {problem}"));
        }
    }
    if violations.is_empty() {
        println!("asset policy: {} files ok", files.len());
        return Ok(());
    }
    for v in &violations {
        eprintln!("POLICY: {v}");
    }
    bail!(
        "{} asset-policy violation(s). Nothing from the game may be committed; \
         see docs/asset-policy.md",
        violations.len()
    )
}

/// Whether the index holds `path` as a submodule link (mode 160000).
fn is_gitlink(root: &Path, path: &str) -> Result<bool> {
    let out = Command::new("git")
        .args(["ls-files", "--stage", "-z", "--", path])
        .env("GIT_LITERAL_PATHSPECS", "1")
        .current_dir(root)
        .output()
        .context("running git")?;
    Ok(out.status.success() && out.stdout.starts_with(b"160000 "))
}

fn list_files(root: &Path, staged: bool) -> Result<Vec<String>> {
    let git = |args: &[&str]| -> Result<Option<Vec<String>>> {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .context("running git")?;
        if !out.status.success() {
            return Ok(None);
        }
        // Git prints '/' separators on every platform, so names are kept
        // verbatim: rewriting a backslash that is part of a name would make
        // the file unreadable under that name and let it skip the check.
        out.stdout
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| {
                String::from_utf8(s.to_vec())
                    .with_context(|| format!("file name {:?} is not valid UTF-8", s))
            })
            .collect::<Result<Vec<_>>>()
            .map(Some)
    };
    if staged {
        return git(&[
            "diff",
            "--cached",
            "--name-only",
            // T: a type change, such as a symlink replaced by a file.
            "--diff-filter=ACMRT",
            "-z",
        ])?
        .context("--staged needs a git repository");
    }
    if let Some(mut tracked) = git(&[
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
    ])? {
        tracked.sort();
        tracked.dedup();
        return Ok(tracked);
    }
    // Not a git repository: walk everything except build output.
    Ok(walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !matches!(e.file_name().to_str(), Some("target" | ".git")))
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            e.path().strip_prefix(root).ok().map(|p| {
                // Only the host's own separator is rewritten: elsewhere
                // a backslash is part of the name.
                let p = p.to_string_lossy();
                if cfg!(windows) {
                    p.replace('\\', "/")
                } else {
                    p.into_owned()
                }
            })
        })
        .collect())
}

/// All problems with one file. Pure, for testing.
pub fn check(path: &str, content: &[u8]) -> Vec<String> {
    let mut problems = Vec::new();
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    // Our own pictures for the README (drawings and screenshots of the Mac
    // port, never game content), and only in docs/images.
    let readme_art = path.starts_with("docs/images/")
        && matches!(ext.as_deref(), Some("svg" | "png" | "jpg" | "jpeg" | "gif"));
    let allowed = readme_art
        || ALLOWED_NAMES.contains(&name)
        || ext
            .as_deref()
            .is_some_and(|e| ALLOWED_EXTENSIONS.contains(&e));
    if !allowed {
        problems.push("file type not on the allowlist".to_owned());
    }
    if content.len() > MAX_FILE_SIZE {
        problems.push(format!(
            "{} bytes exceeds the {MAX_FILE_SIZE}-byte limit",
            content.len()
        ));
    }
    for (magic, what) in FORBIDDEN_MAGIC {
        if content.starts_with(magic) {
            problems.push(format!("looks like a {what}"));
        }
    }
    if content.contains(&0) {
        problems.push("binary content (NUL bytes)".to_owned());
    } else if let Some(run) = longest_encoded_run(content)
        && run > MAX_ENCODED_RUN
    {
        problems.push(format!("{run}-character hex/base64 run (embedded data?)"));
    }
    problems
}

/// Longest run of characters that could be hex or base64 payload.
fn longest_encoded_run(content: &[u8]) -> Option<usize> {
    let is_payload = |b: &u8| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=');
    content.split(|b| !is_payload(b)).map(<[u8]>::len).max()
}

#[cfg(test)]
mod tests {
    use super::check;

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
        let dir = std::env::temp_dir().join(format!("victory-xtask-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    #[cfg(unix)]
    fn staged_file_with_backslash_in_name_is_checked() {
        let dir = scratch("backslash");
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("evil\\x.bin"), b"MZ\x90\0").unwrap();
        git(&dir, &["add", "-A"]);
        let staged = super::run(&dir, true);
        let tracked = super::run(&dir, false);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(staged.is_err(), "staged file passed the policy");
        assert!(tracked.is_err(), "tracked file passed the policy");
    }

    /// Without git, the files are found by walking the folder: a backslash
    /// in a name is still part of the name there.
    #[test]
    #[cfg(unix)]
    fn walked_file_with_backslash_in_name_is_checked() {
        let dir = scratch("walk");
        std::fs::write(dir.join("evil\\x.bin"), b"MZ\x90\0").unwrap();
        let walked = super::run(&dir, false);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(walked.is_err(), "walked file passed the policy");
    }

    /// A committed symlink replaced by a file is staged as a type change,
    /// and the file's content is checked like any other.
    #[test]
    #[cfg(unix)]
    fn staged_type_change_is_checked() {
        let dir = scratch("typechange");
        git(&dir, &["init", "-q"]);
        std::os::unix::fs::symlink("elsewhere", dir.join("notes.txt")).unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-q", "-m", "a link"]);
        std::fs::remove_file(dir.join("notes.txt")).unwrap();
        std::fs::write(dir.join("notes.txt"), b"MZ\x90\0").unwrap();
        git(&dir, &["add", "-A"]);
        let staged = super::run(&dir, true);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(staged.is_err(), "staged type change passed the policy");
    }

    /// A nested repository is listed as a folder, and staged as a link to
    /// a commit: neither has content to check, and neither is an error.
    #[test]
    #[cfg(unix)]
    fn nested_repositories_are_not_read_as_files() {
        let dir = scratch("nested");
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("readme.md"), b"ours\n").unwrap();
        let inner = dir.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        git(&inner, &["init", "-q"]);
        std::fs::write(inner.join("readme.md"), b"theirs\n").unwrap();
        git(&inner, &["add", "-A"]);
        git(&inner, &["commit", "-q", "-m", "inner"]);
        let untracked = super::run(&dir, false);
        // Staging the folder records the inner repository as a link.
        git(&dir, &["add", "-A"]);
        let staged = super::run(&dir, true);
        let tracked = super::run(&dir, false);
        let _ = std::fs::remove_dir_all(&dir);
        untracked.expect("an untracked nested repository");
        staged.expect("a staged link to a nested repository");
        tracked.expect("a tracked link to a nested repository");
    }

    /// Enforces the policy wherever tests run, even in clones without the hook.
    #[test]
    fn repository_passes_policy() {
        // The check lists files with git. Where git cannot be started, as
        // inside Wine on the macOS test runs, there is nothing to check.
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("git cannot be started here: policy not checked");
            return;
        }
        super::run(&crate::workspace_root(), false).unwrap();
    }

    #[test]
    fn accepts_source() {
        assert!(check("crates/x/src/lib.rs", b"fn main() {}\n").is_empty());
        assert!(check(".gitignore", b"/target/\n").is_empty());
        assert!(check(".githooks/pre-commit", b"#!/bin/sh\n").is_empty());
    }

    #[test]
    fn rejects_game_like_files() {
        assert!(!check("assets/tex.dds", b"DDS \x7c\0\0\0").is_empty());
        assert!(
            !check("notes.txt", b"MZ\x90\0").is_empty(),
            "magic wins over extension"
        );
        assert!(
            !check("data.bin", b"hello").is_empty(),
            "extension not allowed"
        );
        assert!(
            !check("a.rs", &vec![b'a'; 600 * 1024]).is_empty(),
            "too big"
        );
    }

    #[test]
    fn rejects_data_smuggled_into_text() {
        let blob = format!("const DATA: &str = \"{}\";", "A1".repeat(400));
        assert!(!check("src/blob.rs", blob.as_bytes()).is_empty());
    }
}
