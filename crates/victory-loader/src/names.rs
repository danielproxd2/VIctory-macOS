//! Names a title's image gives us, before they are used: the DLL names of
//! its import tables (and of its `LoadLibrary` calls), which become file
//! paths. Pure, so that the rules are tested on every host.

use std::path::{Path, PathBuf};

/// Device names Windows reserves in every folder, with or without an
/// extension ("Naming Files, Paths, and Namespaces",
/// <https://learn.microsoft.com/en-us/windows/win32/fileio/naming-a-file>).
const DEVICES: [&str; 4] = ["con", "prn", "aux", "nul"];
const NUMBERED_DEVICES: [&str; 2] = ["com", "lpt"];

/// The file `name` in the title's folder `dir`, if `name` is the plain name
/// of a file: no folder part, no drive, nothing Windows would read as
/// another place or another file. `None` for everything else, which the
/// loader then leaves alone as it does a DLL the folder does not have.
///
/// A DLL name comes from the title's import table, so it is untrusted: as a
/// path it could name any file on the machine (`..\..\x.dll`,
/// `C:\Windows\x.dll`, `\\server\share\x.dll`).
///
/// Refused, after the rules of the page named above: the empty name; a
/// path separator (`\` or `/`); a colon (a drive or a stream of another
/// file); the other reserved characters `< > " | ? *` and the control
/// characters, NUL among them; a name that ends in a dot or a space (which
/// Windows drops, so `..` and `x.dll.` are other names than they look);
/// and the reserved device names.
pub fn title_dll_path(dir: &Path, name: &str) -> Option<PathBuf> {
    let reserved = |c: char| c < ' ' || r#"<>:"/\|?*"#.contains(c);
    if name.is_empty() || name.chars().any(reserved) || name.ends_with(['.', ' ']) {
        return None;
    }
    // A device name is one whatever follows its first dot (`NUL.dll`), and
    // Windows drops spaces before that dot too.
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    let stem = stem.to_ascii_lowercase();
    let numbered = NUMBERED_DEVICES.iter().any(|device| {
        let mut rest = stem.strip_prefix(device).unwrap_or("").chars();
        matches!(
            (rest.next(), rest.next()),
            (Some('0'..='9' | '\u{b9}' | '\u{b2}' | '\u{b3}'), None)
        )
    });
    if numbered || DEVICES.contains(&stem.as_str()) {
        return None;
    }
    Some(dir.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_dll_path_rejects_traversal_and_absolute_names() {
        let dir = Path::new("title");
        for refused in [
            "",
            ".",
            "..",
            "...",
            "..\\escape.dll",
            "../escape.dll",
            "sub\\x.dll",
            "sub/x.dll",
            "\\x.dll",
            "/x.dll",
            "\\\\server\\share\\x.dll",
            "\\\\?\\C:\\x.dll",
            "C:\\Windows\\System32\\x.dll",
            "C:x.dll",
            "c:",
            "x.dll:stream",
            "x.dll.",
            "x.dll ",
            ".. ",
            "x\0.dll",
            "x.dll\0",
            "x\n.dll",
            "x?.dll",
            "x*.dll",
            "<x>.dll",
            "\"x\".dll",
            "x|y.dll",
            "nul",
            "NUL.dll",
            "con.dll",
            "Prn.dll",
            "aux.tar.dll",
            "nul .dll",
            "COM1.dll",
            "com0",
            "lpt9.dll",
            "LPT\u{b9}.dll",
        ] {
            assert_eq!(title_dll_path(dir, refused), None, "{refused:?}");
        }
    }

    #[test]
    fn title_dll_path_keeps_plain_names_in_the_folder() {
        let dir = Path::new("title");
        for plain in [
            "x.dll",
            "X.DLL",
            "title_a.dll",
            "with space.dll",
            "two.dots.dll",
            "no-extension",
            ".hidden.dll",
            // Not device names: more than one digit, none, or more letters.
            "com10.dll",
            "com.dll",
            "lpt.dll",
            "console.dll",
            "null.dll",
            "auxiliary.dll",
        ] {
            let path = title_dll_path(dir, plain);
            assert_eq!(path, Some(dir.join(plain)), "{plain:?}");
            let path = path.unwrap();
            assert_eq!(path.parent(), Some(dir), "{plain:?}");
            assert_eq!(path.file_name().and_then(|n| n.to_str()), Some(plain));
        }
    }
}
