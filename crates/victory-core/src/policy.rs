//! Rule #1, enforced at runtime: game-derived data never lands in the repo.
//!
//! Everything VIctory produces from the user's files (converted assets,
//! prepared images, shader caches, crash dumps) is as much "from the game"
//! as the files themselves. Every tool that writes such output must call
//! [`ensure_outside_repo`] on the destination first. The static half of the
//! rule is `cargo xtask policy`, which checks what git tracks.

use std::path::{Component, Path, PathBuf};
use std::sync::Once;

/// Marker file at the root of the source tree.
pub const ROOT_MARKER: &str = ".victory-root";

/// The source tree this binary was compiled from.
fn compiled_workspace_root() -> PathBuf {
    // crates/victory-core -> workspace root
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error(
        "refusing to use {path} for game-derived data: it is inside the VIctory source tree \
         ({root}). Keep game files, caches and outputs outside the repository \
         (see docs/asset-policy.md)"
    )]
    InsideRepo { path: PathBuf, root: PathBuf },
    #[error("cannot resolve {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Returns `Ok` only if `path` is outside every VIctory source tree.
///
/// `path` does not have to exist yet. These checks are made:
/// 1. it is not below the source tree this binary was built from;
/// 2. none of its ancestors contains the [`ROOT_MARKER`] file (catches
///    other clones/copies of the repository);
/// 3. under Wine, none of the ancestors of its host (unix) path contains
///    the marker either. A Windows path hides what is above its drive: a
///    Wine prefix inside the source tree makes all of `C:` part of the
///    tree, and a host symlink is an ordinary folder to Windows.
pub fn ensure_outside_repo(path: &Path) -> Result<(), PolicyError> {
    static REPORTED: Once = Once::new();
    ensure_outside(path, &compiled_workspace_root(), &marker_file, &REPORTED)
}

/// Says whether a folder is the root of a VIctory source tree.
type Marked<'a> = &'a dyn Fn(&Path) -> bool;

/// What says so outside the tests: the folder holds the [`ROOT_MARKER`]
/// file. The tests pass a check that sees only the folders they made, so
/// that where the machine keeps its temporary folder decides nothing.
fn marker_file(dir: &Path) -> bool {
    dir.join(ROOT_MARKER).is_file()
}

/// [`ensure_outside_repo`] with the compiled-in source tree and the marker
/// check passed in, and with what remembers that check 1 could not be
/// made: that is said once per `reported`, which is once per process for
/// the public function.
fn ensure_outside(
    path: &Path,
    compiled_root: &Path,
    marked: Marked<'_>,
    reported: &Once,
) -> Result<(), PolicyError> {
    if let CompiledRoot::Unreadable(kind) = check(path, compiled_root, marked)? {
        reported.call_once(|| report_unreadable_root(compiled_root, kind));
    }
    Ok(())
}

/// What became of check 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompiledRoot {
    /// The tree this binary was built from is here and `path` is outside it.
    Outside,
    /// The tree is here but is no VIctory source tree any more (no marker).
    Unmarked,
    /// The tree could not be looked at, so `path` was not compared with it.
    Unreadable(std::io::ErrorKind),
}

/// Says that check 1 could not be made.
///
/// A tree that is not there is the normal case for a binary that runs on
/// another machine, or under Wine on another drive than the tree's (where
/// check 3 covers it), and is only logged for debugging. Anything else,
/// such as a tree that may not be read, is a warning: the path was not
/// compared with the tree and only the marker checks stand.
fn report_unreadable_root(root: &Path, kind: std::io::ErrorKind) {
    if kind == std::io::ErrorKind::NotFound {
        tracing::debug!(
            root = %root.display(),
            "asset policy: the source tree this binary was built from is not here"
        );
    } else {
        tracing::warn!(
            root = %root.display(),
            error = %kind,
            "asset policy: cannot read the source tree this binary was built from; \
             paths are only checked for the {ROOT_MARKER} marker"
        );
    }
}

/// [`ensure_outside_repo`] with the compiled-in source tree and the marker
/// check passed in. Check 3 (Wine) always asks the host itself.
fn check(
    path: &Path,
    compiled_root: &Path,
    marked: Marked<'_>,
) -> Result<CompiledRoot, PolicyError> {
    let resolved = resolve(path)?;
    let inside = |root: &Path| PolicyError::InsideRepo {
        path: path.to_path_buf(),
        root: root.to_path_buf(),
    };

    let compiled = match std::fs::canonicalize(compiled_root) {
        Ok(root) if !marked(&root) => CompiledRoot::Unmarked,
        Ok(root) if resolved.starts_with(&root) => return Err(inside(&root)),
        Ok(_) => CompiledRoot::Outside,
        Err(e) => CompiledRoot::Unreadable(e.kind()),
    };

    if let Some(root) = resolved.ancestors().find(|dir| marked(dir)) {
        return Err(inside(root));
    }

    #[cfg(windows)]
    if let Some(unix) = resolved.ancestors().find_map(wine::unix_file_name) {
        // The host path of the longest part of `resolved` that Wine can
        // name; what is below it does not exist and holds no marker.
        if !wine::is_dir("/") {
            return Err(PolicyError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::other(
                    "this Wine does not show host paths, so the path cannot be checked \
                     against the source tree",
                ),
            });
        }
        if let Some(root) = unix_marker_root(&unix, wine::has_marker) {
            return Err(inside(Path::new(root)));
        }
    }

    Ok(compiled)
}

/// The nearest ancestor of the absolute unix path `unix` (the path itself
/// included) for which `has_marker` holds.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn unix_marker_root(unix: &str, has_marker: impl Fn(&str) -> bool) -> Option<&str> {
    let mut dir = unix.trim_end_matches('/');
    loop {
        // What is left of "/" itself is the empty string.
        let shown = if dir.is_empty() { "/" } else { dir };
        if has_marker(shown) {
            return Some(shown);
        }
        if dir.is_empty() {
            return None;
        }
        dir = dir[..dir.rfind('/')?].trim_end_matches('/');
    }
}

/// Wine's view of the host file system, for [`check`].
#[cfg(windows)]
mod wine {
    use std::ffi::{CStr, c_char, c_void};
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    use super::ROOT_MARKER;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleW(name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
        fn GetProcessHeap() -> *mut c_void;
        fn HeapFree(heap: *mut c_void, flags: u32, memory: *mut c_void) -> i32;
    }

    /// Wine's `char *wine_get_unix_file_name(const WCHAR *dos)`.
    type GetUnixFileName = unsafe extern "C" fn(*const u16) -> *mut c_char;

    /// Wine's kernel32 export, or `None` where this is not Wine.
    fn get_unix_file_name() -> Option<GetUnixFileName> {
        let kernel32: Vec<u16> = "kernel32.dll\0".encode_utf16().collect();
        // SAFETY: both names are NUL-terminated, and kernel32 is loaded in
        // every process and never unloaded.
        let export = unsafe {
            let module = GetModuleHandleW(kernel32.as_ptr());
            if module.is_null() {
                return None;
            }
            GetProcAddress(module, c"wine_get_unix_file_name".as_ptr())
        };
        if export.is_null() {
            return None;
        }
        // SAFETY: Wine exports the function under this name with this
        // signature, and no other kernel32 has an export of this name.
        Some(unsafe { std::mem::transmute::<*mut c_void, GetUnixFileName>(export) })
    }

    /// Whether this process runs under Wine.
    #[cfg(test)]
    pub fn present() -> bool {
        get_unix_file_name().is_some()
    }

    /// The host path of `path` with host symlinks resolved, or `None` where
    /// this is not Wine or Wine cannot name the path (it names a path whose
    /// folder exists).
    pub fn unix_file_name(path: &Path) -> Option<String> {
        let convert = get_unix_file_name()?;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        if wide[..wide.len() - 1].contains(&0) {
            return None;
        }
        // SAFETY: `wide` is NUL-terminated and lives across the call.
        let unix = unsafe { convert(wide.as_ptr()) };
        if unix.is_null() {
            return None;
        }
        // SAFETY: Wine returns a NUL-terminated string that it allocated on
        // the process heap and that the caller frees there.
        let text = unsafe {
            let text = CStr::from_ptr(unix).to_string_lossy().into_owned();
            HeapFree(GetProcessHeap(), 0, unix.cast());
            text
        };
        text.starts_with('/').then_some(text)
    }

    /// The absolute unix path `unix` as a path Wine opens, through its
    /// `\\?\unix` name for the host's root.
    pub fn host_path(unix: &str) -> PathBuf {
        let mut path = String::from(r"\\?\unix");
        for part in unix.split('/').filter(|part| !part.is_empty()) {
            path.push('\\');
            path.push_str(part);
        }
        if path.len() == r"\\?\unix".len() {
            path.push('\\');
        }
        PathBuf::from(path)
    }

    pub fn is_dir(unix: &str) -> bool {
        host_path(unix).is_dir()
    }

    pub fn has_marker(unix_dir: &str) -> bool {
        host_path(unix_dir).join(ROOT_MARKER).is_file()
    }
}

/// Canonicalizes the longest existing prefix of `path` and re-appends the
/// rest, so that paths which do not exist yet can still be compared.
fn resolve(path: &Path) -> Result<PathBuf, PolicyError> {
    resolve_at(path, 0)
}

fn resolve_at(path: &Path, depth: u32) -> Result<PathBuf, PolicyError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|source| PolicyError::Io {
                path: path.to_path_buf(),
                source,
            })?
            .join(path)
    };

    let mut existing = absolute.as_path();
    let mut rest = Vec::new();
    loop {
        match std::fs::canonicalize(existing) {
            Ok(canon) => {
                let mut out = canon;
                for component in rest.iter().rev() {
                    out.push(component);
                }
                return Ok(normalize(&out));
            }
            Err(source) => match (existing.parent(), existing.file_name()) {
                (Some(parent), Some(name)) => {
                    // A dangling symlink does not canonicalize, yet creating
                    // a file through it creates the file at its target.
                    if let Ok(target) = std::fs::read_link(existing) {
                        if depth >= 40 {
                            return Err(PolicyError::Io {
                                path: path.to_path_buf(),
                                source,
                            });
                        }
                        let mut out = resolve_at(&parent.join(target), depth + 1)?;
                        for component in rest.iter().rev() {
                            out.push(component);
                        }
                        return Ok(normalize(&out));
                    }
                    rest.push(name.to_os_string());
                    existing = parent;
                }
                _ => {
                    return Err(PolicyError::Io {
                        path: path.to_path_buf(),
                        source,
                    });
                }
            },
        }
    }
}

/// Removes `.` and resolves `..` lexically (only used on the non-existing tail).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt;

    /// A marker check that sees the folders of one test only: the marker
    /// file counts in `below` and under it and nowhere else. With it a
    /// test's verdicts follow from what the test made, not from where this
    /// machine keeps its temporary folder (a clone above it would otherwise
    /// refuse every path the tests expect to be accepted).
    fn marked_below(below: &Path) -> impl Fn(&Path) -> bool {
        let below = std::fs::canonicalize(below).unwrap();
        move |dir| dir.starts_with(&below) && marker_file(dir)
    }

    #[test]
    fn rejects_paths_inside_this_repo() {
        let inside = compiled_workspace_root().join("cache").join("textures");
        assert!(matches!(
            ensure_outside_repo(&inside),
            Err(PolicyError::InsideRepo { .. })
        ));
        let sneaky = std::env::temp_dir().join("..").join("x"); // sanity: no panic
        let _ = ensure_outside_repo(&sneaky);
    }

    #[test]
    fn rejects_dotdot_escapes_back_into_repo() {
        let root = compiled_workspace_root();
        let path = root
            .join("crates")
            .join("..")
            .join("not-yet-created")
            .join("out");
        assert!(ensure_outside_repo(&path).is_err());
    }

    #[test]
    fn rejects_other_clones_via_marker() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(ROOT_MARKER), "").unwrap();
        assert!(ensure_outside_repo(&tmp.path().join("cache")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_dangling_symlink_into_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let target = compiled_workspace_root()
            .join("not-yet-created")
            .join("out");
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(matches!(
            ensure_outside_repo(&link),
            Err(PolicyError::InsideRepo { .. })
        ));
        assert!(matches!(
            ensure_outside_repo(&link.join("sub")),
            Err(PolicyError::InsideRepo { .. })
        ));
    }

    /// Collects the log events of the code a test runs.
    struct Events(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl tracing::Subscriber for Events {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            struct Text(String);
            impl tracing::field::Visit for Text {
                fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
                    use fmt::Write;
                    let _ = write!(self.0, " {}={value:?}", field.name());
                }
            }
            let mut text = Text(event.metadata().level().to_string());
            event.record(&mut text);
            self.0.lock().unwrap().push(text.0);
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// What `run` logs on this thread, one "LEVEL field=value ..." each.
    fn events_of(run: impl FnOnce()) -> Vec<String> {
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        tracing::subscriber::with_default(Events(events.clone()), run);
        events.lock().unwrap().clone()
    }

    #[test]
    fn unreadable_compiled_root_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let marked = marked_below(tmp.path());
        let gone = tmp.path().join("no-such-tree");
        let path = tmp.path().join("cache");
        assert_eq!(
            check(&path, &gone, &marked).unwrap(),
            CompiledRoot::Unreadable(std::io::ErrorKind::NotFound)
        );
        // It is said, once for each `Once`, and a tree that is merely not
        // here is not worth a warning.
        let reported = Once::new();
        let events = events_of(|| {
            ensure_outside(&path, &gone, &marked, &reported).unwrap();
            ensure_outside(&path.join("deeper"), &gone, &marked, &reported).unwrap();
        });
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(events[0].starts_with("DEBUG "), "{events:?}");
        assert!(
            events[0].contains("is not here") && events[0].contains("no-such-tree"),
            "{events:?}"
        );
        let again = events_of(|| ensure_outside(&path, &gone, &marked, &Once::new()).unwrap());
        assert_eq!(again.len(), 1, "another `Once` reports again: {again:?}");
        // A tree that was compared has nothing to report.
        let tree = tmp.path().join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        for with_marker in [false, true] {
            if with_marker {
                std::fs::write(tree.join(ROOT_MARKER), "").unwrap();
            }
            let events = events_of(|| ensure_outside(&path, &tree, &marked, &Once::new()).unwrap());
            assert_eq!(events, Vec::<String>::new(), "marked: {with_marker}");
        }
        // The marker check still stands without the compiled-in tree.
        std::fs::write(tmp.path().join(ROOT_MARKER), "").unwrap();
        assert!(matches!(
            check(&path, &gone, &marked),
            Err(PolicyError::InsideRepo { .. })
        ));
    }

    /// A tree that is there but cannot be looked at is a warning: here its
    /// path leads through a file.
    #[cfg(unix)]
    #[test]
    fn compiled_root_that_cannot_be_read_is_a_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let marked = marked_below(tmp.path());
        std::fs::write(tmp.path().join("file"), "").unwrap();
        let root = tmp.path().join("file").join("tree");
        let path = tmp.path().join("cache");
        let kind = match check(&path, &root, &marked).unwrap() {
            CompiledRoot::Unreadable(kind) => kind,
            other => panic!("expected Unreadable, got {other:?}"),
        };
        assert_ne!(kind, std::io::ErrorKind::NotFound);
        let reported = Once::new();
        let events = events_of(|| {
            ensure_outside(&path, &root, &marked, &reported).unwrap();
            ensure_outside(&path, &root, &marked, &reported).unwrap();
        });
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(events[0].starts_with("WARN "), "{events:?}");
        assert!(
            events[0].contains("cannot read the source tree")
                && events[0].contains(ROOT_MARKER)
                && events[0].contains(&root.display().to_string()),
            "{events:?}"
        );
        // The path is still refused where a marker says so, without a word.
        std::fs::write(tmp.path().join(ROOT_MARKER), "").unwrap();
        let events = events_of(|| {
            let refused = ensure_outside(&path, &root, &marked, &Once::new());
            assert!(matches!(refused, Err(PolicyError::InsideRepo { .. })));
        });
        assert_eq!(events, Vec::<String>::new());
    }

    #[test]
    fn compiled_root_is_compared_when_it_is_there() {
        let tmp = tempfile::tempdir().unwrap();
        let marked = marked_below(tmp.path());
        let tree = tmp.path().join("tree");
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&tree).unwrap();
        // A folder that is no source tree any more refuses nothing.
        assert_eq!(
            check(&tree.join("cache"), &tree, &marked).unwrap(),
            CompiledRoot::Unmarked
        );
        std::fs::write(tree.join(ROOT_MARKER), "").unwrap();
        assert_eq!(
            check(&elsewhere, &tree, &marked).unwrap(),
            CompiledRoot::Outside
        );
        match check(&tree.join("cache"), &tree, &marked) {
            Err(PolicyError::InsideRepo { root, .. }) => {
                assert_eq!(root, std::fs::canonicalize(&tree).unwrap());
            }
            other => panic!("expected InsideRepo, got {other:?}"),
        }
    }

    /// On a volume that ignores case, the tree under another spelling is
    /// the same tree.
    #[cfg(target_os = "macos")]
    #[test]
    fn differently_cased_repo_path_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let marked = marked_below(tmp.path());
        let tree = tmp.path().join("victory-tree");
        std::fs::create_dir_all(tree.join("sub")).unwrap();
        std::fs::write(tree.join(ROOT_MARKER), "").unwrap();
        let shouted = tmp.path().join("VICTORY-TREE");
        if !shouted.join(ROOT_MARKER).is_file() {
            crate::testing::skip("the temporary folder is on a case-sensitive volume");
            return;
        }
        // Compared with the tree (check 1) and found by its marker (check 2).
        for (path, root) in [
            (shouted.join("cache"), tree.clone()),
            (shouted.join("SUB").join("new").join("x"), tree.clone()),
            (shouted.join("cache"), tmp.path().join("no-such-tree")),
        ] {
            assert!(
                matches!(
                    check(&path, &root, &marked),
                    Err(PolicyError::InsideRepo { .. })
                ),
                "{} was accepted",
                path.display()
            );
        }
    }

    /// The marker is looked for in every ancestor of a host path, the
    /// path and the root included, nearest first.
    #[test]
    fn unix_ancestors_are_searched_for_the_marker() {
        let seen = std::cell::RefCell::new(Vec::new());
        let none = unix_marker_root("/a/b//c/", |dir| {
            seen.borrow_mut().push(dir.to_owned());
            false
        });
        assert_eq!(none, None);
        assert_eq!(*seen.borrow(), ["/a/b//c", "/a/b", "/a", "/"]);
        assert_eq!(unix_marker_root("/", |dir| dir == "/"), Some("/"));
        assert_eq!(unix_marker_root("/", |_| false), None);
        assert_eq!(unix_marker_root("/a/b", |dir| dir == "/a/b"), Some("/a/b"));
        assert_eq!(unix_marker_root("/a/b", |dir| dir == "/a"), Some("/a"));
        assert_eq!(unix_marker_root("/ab/c", |dir| dir == "/a"), None);
    }

    /// The launcher's old layout: the Wine prefix under `<tree>/target`, so
    /// that `C:\users\...\AppData\Local` is inside the tree while no
    /// ancestor of the Windows path shows it.
    #[test]
    fn prefix_inside_the_repo_is_refused() {
        let local_app_data = "/src/victory/target/wineprefix-play/drive_c/users/u/AppData/Local";
        let has_marker = |dir: &str| dir == "/src/victory";
        assert_eq!(
            unix_marker_root(&format!("{local_app_data}/victory/cache"), has_marker),
            Some("/src/victory")
        );
        let outside = "/Users/u/Library/Caches/victory-macos/prefix-play/drive_c/users/u";
        assert_eq!(unix_marker_root(outside, has_marker), None);
    }

    /// Runs a host program from inside Wine and waits until `done` holds.
    /// Wine starts host programs but hands back no process to wait for
    /// (the call returns "invalid handle" after starting it).
    #[cfg(windows)]
    fn host_run(program: &str, args: &[&str], done: impl Fn() -> bool) {
        let started = std::process::Command::new(wine::host_path(program))
            .args(args)
            .status();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !done() {
            assert!(
                std::time::Instant::now() < deadline,
                "the host's {program} {args:?} did not do its work (start: {started:?})"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// A host symlink is an ordinary folder to Windows, so no ancestor of
    /// the Windows path shows the tree that the files would land in. This
    /// is also what a Wine prefix inside the tree looks like from `C:`.
    #[cfg(windows)]
    #[test]
    fn host_path_inside_a_source_tree_is_refused() {
        if !wine::present() {
            crate::testing::skip("not Wine: kernel32 has no wine_get_unix_file_name");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let marked = marked_below(tmp.path());
        let tree = tmp.path().join("tree");
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(tree.join("out")).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(tree.join(ROOT_MARKER), "").unwrap();
        let unix = |p: &Path| wine::unix_file_name(p).expect("Wine names an existing folder");
        let link = elsewhere.join("link");
        // Named below its folder: Wine's name for the link itself is the
        // name of what it points at.
        let unix_link = format!("{}/link", unix(&elsewhere));
        host_run(
            "/bin/ln",
            &["-s", &unix(&tree.join("out")), &unix_link],
            || link.is_dir(),
        );

        let gone = tmp.path().join("no-such-tree");
        for path in [
            link.clone(),
            link.join("file"),
            link.join("new").join("deeper").join("file"),
        ] {
            match check(&path, &gone, &marked) {
                Err(PolicyError::InsideRepo { root, .. }) => {
                    assert_eq!(root, PathBuf::from(unix(&tree)), "{}", path.display());
                }
                other => panic!("{}: expected InsideRepo, got {other:?}", path.display()),
            }
        }
        // Beside the link nothing is refused.
        check(&elsewhere.join("file"), &gone, &marked).unwrap();
        // Windows cannot remove the link, and would empty the tree through it.
        host_run("/bin/rm", &[&unix_link], || !link.exists());
    }

    /// The tree this binary was built from, named through Wine's drive for
    /// the host's root and through a Windows-form path.
    #[cfg(windows)]
    #[test]
    fn repo_path_through_a_wine_drive_is_refused() {
        if !wine::present() {
            crate::testing::skip("not Wine: kernel32 has no wine_get_unix_file_name");
            return;
        }
        // A cross-compiled binary has the host's spelling of its tree.
        let manifest = env!("CARGO_MANIFEST_DIR");
        if !manifest.starts_with('/') {
            crate::testing::skip("built on Windows: the tree has no host path");
            return;
        }
        let root = normalize(&wine::host_path(manifest).join("..").join(".."));
        assert!(root.join(ROOT_MARKER).is_file(), "{}", root.display());
        let gone = Path::new(r"C:\no-such-tree");
        // The real tree, so the real markers.
        let marked = marker_file;
        // `\\?\unix\<tree>\cache\x` and whatever drive Wine shows it on.
        let through_unix = root.join("cache").join("x");
        let on_a_drive = std::fs::canonicalize(&root)
            .unwrap()
            .join("cache")
            .join("x");
        for path in [through_unix, on_a_drive] {
            assert!(
                matches!(
                    check(&path, gone, &marked),
                    Err(PolicyError::InsideRepo { .. })
                ),
                "{} was accepted",
                path.display()
            );
            assert!(matches!(
                ensure_outside_repo(&path),
                Err(PolicyError::InsideRepo { .. })
            ));
        }
    }

    /// Where the tools put derived data when nothing is configured: the
    /// defaults of an environment the test makes up, in folders it made,
    /// are accepted outside a source tree and refused inside one. (Under
    /// Wine check 3 still asks the host about the folders above the
    /// temporary one; `this_runs_own_folders_are_outside_every_source_tree`
    /// names that.)
    #[test]
    fn default_directories_are_judged_by_where_the_environment_puts_them() {
        use std::ffi::OsString;

        let tmp = tempfile::tempdir().unwrap();
        let marked = marked_below(tmp.path());
        let tree = tmp.path().join("tree");
        // What a Wine prefix or a home folder inside a clone looks like.
        let inside = tree.join("target").join("prefix").join("user");
        let outside = tmp.path().join("elsewhere").join("user");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(tree.join(ROOT_MARKER), "").unwrap();
        let root = std::fs::canonicalize(&tree).unwrap();

        // Each source of the defaults on its own, as `default_dir` reads
        // them: Windows and Wine, XDG, and a bare home folder.
        let sources: [&[&str]; 3] = [
            &["LOCALAPPDATA"],
            &["XDG_CACHE_HOME", "XDG_DATA_HOME"],
            &["HOME"],
        ];
        for vars in sources {
            let defaults = |base: &Path| {
                let value = OsString::from(base);
                crate::Config::defaults_of(|k| vars.contains(&k).then(|| value.clone())).unwrap()
            };
            let cfg = defaults(&outside);
            for dir in [&cfg.cache_dir, &cfg.data_dir] {
                assert!(dir.starts_with(&outside), "{vars:?}: {}", dir.display());
                assert_eq!(
                    check(dir, &tree, &marked).unwrap(),
                    CompiledRoot::Outside,
                    "{vars:?}: {}",
                    dir.display()
                );
                // The tree is found by its marker alone too.
                check(dir, &tmp.path().join("no-such-tree"), &marked).unwrap();
            }
            let cfg = defaults(&inside);
            for dir in [&cfg.cache_dir, &cfg.data_dir] {
                for compiled in [tree.clone(), tmp.path().join("no-such-tree")] {
                    match check(dir, &compiled, &marked) {
                        Err(PolicyError::InsideRepo { root: found, .. }) => {
                            assert_eq!(found, root, "{vars:?}: {}", dir.display());
                        }
                        other => panic!(
                            "{vars:?}: {} should be refused, got {other:?}",
                            dir.display()
                        ),
                    }
                }
            }
        }
    }

    /// A check of this machine and this run, not of the code: the user and
    /// temporary folders the tests and tools use here (under Wine: those of
    /// the Wine prefix) must lie outside every VIctory source tree. It
    /// fails with no code change where a home or temporary folder is inside
    /// a clone, or `WINEPREFIX` is; `tools/macos/test-wine.sh` keeps its
    /// prefix outside the repository for this reason.
    #[test]
    fn this_runs_own_folders_are_outside_every_source_tree() {
        let cfg = crate::Config::defaults_of(|k| {
            // Only what the defaults are made of, not the user's VICTORY_*.
            ["LOCALAPPDATA", "XDG_CACHE_HOME", "XDG_DATA_HOME", "HOME"]
                .contains(&k)
                .then(|| std::env::var_os(k))
                .flatten()
        })
        .expect("this run's environment names a user folder (HOME or LOCALAPPDATA)");
        let temporary = std::env::temp_dir().join("victory-test");
        for (what, dir) in [
            ("default cache folder", &cfg.cache_dir),
            ("default data folder", &cfg.data_dir),
            ("temporary folder", &temporary),
        ] {
            if let Err(error) = ensure_outside_repo(dir) {
                panic!(
                    "this machine's {what} ({}) cannot be used: {error}\n\
                     This is a precondition of the run, not a fault in the code: move HOME, \
                     LOCALAPPDATA, TMPDIR or WINEPREFIX out of the source tree.",
                    dir.display()
                );
            }
        }
    }

    #[test]
    fn accepts_unrelated_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let marked = marked_below(tmp.path());
        let tree = tmp.path().join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join(ROOT_MARKER), "").unwrap();
        // Beside a source tree, existing or not yet.
        for path in [
            tmp.path().join("does").join("not").join("exist"),
            tmp.path().to_path_buf(),
            tmp.path().join("tree-2").join("cache"),
        ] {
            assert_eq!(
                check(&path, &tree, &marked).unwrap(),
                CompiledRoot::Outside,
                "{}",
                path.display()
            );
            ensure_outside(&path, &tree, &marked, &Once::new()).unwrap();
        }
    }
}
