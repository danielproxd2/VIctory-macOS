//! Rewriting a title's path on a virtual drive to the host path behind it.
//! Works on UTF-16 slices and a mount table, with no host calls, so it
//! builds and is tested everywhere.

pub(crate) const SEPARATORS: [u16; 2] = [b'\\' as u16, b'/' as u16];
const VERBATIM: [u16; 4] = [b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];

/// Host paths from this length on take the `\\?\` form, which lifts the
/// host's MAX_PATH limit (248 characters for a directory, 260 for a file).
const LONG: usize = 248;

/// `root` and the title's `tail` as a `\\?\` path. That form turns off the
/// host's own clean-up, so `/` becomes `\` and `.` and `..` are resolved
/// here (`..` stops at the root).
pub(crate) fn verbatim(root: &[u16], tail: &[u16]) -> Vec<u16> {
    let backslash = u16::from(b'\\');
    let mut out = if root.starts_with(&VERBATIM) {
        root.to_vec()
    } else if let Some(unc) = root.strip_prefix(&[backslash; 2][..]) {
        let mut out = VERBATIM.to_vec();
        out.extend("UNC\\".encode_utf16());
        out.extend_from_slice(unc);
        out
    } else {
        [&VERBATIM[..], root].concat()
    };
    while out.last() == Some(&backslash) {
        out.pop();
    }
    let dot = u16::from(b'.');
    let mut parts: Vec<&[u16]> = Vec::new();
    for part in tail.split(|c| SEPARATORS.contains(c)) {
        match part {
            [] => {}
            [d] if *d == dot => {}
            [d, e] if [*d, *e] == [dot; 2] => drop(parts.pop()),
            _ => parts.push(part),
        }
    }
    for part in parts {
        out.push(backslash);
        out.extend_from_slice(part);
    }
    out
}

/// The host path, NUL-terminated, for `path` (without its NUL) if it is on
/// one of `drives`: upper-case letter and host root (UTF-16, no trailing
/// separator, no NUL) each.
pub(crate) fn translate(drives: &[(u16, Vec<u16>)], path: &[u16]) -> Option<Vec<u16>> {
    let (prefix, rest) = match path.strip_prefix(&VERBATIM[..]) {
        Some(rest) => (&VERBATIM[..], rest),
        None => (&[][..], path),
    };
    let &[letter, colon, ..] = rest else {
        return None;
    };
    if colon != u16::from(b':') {
        return None;
    }
    let letter = u8::try_from(letter).ok().filter(u8::is_ascii_alphabetic)?;
    let letter = u16::from(letter.to_ascii_uppercase());
    let (_, root) = drives.iter().find(|(l, _)| *l == letter)?;
    let tail = &rest[2..];
    // The host resolves `..` itself in the short form, past the folder
    // behind the drive; the drive's root has no parent, so resolve it here.
    let dot = u16::from(b'.');
    let climbs = tail
        .split(|c| SEPARATORS.contains(c))
        .any(|part| part == [dot; 2]);
    if prefix.is_empty() && (climbs || root.len() + tail.len() >= LONG) {
        let mut out = verbatim(root, tail);
        out.push(0);
        return Some(out);
    }
    let mut out = prefix.to_vec();
    out.extend_from_slice(root);
    // `X:` and `X:name` are relative to the root.
    if !tail.first().is_some_and(|c| SEPARATORS.contains(c)) {
        out.push(b'\\'.into());
    }
    let verbatim = !prefix.is_empty();
    out.extend(tail.iter().map(|&c| {
        if verbatim && c == u16::from(b'/') {
            b'\\'.into()
        } else {
            c
        }
    }));
    out.push(0);
    Some(out)
}

/// `path` without the `\\?\` prefix (`\\?\UNC\server` as `\\server`) and
/// without trailing separators.
fn plain(path: &[u16]) -> Vec<u16> {
    let backslash = u16::from(b'\\');
    let unc: Vec<u16> = "UNC\\".encode_utf16().collect();
    let mut out = match path.strip_prefix(&VERBATIM[..]) {
        Some(rest) => match rest.get(..4) {
            Some(head) if same(head, &unc) => [&[backslash; 2][..], &rest[4..]].concat(),
            _ => rest.to_vec(),
        },
        None => path.to_vec(),
    };
    while out.last().is_some_and(|c| SEPARATORS.contains(c)) {
        out.pop();
    }
    out
}

/// Whether two stretches of a host path name the same thing as far as can
/// be told without the host: ASCII letters in either case, either
/// separator. Other letters must match exactly.
fn same(a: &[u16], b: &[u16]) -> bool {
    let fold = |c: u16| match u8::try_from(c) {
        Ok(b'/') => u16::from(b'\\'),
        Ok(c) => u16::from(c.to_ascii_uppercase()),
        Err(_) => c,
    };
    a.len() == b.len() && a.iter().zip(b).all(|(&x, &y)| fold(x) == fold(y))
}

/// The title's path (`X:\tail`, no NUL) for the host path `host` (no NUL)
/// if it lies in the folder behind one of `drives`: the reverse of
/// [`translate`], for what the host hands back (the current directory, a
/// full path, the path of an open file). The deepest folder wins. A host
/// path in the `\\?\` form is recognised; the answer never has the prefix.
pub(crate) fn untranslate(drives: &[(u16, Vec<u16>)], host: &[u16]) -> Option<Vec<u16>> {
    let ends_in_separator = host.last().is_some_and(|c| SEPARATORS.contains(c));
    let host = plain(host);
    let (letter, tail) = drives
        .iter()
        .filter_map(|(letter, root)| {
            let root = plain(root);
            let head = host.get(..root.len())?;
            let tail = &host[root.len()..];
            let inside = tail.first().is_none_or(|c| SEPARATORS.contains(c));
            (!root.is_empty() && inside && same(head, &root)).then_some((root.len(), *letter, tail))
        })
        .max_by_key(|(len, ..)| *len)
        .map(|(_, letter, tail)| (letter, tail))?;
    let backslash = u16::from(b'\\');
    let mut out = vec![letter, u16::from(b':'), backslash];
    out.extend(tail.iter().skip_while(|c| SEPARATORS.contains(c)));
    // A trailing separator says "folder" (the host's temporary folder ends
    // in one); the root has its own.
    if ends_in_separator && out.len() > 3 {
        out.push(backslash);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    const ROOT: &str = r"C:\host\root";

    /// `path` against a table with `T:` on [`ROOT`] and `G:` on a share.
    fn tr(path: &str) -> Option<String> {
        let drives = [
            (u16::from(b'T'), w(ROOT)),
            (u16::from(b'G'), w(r"\\server\share\game")),
        ];
        translate(&drives, &w(path)).map(|host| {
            assert_eq!(host.last(), Some(&0), "NUL-terminated");
            String::from_utf16(&host[..host.len() - 1]).unwrap()
        })
    }

    #[test]
    fn translate_table() {
        let some = |s: &str| Some(s.to_owned());
        // Either separator and either case of the letter; the title's
        // separators stay in the short form.
        assert_eq!(tr("T:/a/b.txt"), some(r"C:\host\root/a/b.txt"));
        assert_eq!(tr(r"t:\a"), some(r"C:\host\root\a"));
        // `X:` and `X:name` are relative to the root.
        assert_eq!(tr("T:"), some(r"C:\host\root\"));
        assert_eq!(tr("T:name"), some(r"C:\host\root\name"));
        // The verbatim form keeps its prefix and takes backslashes.
        assert_eq!(tr(r"\\?\T:/a/b"), some(r"\\?\C:\host\root\a\b"));
        // The table decides the drive.
        assert_eq!(tr(r"G:\data.bin"), some(r"\\server\share\game\data.bin"));
    }

    #[test]
    fn translate_leaves_other_paths_alone() {
        for path in [
            "",
            "T",
            r"C:\Windows",
            "relative/path",
            r"\T:\a",
            r"\\server\share\T:\a",
            r"\\?\",
            r"\\?\C:\Windows",
            r"1:\a",
            r"::\a",
            // Not ASCII letters, though their low byte or look says T.
            "\u{154}:\\a",
            "\u{ff34}:\\a",
        ] {
            assert_eq!(tr(path), None, "{path:?}");
        }
        // No drives, no rewriting.
        assert_eq!(translate(&[], &w(r"T:\a")), None);
    }

    #[test]
    fn translate_resolves_dot_dot_inside_the_root() {
        let inside = Some(r"\\?\C:\host\root\Windows\x".to_owned());
        assert_eq!(tr(r"T:\..\..\Windows\x"), inside);
        assert_eq!(tr("T:/a/../../Windows/x"), inside);
        assert_eq!(tr(r"T:..\Windows\x"), inside);
        assert_eq!(tr(r"T:\a\..\b"), Some(r"\\?\C:\host\root\b".to_owned()));
        assert_eq!(
            tr(r"G:\a\..\..\b"),
            Some(r"\\?\UNC\server\share\game\b".to_owned())
        );
        // A name that only starts with dots is a name.
        assert_eq!(tr(r"T:\...\x"), Some(r"C:\host\root\...\x".to_owned()));
    }

    #[test]
    fn translate_takes_the_long_form_from_248_characters() {
        // Root and tail together, one short of the limit and at it.
        let fits = "d".repeat(LONG - ROOT.len() - 2);
        assert_eq!(tr(&format!(r"T:\{fits}")), Some(format!(r"{ROOT}\{fits}")));
        let long = format!("{fits}d");
        assert_eq!(
            tr(&format!(r"T:\{long}")),
            Some(format!(r"\\?\{ROOT}\{long}"))
        );
        // The long form is cleaned up as the host would the short one.
        let long = "d".repeat(250);
        assert_eq!(
            tr(&format!("T:/a/./b/../{long}//f")),
            Some(format!(r"\\?\{ROOT}\a\{long}\f"))
        );
    }

    #[test]
    fn verbatim_table() {
        for (root, tail, expected) in [
            (r"C:\r", "a/b", r"\\?\C:\r\a\b"),
            (r"C:\r", "", r"\\?\C:\r"),
            (r"C:\r", r"\a\.\b\\c\", r"\\?\C:\r\a\b\c"),
            (r"C:\r", r"a\..\b", r"\\?\C:\r\b"),
            // `..` stops at the root.
            (r"C:\r", r"..\..\a\..\..\b", r"\\?\C:\r\b"),
            // A root that is verbatim already keeps its form.
            (r"\\?\C:\r", "/a", r"\\?\C:\r\a"),
            // A share takes the UNC form.
            (r"\\server\share\", "/x/../../y", r"\\?\UNC\server\share\y"),
            (r"\\server\share", "x", r"\\?\UNC\server\share\x"),
        ] {
            assert_eq!(
                String::from_utf16(&verbatim(&w(root), &w(tail))).unwrap(),
                expected,
                "{root:?} + {tail:?}"
            );
        }
    }

    /// `host` against the table of [`tr`], back to the title's path.
    fn back(host: &str) -> Option<String> {
        let drives = [
            (u16::from(b'T'), w(ROOT)),
            (u16::from(b'G'), w(r"\\server\share\game")),
            (u16::from(b'S'), w(r"C:\host\root\saves")),
        ];
        untranslate(&drives, &w(host)).map(|path| String::from_utf16(&path).unwrap())
    }

    #[test]
    fn host_roots_map_back_to_drives() {
        let some = |s: &str| Some(s.to_owned());
        assert_eq!(back(r"C:\host\root"), some(r"T:\"));
        assert_eq!(back(r"C:\host\root\"), some(r"T:\"));
        assert_eq!(back(r"C:\host\root\a\b.txt"), some(r"T:\a\b.txt"));
        // As the host compares ASCII names, and with either separator.
        assert_eq!(back(r"c:/HOST/Root/a"), some(r"T:\a"));
        // A trailing separator is kept: it says "folder".
        assert_eq!(back(r"C:\host\root\tmp\"), some(r"T:\tmp\"));
        // The form GetFinalPathNameByHandleW answers in, also for a share.
        assert_eq!(back(r"\\?\C:\host\root\a"), some(r"T:\a"));
        assert_eq!(back(r"\\?\UNC\server\share\game\a"), some(r"G:\a"));
        assert_eq!(back(r"\\server\share\game"), some(r"G:\"));
        // The deepest mounted folder wins.
        assert_eq!(back(r"C:\host\root\saves\slot1"), some(r"S:\slot1"));
        assert_eq!(back(r"C:\host\root\savesx"), some(r"T:\savesx"));
    }

    #[test]
    fn host_paths_outside_every_drive_stay_host_paths() {
        for host in [
            "",
            r"C:\",
            r"C:\host",
            // A folder whose name only starts like the root's.
            r"C:\host\rootx\a",
            r"D:\host\root\a",
            r"\\server\share",
            r"\\server\share\gamex",
            r"\\?\C:\host",
            "relative",
        ] {
            assert_eq!(back(host), None, "{host:?}");
        }
        assert_eq!(untranslate(&[], &w(r"C:\host\root")), None);
        // Letters outside ASCII are compared as written.
        let drives = [(u16::from(b'T'), w("C:\\\u{e9}"))];
        assert_eq!(untranslate(&drives, &w("C:\\\u{c9}\\a")), None);
        assert_eq!(untranslate(&drives, &w("C:\\\u{e9}\\a")), Some(w(r"T:\a")));
    }

    /// What `translate` makes of a title path maps back to it.
    #[test]
    fn untranslate_reverses_translate() {
        let drives = [
            (u16::from(b'T'), w(ROOT)),
            (u16::from(b'G'), w(r"\\server\share\game")),
        ];
        let long = format!(r"T:\{}\f", "d".repeat(250));
        for path in [r"T:\", r"T:\a\b.txt", r"G:\data.bin", long.as_str()] {
            let host = translate(&drives, &w(path)).unwrap();
            let host = &host[..host.len() - 1];
            assert_eq!(untranslate(&drives, host), Some(w(path)), "{path}");
        }
    }
}
