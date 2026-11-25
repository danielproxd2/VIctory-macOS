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
/// The longest list of numeric literals a file may hold. Our largest
/// hand-written table has a few hundred entries.
const MAX_LITERAL_RUN: usize = 1024;

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

/// A path exempted from the checks, as one exact file.
pub(crate) struct Exception {
    pub path: &'static str,
    /// Why the file is here. Read by people, not by the check.
    #[allow(dead_code)]
    pub reason: &'static str,
    /// The file's length in bytes.
    pub len: usize,
    /// FNV-1a of the file, as `embed_clamp::fnv` computes it. It pins the
    /// content against a slip, not against an attacker.
    pub fnv: u64,
}

/// Paths explicitly exempted, with a reason. Keep this empty if at all
/// possible. An entry names one exact file: anything else at its path
/// fails.
const EXCEPTIONS: &[Exception] = &[
    Exception {
        path: "crates/victory-formats/src/bcpack-tables.bin",
        reason: "the BCPack format's constant tables: nothing decodes without them and they cannot be derived (owner's decision, 2026-11-23; docs/asset-policy.md)",
        len: 77982,
        fnv: 0x4808703315eae4f0,
    },
    Exception {
        path: "crates/victory-gpu/src/clamp_bytecode.rs",
        reason: "our own clamp.hlsl compiled by Microsoft's compilers, which Wine cannot run; written by `cargo xtask embed-clamp`, which prints this entry's numbers",
        len: 40191,
        fnv: 0xc6be13f15270dfea,
    },
];

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

/// Raster pictures allowed in docs/images: extensions and the signatures
/// such a file starts with.
const RASTER_SIGNATURES: &[(&[&str], &[&[u8]])] = &[
    (&["png"], &[b"\x89PNG\r\n\x1a\n"]),
    (&["jpg", "jpeg"], &[b"\xFF\xD8\xFF"]),
    (&["gif"], &[b"GIF87a", b"GIF89a"]),
];

pub fn run(root: &Path, staged: bool) -> Result<()> {
    run_with(root, staged, EXCEPTIONS)
}

fn run_with(root: &Path, staged: bool, exceptions: &[Exception]) -> Result<()> {
    let files = list_files(root, staged)?;
    let mut violations = Vec::new();
    for path in &files {
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
        let problems = match exceptions.iter().find(|e| e.path == path) {
            Some(exception) => check_exception(exception, &content),
            None => check(path, &content),
        };
        for problem in problems {
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

/// An exempted path passes only as the file its entry pins.
fn check_exception(exception: &Exception, content: &[u8]) -> Vec<String> {
    let mut problems = Vec::new();
    if content.len() != exception.len {
        problems.push(format!(
            "exception pinned at {} bytes, found {}",
            exception.len,
            content.len()
        ));
    }
    let found = crate::embed_clamp::fnv(content);
    if found != exception.fnv {
        problems.push(format!(
            "exception pinned at hash 0x{:016x}, found 0x{found:016x}",
            exception.fnv
        ));
    }
    problems
}

/// All problems with one file. Pure, for testing.
pub fn check(path: &str, content: &[u8]) -> Vec<String> {
    let mut problems = Vec::new();
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    // Our own pictures for the README (drawings and screenshots of the Mac
    // port, never game content), and only in docs/images.
    let in_images = path.starts_with("docs/images/");
    let raster = match ext.as_deref() {
        Some(e) if in_images => RASTER_SIGNATURES
            .iter()
            .find(|(exts, _)| exts.contains(&e))
            .map(|(_, signatures)| *signatures),
        _ => None,
    };
    let allowed = raster.is_some()
        || (in_images && ext.as_deref() == Some("svg"))
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
    if let Some(signatures) = raster {
        // A picture is binary by nature, so the text checks say nothing
        // about it. It must at least be the picture its name says it is.
        if !signatures.iter().any(|s| content.starts_with(s)) {
            problems.push("not the picture its name says it is".to_owned());
        }
    } else if content.contains(&0) {
        problems.push("binary content (NUL bytes)".to_owned());
    } else if let Some(run) = longest_encoded_run(content)
        && run > MAX_ENCODED_RUN
    {
        problems.push(format!("{run}-character hex/base64 run (embedded data?)"));
    } else {
        let digits = longest_hex_literal_run(content);
        if digits > MAX_ENCODED_RUN {
            problems.push(format!(
                "{digits} hex digits in one run of \\x/0x literals (embedded data?)"
            ));
        }
        let run = longest_literal_run(content);
        if run > MAX_LITERAL_RUN {
            problems.push(format!("{run}-literal byte-array run (embedded data?)"));
        }
    }
    problems
}

/// Longest run of characters that could be hex or base64 payload.
fn longest_encoded_run(content: &[u8]) -> Option<usize> {
    let is_payload = |b: &u8| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=');
    content.split(|b| !is_payload(b)).map(<[u8]>::len).max()
}

/// Most hex digits in `\xNN` escapes or `0x` literals with only commas,
/// whitespace or backslashes between them: a byte string or `xxd -i` array,
/// which `longest_encoded_run` sees as many 3- or 4-character runs.
/// Decimal arrays (`[77, 90, ...]`) are not seen here: `longest_literal_run`
/// counts those.
fn longest_hex_literal_run(content: &[u8]) -> usize {
    let (mut i, mut run, mut longest) = (0, 0, 0);
    while i < content.len() {
        match &content[i..] {
            [b'\\' | b'0', b'x' | b'X', rest @ ..] => {
                let len = rest
                    .iter()
                    .take_while(|b| b.is_ascii_hexdigit() || **b == b'_')
                    .count();
                run += rest[..len].iter().filter(|b| b.is_ascii_hexdigit()).count();
                longest = longest.max(run);
                // Skip a type suffix such as `u8` or `u32`.
                let suffix = rest[len..]
                    .iter()
                    .take_while(|b| b.is_ascii_alphanumeric())
                    .count();
                i += 2 + len + suffix;
            }
            [b, ..] if matches!(b, b',' | b'\\') || b.is_ascii_whitespace() => i += 1,
            _ => {
                run = 0;
                i += 1;
            }
        }
    }
    longest
}

/// Longest list of numeric literals: `0x4d, 0x5a,` or `77, 90,`, of any
/// width, with or without a type suffix. A file converted to an array
/// looks like this, and breaks the encoded run at every comma.
///
/// A literal counts when a comma or a closing bracket follows it. The list
/// goes on across commas, brackets of all three kinds, line breaks and
/// comments, so rows (`[1, 2], [3, 4]`), tuples (`(1, 2), (3, 4)`) and a
/// comment between two rows are all one list. Anything else ends it: a
/// name, a string, an operator, a semicolon. A number with a decimal point
/// is not counted.
///
/// Comments are read both ways and the longer list is taken: once as
/// nothing at all, so that a comment between two entries does not split a
/// list, and once as ordinary text whose markers (`/`, `*`, `#`) are
/// stepped over, so that a list written in comments that hold nothing else
/// is counted.
///
/// That is all it does. In the second reading a word ends a list as it
/// does anywhere else, so rows of literals in comments with a comment of
/// words between the rows count one row at a time and pass. It is one of
/// the gaps docs/asset-policy.md lists.
fn longest_literal_run(content: &[u8]) -> usize {
    literal_run(content, true)
        .0
        .max(literal_run(content, false).0)
}

/// [`longest_literal_run`] with comments either stepped over
/// (`skip_comments`) or read as text whose markers end no list.
///
/// The second number is the work spent looking for the ends of comments,
/// in bytes looked at. It is there for the test that keeps this linear in
/// the file's length.
fn literal_run(content: &[u8], skip_comments: bool) -> (usize, usize) {
    use std::cell::Cell;

    let (mut best, mut run, mut i) = (0, 0, 0);
    let word = |b: &u8| b.is_ascii_alphanumeric() || *b == b'_';
    let searched = Cell::new(0usize);
    let line_end = |from: usize| {
        let end = content[from..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(content.len(), |n| from + n);
        searched.set(searched.get() + (end - from));
        end
    };
    // Where a search for `*/` last came up empty: none follows that
    // position, so none follows a later one either. Without this, every
    // `/*` of a file that never closes one reads the rest of the file.
    let no_close_from = Cell::new(usize::MAX);
    // The end of the comment that starts at `i`, if one does: `//` and `#`
    // to the end of the line, `/*` to its `*/`.
    let comment = |i: usize| -> Option<usize> {
        let rest = &content[i..];
        if rest.starts_with(b"//") || rest.starts_with(b"#") {
            Some(line_end(i))
        } else if rest.starts_with(b"/*") {
            if i >= no_close_from.get() {
                return None;
            }
            let close = rest[2..].windows(2).position(|w| w == b"*/");
            searched.set(searched.get() + close.map_or(rest.len() - 2, |n| n + 2));
            if close.is_none() {
                no_close_from.set(i);
            }
            Some(i + 2 + close? + 2)
        } else {
            None
        }
    };
    // Steps over what neither belongs to a list nor ends one.
    let skip = |mut i: usize| {
        loop {
            match content.get(i) {
                Some(b) if b.is_ascii_whitespace() => i += 1,
                Some(b'/' | b'*' | b'#') if !skip_comments => i += 1,
                Some(b'/' | b'#') if skip_comments => match comment(i) {
                    Some(end) => i = end,
                    None => return i,
                },
                _ => return i,
            }
        }
    };
    loop {
        i = skip(i);
        let start = i;
        let Some(first) = content.get(i) else {
            return (best, searched.get());
        };
        if matches!(first, b',' | b'[' | b']' | b'(' | b')' | b'{' | b'}') {
            i += 1;
            continue;
        }
        if *first == b'-' {
            i += 1;
        }
        let digits = i;
        if content.get(i).is_some_and(u8::is_ascii_digit) {
            // Digits, hex digits, `x`, underscores and a suffix alike.
            while content.get(i).is_some_and(word) {
                i += 1;
            }
        }
        let end = skip(i);
        if i > digits && matches!(content.get(end), Some(b',' | b']' | b')' | b'}')) {
            run += 1;
            best = best.max(run);
            i = end + 1;
        } else {
            run = 0;
            // Whatever stands here is not part of a list. Step over all of
            // it, so that the `1` of a name like `x1` starts nothing.
            i = i.max(start + 1);
            if word(first) {
                while content.get(i).is_some_and(word) {
                    i += 1;
                }
            }
        }
    }
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

    /// An exception is one exact file: the same path with other content,
    /// or with more of it, fails, staged or not.
    #[test]
    #[cfg(unix)]
    fn an_exception_is_pinned_to_its_length_and_hash() {
        let pinned = b"\0tables\0".to_vec();
        let exceptions = [super::Exception {
            path: "data/tables.bin",
            reason: "a test",
            len: pinned.len(),
            fnv: crate::embed_clamp::fnv(&pinned),
        }];
        let mut modified = pinned.clone();
        modified[3] ^= 1;
        let mut grown = pinned.clone();
        grown.push(0);
        let mut results = Vec::new();
        for content in [&pinned, &modified, &grown] {
            let dir = scratch("pinned");
            git(&dir, &["init", "-q"]);
            std::fs::create_dir_all(dir.join("data")).unwrap();
            std::fs::write(dir.join("data/tables.bin"), content).unwrap();
            let untracked = super::run_with(&dir, false, &exceptions);
            git(&dir, &["add", "-A"]);
            let staged = super::run_with(&dir, true, &exceptions);
            let _ = std::fs::remove_dir_all(&dir);
            results.push((untracked.is_ok(), staged.is_ok()));
        }
        assert_eq!(results[0], (true, true), "the pinned file");
        assert_eq!(results[1], (false, false), "a modified file");
        assert_eq!(results[2], (false, false), "a grown file");
        // Without its entry the same file is an ordinary violation.
        let dir = scratch("unpinned");
        std::fs::write(dir.join("tables.bin"), &pinned).unwrap();
        let unlisted = super::run_with(&dir, false, &exceptions);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(unlisted.is_err(), "the file at another path");
    }

    /// The length and hash checks report separately.
    #[test]
    fn an_exception_reports_what_differs() {
        let exception = super::Exception {
            path: "x.bin",
            reason: "a test",
            len: 3,
            fnv: crate::embed_clamp::fnv(b"abc"),
        };
        assert!(super::check_exception(&exception, b"abc").is_empty());
        let same_length = super::check_exception(&exception, b"abd");
        assert_eq!(same_length.len(), 1, "{same_length:?}");
        assert!(same_length[0].contains("hash"), "{same_length:?}");
        let longer = super::check_exception(&exception, b"abcd");
        assert!(longer.iter().any(|p| p.contains("bytes")), "{longer:?}");
    }

    /// A file converted to an array of literals is data, whatever the
    /// literals' width or base, and the hex/base64 check cannot see it.
    #[test]
    fn rejects_data_smuggled_as_a_byte_array() {
        use std::fmt::Write as _;
        let array = |literal: &dyn Fn(usize) -> String, count: usize| {
            let mut text = String::from("pub const DATA: &[u8] = &[\n");
            for i in 0..count {
                let _ = write!(text, "{}", literal(i));
                text.push_str(if i % 16 == 15 { ",\n" } else { ", " });
            }
            text.push_str("];\n");
            text
        };
        let named = |problems: Vec<String>| problems.iter().any(|p| p.contains("byte-array run"));
        let hex = array(&|i| format!("0x{:02x}", i % 256), 4096);
        assert!(named(check("src/blob.rs", hex.as_bytes())), "hex bytes");
        let decimal = array(&|i| format!("{}", i % 256), 4096);
        assert!(named(check("src/blob.rs", decimal.as_bytes())), "decimal");
        let words = array(&|i| format!("0x{:08X}u32", i * 0x0101_0101), 2048);
        assert!(named(check("src/blob.rs", words.as_bytes())), "words");
        let signed = array(&|i| format!("-{}", i % 128), 2048);
        assert!(named(check("data.json", signed.as_bytes())), "signed");
        let packed = array(&|i| format!("{}", i % 10), 4096).replace([' ', '\n'], "");
        assert!(named(check("src/blob.rs", packed.as_bytes())), "no spaces");

        // The limit is on one list, not on a file's total.
        let at_limit = array(&|i| format!("{}", i % 256), super::MAX_LITERAL_RUN);
        assert_eq!(super::longest_literal_run(at_limit.as_bytes()), 1024);
        assert_eq!(
            check("src/table.rs", at_limit.as_bytes()),
            Vec::<String>::new()
        );
        let two = format!("{at_limit}{at_limit}");
        assert_eq!(check("src/table.rs", two.as_bytes()), Vec::<String>::new());
        // The same list in hex is as long, and under this limit, but its
        // 2048 hex digits are over the limit on `0x` literals in a row.
        let hex_at_limit = array(&|i| format!("0x{:02x}", i % 256), super::MAX_LITERAL_RUN);
        assert_eq!(super::longest_literal_run(hex_at_limit.as_bytes()), 1024);
        let problems = check("src/table.rs", hex_at_limit.as_bytes());
        assert!(!named(problems.clone()), "{problems:?}");
        assert!(
            problems.iter().any(|p| p.contains("hex digits in one run")),
            "{problems:?}"
        );

        // Ordinary source with a few constants.
        let source = b"const MASK: u32 = 0xffff_0000;\n\
            const SIZES: [u8; 4] = [1, 2, 4, 8];\n\
            fn f(a: u32, b: u32) -> u32 {\n    g(a, 0x10, b, 3, -1, v2, 7)\n}\n";
        // `[1, 2, 4, 8]`: the bracket closes the list after its last entry.
        assert_eq!(super::longest_literal_run(source), 4);
        assert_eq!(check("src/lib.rs", source), Vec::<String>::new());
        assert_eq!(super::longest_literal_run(b""), 0);
        assert_eq!(super::longest_literal_run(b"1, 2 \n"), 1);
        assert_eq!(super::longest_literal_run(b"1, 2, -"), 2);
        assert_eq!(super::longest_literal_run(b"-, -, x1, 0x"), 0);
    }

    /// Rows, tuples and comments do not split a list: a blob written
    /// sixteen to a row inside inner brackets is as long as it is.
    #[test]
    fn a_list_goes_on_across_brackets_and_comments() {
        use std::fmt::Write as _;
        let run = |text: &str| super::longest_literal_run(text.as_bytes());
        let named = |text: &str| {
            check("src/blob.rs", text.as_bytes())
                .iter()
                .any(|p| p.contains("byte-array run"))
        };
        // `count` literals, `per` to a group, each group between `open`
        // and `close` and followed by `between`.
        let grouped = |count: usize, per: usize, open: &str, close: &str, between: &str| {
            let mut text = String::from("pub const DATA: &[[u8; 16]] = &[\n");
            for group in 0..count / per {
                text.push_str(open);
                for i in 0..per {
                    let sep = if i + 1 == per { "" } else { ", " };
                    let _ = write!(text, "0x{:02x}{sep}", (group * per + i) % 256);
                }
                text.push_str(close);
                text.push_str(between);
            }
            text.push_str("];\n");
            text
        };

        assert_eq!(run("[[1, 2], [3, 4]]"), 4);
        assert_eq!(run("[(1, 2), (3, 4)]"), 4);
        assert_eq!(run("[[[1, 2], [3]], [[4, 5, 6]]]"), 6);
        assert_eq!(run("{ 1, 2 }, { 3, 4 },"), 4);
        assert_eq!(run("[1, 2],\n// the second row\n[3, 4],\n"), 4);
        assert_eq!(run("[1, 2],\n# the second row\n[3, 4],\n"), 4);
        assert_eq!(run("[1, 2], /* the second\n row */ [3, 4]"), 4);
        assert_eq!(run("1 /* a */, 2 /* b */, 3 // c\n, 4,"), 4);
        // A list inside comments is still a list.
        assert_eq!(run("// 1, 2, 3,\n// 4, 5, 6,\n"), 6);
        assert_eq!(run("/* 1, 2,\n * 3, 4,\n */"), 4);
        assert_eq!(run("# 1, 2,\n# 3, 4,\n"), 4);
        // What opens no comment hides nothing: a glob, and a `/*` that is
        // closed far away.
        assert_eq!(run("\"src/*.rs\" 1, 2, 3,"), 3);
        assert_eq!(run("\"a/*\" 1, 2, 3, \"*/b\""), 3);
        // Names, strings, operators and semicolons end a list.
        assert_eq!(run("[1, 2]; [3, 4]"), 2);
        assert_eq!(run("[1, 2] + [3, 4, 5]"), 3);
        assert_eq!(run("[1, 2], x, [3, 4, 5]"), 3);
        assert_eq!(run("(1, \"a\"), (2, \"b\"), (3, \"c\")"), 1);
        // Numbers with a decimal point are not counted.
        assert_eq!(run("[1.5, 2.5, 3.5, 4.5]"), 1);
        assert_eq!(run("[[0.0, 1.0], [2.0, 3.0]]"), 1);

        let rows = grouped(4096, 16, "    [", "],", "\n");
        assert_eq!(run(&rows), 4096);
        assert!(named(&rows), "rows of sixteen");
        let tuples = grouped(4096, 2, "(", "),", " ");
        assert_eq!(run(&tuples), 4096);
        assert!(named(&tuples), "pairs");
        let commented = grouped(4096, 16, "    [", "],", "\n    // next\n");
        assert_eq!(run(&commented), 4096);
        assert!(named(&commented), "a comment after every row");
        let in_comments = grouped(4096, 16, "// ", ",", "\n");
        assert_eq!(run(&in_comments), 4096);
        assert!(named(&in_comments), "rows that are comments");

        // What the two readings do not catch, and docs/asset-policy.md
        // says so: rows in comments with a comment of words between them.
        // Read as text, the word ends each row; with comments stepped over
        // there is nothing left.
        assert_eq!(run("// 1, 2,\n// row\n// 3, 4, 5,\n"), 3);
        let worded = grouped(4096, 16, "// ", ",", "\n// row\n");
        assert_eq!(run(&worded), 16);
        assert!(!named(&worded), "rows that are comments, a word between");
        // A row of code between rows of comments ends the list as well,
        // unless it is itself part of the list.
        assert_eq!(run("// 1, 2,\nlet x = 0;\n// 3, 4, 5,\n"), 3);
        assert_eq!(run("// 1, 2,\n[3, 4],\n// 5, 6,\n"), 6);

        // The same shapes at the limit pass.
        let at_limit = grouped(super::MAX_LITERAL_RUN, 16, "    [", "],", "\n");
        assert_eq!(run(&at_limit), super::MAX_LITERAL_RUN);
        assert!(!named(&at_limit), "rows at the limit");
    }

    /// The check reads every text file under the size limit, so it must
    /// stay linear in the file's length whatever the file holds. Looking
    /// for the end of a comment is the part that can go wrong: a file of
    /// `/* ` that closes none once made every one of them read the rest of
    /// the file, and a file just under the limit took longer than nine
    /// minutes.
    ///
    /// This counts the bytes looked at instead of taking the time: the
    /// count is the same in every build and on a busy machine. Read
    /// quadratically, the first file costs a third of its length squared,
    /// about ninety thousand million bytes; the bound is four times its
    /// length, about two million.
    #[test]
    fn looking_for_the_end_of_a_comment_is_linear() {
        let fill = |unit: &str| unit.repeat((super::MAX_FILE_SIZE - 1) / unit.len());
        for unit in [
            "/* ",
            "/*",
            "/*/",
            "/* */ ",
            "x /* */ ",
            "- /* 1, */ ",
            "// 1,\n",
            "x // 1,\n",
            "# ",
            "/ * ",
        ] {
            let text = fill(unit);
            assert!(text.len() < super::MAX_FILE_SIZE && text.len() > super::MAX_FILE_SIZE - 16);
            for skip_comments in [true, false] {
                let (_, searched) = super::literal_run(text.as_bytes(), skip_comments);
                assert!(
                    searched <= 4 * text.len(),
                    "{unit:?} repeated, comments skipped: {skip_comments}: \
                     {searched} bytes looked at for a file of {}",
                    text.len()
                );
            }
        }
        // Such a file is checked like any other, and it holds no list.
        let unclosed = fill("/* ");
        assert_eq!(
            check("src/lib.rs", unclosed.as_bytes()),
            Vec::<String>::new()
        );
        // A `/*` that is never closed is no comment, here as in a short
        // file: what follows it is read, and it ends a list as any other
        // text does.
        let skipping = |text: &str| super::literal_run(text.as_bytes(), true).0;
        assert_eq!(skipping("/* 1, 2, 3,"), 3);
        assert_eq!(skipping("/* a */ 1, /* 2, 3, 4,"), 3);
        assert_eq!(skipping("1, /* 2, /* 3, */ 4, /* 5,"), 2);
        assert_eq!(skipping("1, /* 2, */ 3, /* 4, */ 5, /* 6, 7"), 3);
    }

    /// The embedded clamp bytecode is such an array. It passes as the one
    /// file its exception pins, and as nothing else.
    #[test]
    fn clamp_bytecode_passes_only_through_its_exception() {
        let path = "crates/victory-gpu/src/clamp_bytecode.rs";
        let content = std::fs::read(crate::workspace_root().join(path)).unwrap();
        // A checkout that rewrote line ends would differ from the pin.
        assert!(!content.contains(&b'\r'), "line ends were rewritten");
        let problems = check(path, &content);
        assert!(
            problems.iter().any(|p| p.contains("byte-array run")),
            "{problems:?}"
        );
        let exception = super::EXCEPTIONS
            .iter()
            .find(|e| e.path == path)
            .expect("an entry for the clamp bytecode");
        assert_eq!(
            super::check_exception(exception, &content),
            Vec::<String>::new(),
            "run `cargo xtask embed-clamp` again and enter the numbers it prints"
        );
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

    /// The start of a PNG, assembled here: the signature and an IHDR chunk
    /// header for a 1x1 picture. Its length field alone holds NUL bytes.
    fn png_start() -> Vec<u8> {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&1u32.to_be_bytes());
        png.extend_from_slice(&1u32.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0]);
        png
    }

    #[test]
    fn readme_pictures_pass_only_in_docs_images() {
        let png = png_start();
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>\n";
        assert_eq!(check("docs/images/x.png", &png), Vec::<String>::new());
        assert_eq!(check("docs/images/x.svg", svg), Vec::<String>::new());
        assert_eq!(
            check("docs/images/x.gif", b"GIF89a\x01\0\x01\0"),
            Vec::<String>::new()
        );
        assert_eq!(
            check("docs/images/x.JPG", b"\xFF\xD8\xFF\xE0\0\x10JFIF\0"),
            Vec::<String>::new()
        );
        assert!(
            !check("other/x.png", &png).is_empty(),
            "a picture outside docs/images"
        );
        assert!(
            !check("other/x.svg", svg).is_empty(),
            "a drawing outside docs/images"
        );
        assert!(
            !check("docs/images/x.svg", &png).is_empty(),
            "a PNG named .svg"
        );
        assert!(
            !check("docs/images/x.png", b"GIF89a\x01\0\x01\0").is_empty(),
            "a GIF named .png"
        );
        assert!(
            !check("docs/images/x.png", b"MZ\x90\0").is_empty(),
            "an executable named .png"
        );
        assert!(
            !check("docs/images/x.png", svg).is_empty(),
            "text named .png"
        );
        let mut big = png;
        big.resize(600 * 1024, 0);
        assert!(!check("docs/images/x.png", &big).is_empty(), "too big");
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

    /// Synthetic bytes, never game data.
    fn synthetic(n: u32) -> impl Iterator<Item = u32> {
        (0..n).map(|i| (i * 37 + 11) % 256)
    }

    #[test]
    fn rejects_data_smuggled_as_an_escaped_byte_string() {
        // 4096 bytes as one unbroken `\xNN` hex run, the shape a byte-string
        // literal of copied data takes.
        let esc: String = synthetic(4096).map(|b| format!("\\x{b:02x}")).collect();
        let src = format!("pub const DATA: &[u8] = b\"{esc}\";\n");
        assert!(
            !check("src/data.rs", src.as_bytes()).is_empty(),
            "{} bytes of \\x-escaped hex passed",
            src.len()
        );
    }

    #[test]
    fn rejects_data_smuggled_as_a_hex_byte_array() {
        // `xxd -i` style: 4096 bytes as hex literals.
        let lits: Vec<String> = synthetic(4096).map(|b| format!("0x{b:02x}")).collect();
        let src = format!("pub const DATA: [u8; 4096] = [{}];\n", lits.join(", "));
        assert!(
            !check("src/data.rs", src.as_bytes()).is_empty(),
            "{} bytes of hex array passed",
            src.len()
        );
        // A short constant table, like the 16-entry ones in the tree, stays legal.
        let table: Vec<String> = synthetic(16).map(|w| format!("0x{w:08x}_u32")).collect();
        let src = format!(
            "const T: [u32; 16] = [\n    {},\n];\n",
            table.join(",\n    ")
        );
        assert!(check("src/table.rs", src.as_bytes()).is_empty());
    }
}
