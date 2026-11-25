//! Persistent local storage: a host folder of the title's own
//! (`runtime::Title::storage`, under the data directory, never the cache)
//! with the quota its manifest declares, seen by the title as a drive
//! (`files`).
//!
//! The quota is reported, not enforced: the space queries and the prompt
//! answer from it, but a write that takes the folder past it succeeds as
//! far as the host disk allows. Which call fails on the console when the
//! quota is full, and with which error, is not established by anything in
//! this repository, so no write is refused here on a guess. The account
//! stays truthful past the quota: what is used is what is there, and
//! nothing is free.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::path::Path;

use super::xasync::{self, AsyncBlock, E_ABORT};
use super::xgameruntime::{
    Guid, HRESULT, Interface, Methods, NOT_SUPPORTED, Object, S_OK, add_ref, put, put_string,
    query_interface,
};
use crate::runtime;

pub const PROVIDER: Guid = Guid::new(
    0xf4fa_f4d4,
    0x2d04,
    0x4fce,
    [0xb3, 0xe0, 0x47, 0x4a, 0x71, 0x3a, 0x3e, 0x84],
);
pub const IID: Guid = Guid::new(
    0xd294_11df,
    0x0794,
    0x4553,
    [0x8b, 0x27, 0x95, 0xfc, 0x02, 0xd0, 0xf7, 0x5d],
);

/// As Grand Theft Auto VI (2026) asks for it; it calls 0x20 (path) and 0x28
/// (space) with the same arguments.
pub const IID_2026: Guid = Guid::new(
    0x41a4_e10c,
    0x5a7e,
    0x41d9,
    [0x8c, 0x37, 0x37, 0xbd, 0xe6, 0x2a, 0x07, 0xd6],
);

pub static OBJECT: Object = Object::new("persistent local storage", &VTABLE);
pub const INTERFACES: &[Interface] = &[(PROVIDER, IID, &OBJECT), (PROVIDER, IID_2026, &OBJECT)];

const E_FAIL: HRESULT = 0x8000_4005_u32 as i32;

type This = *const Object;

/// The path titles get: the root of a virtual drive over the folder. On the
/// console storage is a drive of its own, and titles rely on it: Minecraft
/// keeps only what precedes the path's last `:\` and mounts that as a
/// drive, so a host folder such as `C:\Users\..` would make it write to the
/// root of `C:`. None (the path calls fail) before the title is set.
fn folder() -> Option<String> {
    let storage = runtime::title().storage;
    if storage.as_os_str().is_empty() {
        return None;
    }
    #[cfg(windows)]
    return super::files::mount(&storage).map(|letter| format!("{letter}:\\"));
    #[cfg(not(windows))]
    None
}

extern "system" fn path_size(_: This, out: *mut usize) -> HRESULT {
    match folder() {
        Some(path) => put(out, path.len() + 1),
        None => NOT_SUPPORTED,
    }
}

/// A buffer too small for the path fails with `E_FAIL`, the documented
/// Xbox result (a PC gets `ERROR_INSUFFICIENT_BUFFER`).
extern "system" fn path(_: This, size: usize, buffer: *mut u8, used: *mut usize) -> HRESULT {
    match folder() {
        Some(path) if size <= path.len() => E_FAIL,
        Some(path) => put_string(&path, size, buffer, used),
        None => NOT_SUPPORTED,
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpaceInfo {
    /// Writable now.
    pub available_free: u64,
    /// Left in the quota.
    pub total_free: u64,
    pub used: u64,
    /// The quota.
    pub total: u64,
}

/// The bytes of the files under `dir`. Links are not followed and count
/// nothing: a link to a folder (a symbolic link, a junction) can lead back
/// into the tree or out of the title's storage, and following it would
/// count files twice, or without end. Without links a folder tree is
/// finite; it is walked with a list, not by recursion, so its depth costs
/// no stack. The sum stops at `u64::MAX`.
fn used_bytes(dir: &Path) -> u64 {
    let mut used = 0u64;
    let mut folders = vec![dir.to_owned()];
    while let Some(folder) = folders.pop() {
        let Ok(entries) = std::fs::read_dir(&folder) else {
            continue;
        };
        for entry in entries.flatten() {
            // The entry's own type and size: `metadata` would follow links.
            match std::fs::symlink_metadata(entry.path()) {
                Ok(m) if m.file_type().is_symlink() => {}
                Ok(m) if m.is_dir() => folders.push(entry.path()),
                Ok(m) => used = total([used, m.len()]),
                Err(_) => {}
            }
        }
    }
    used
}

/// A sum of sizes that stops at `u64::MAX`.
fn total(sizes: impl IntoIterator<Item = u64>) -> u64 {
    sizes.into_iter().fold(0, u64::saturating_add)
}

/// What the host disk under `dir` has free (unlimited if unknown).
pub(crate) fn disk_free(dir: &Path) -> u64 {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
        let mut free = 0;
        let null = std::ptr::null_mut();
        // SAFETY: a NUL-terminated path and a valid output.
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free,
                null,
                null,
            )
        };
        if ok != 0 {
            return free;
        }
    }
    let _ = dir;
    u64::MAX
}

fn space() -> SpaceInfo {
    let title = runtime::title();
    space_of(&title.storage, title.storage_bytes)
}

/// The account of the folder `storage` under a quota of `quota` bytes. A
/// folder that holds more than its quota has nothing free; it never has
/// "less than nothing", which an unsigned number would show as a huge
/// amount.
fn space_of(storage: &Path, quota: u64) -> SpaceInfo {
    let used = used_bytes(storage);
    let free = quota.saturating_sub(used);
    SpaceInfo {
        // Writable now: the quota's rest, as far as the host disk allows.
        available_free: free.min(disk_free(storage)),
        total_free: free,
        used,
        total: quota,
    }
}

extern "system" fn space_info(_: This, out: *mut SpaceInfo) -> HRESULT {
    put(out, space())
}

/// Tells our prompts apart from other calls on the same block.
static PROMPT: u8 = 0;

/// There is nobody to ask: granted when the quota has room, else declined.
extern "system" fn prompt_for_space(_: This, bytes: u64, block: *mut AsyncBlock) -> HRESULT {
    let hr = if bytes <= space().total_free {
        S_OK
    } else {
        E_ABORT
    };
    xasync::finished(block, (&raw const PROMPT).cast(), hr)
}

extern "system" fn prompt_result(_: This, block: *mut AsyncBlock) -> HRESULT {
    let none = std::ptr::null_mut();
    xasync::XAsyncGetResult(block, (&raw const PROMPT).cast(), 0, none, none.cast())
}

static VTABLE: Methods<8> = Methods([
    query_interface as *const (),
    add_ref as *const (),
    add_ref as *const (),
    path_size as *const (),        // 0x18
    path as *const (),             // 0x20
    space_info as *const (),       // 0x28
    prompt_for_space as *const (), // 0x30
    prompt_result as *const (),    // 0x38
]);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::xgameruntime::method;

    #[test]
    // Reaching the folder goes through the Windows file layer.
    #[cfg(windows)]
    fn serves_a_drive_over_the_title_folder_and_its_quota() {
        type Size = extern "system" fn(This, *mut usize) -> HRESULT;
        type Get = extern "system" fn(This, usize, *mut u8, *mut usize) -> HRESULT;
        type Space = extern "system" fn(This, *mut SpaceInfo) -> HRESULT;
        type Prompt = extern "system" fn(This, u64, *mut AsyncBlock) -> HRESULT;
        type Result = extern "system" fn(This, *mut AsyncBlock) -> HRESULT;
        let this: This = &OBJECT;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("save.dat"), [0u8; 100]).unwrap();
        runtime::set_title(runtime::Title {
            id: 1,
            storage: dir.path().to_owned(),
            storage_bytes: 1000,
            ..Default::default()
        });

        // The path is a drive root, never the host folder.
        let mut size = 0;
        assert_eq!(method::<Size>(&OBJECT, 0x18)(this, &mut size), S_OK);
        assert_eq!(size, 4, "X:\\ and its NUL");
        let (mut buffer, mut used) = ([0u8; 4], 0);
        let get = method::<Get>(&OBJECT, 0x20);
        assert_eq!(get(this, 4, buffer.as_mut_ptr(), &mut used), S_OK);
        assert_eq!(get(this, 3, buffer.as_mut_ptr(), &mut used), E_FAIL);
        let drive = std::str::from_utf8(&buffer[..3]).unwrap();
        assert!(drive.ends_with(":\\"), "{drive}");
        // Title file calls on it reach the folder.
        let path = format!("{drive}save.dat\0");
        let mut attributes = [0u8; 36];
        let get_attributes = super::super::files::GetFileAttributesExW;
        let wide: Vec<u16> = path.encode_utf16().collect();
        assert_ne!(
            get_attributes(wide.as_ptr(), 0, attributes.as_mut_ptr().cast()),
            0
        );

        let mut info = SpaceInfo {
            available_free: 0,
            total_free: 0,
            used: 0,
            total: 0,
        };
        assert_eq!(method::<Space>(&OBJECT, 0x28)(this, &mut info), S_OK);
        assert_eq!((info.used, info.total_free, info.total), (100, 900, 1000));

        let prompt = method::<Prompt>(&OBJECT, 0x30);
        let result = method::<Result>(&OBJECT, 0x38);
        for (bytes, expect) in [(900, S_OK), (901, E_ABORT)] {
            let mut block = AsyncBlock {
                queue: std::ptr::null(),
                context: std::ptr::null_mut(),
                callback: None,
                internal: [0; 4],
            };
            assert_eq!(prompt(this, bytes, &mut block), S_OK);
            assert_eq!(xasync::XAsyncGetStatus(&mut block, true), expect);
            assert_eq!(result(this, &mut block), expect, "{bytes} bytes");
        }
    }

    #[test]
    fn free_space_is_the_quota_minus_use() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("top.dat"), [0u8; 100]).unwrap();
        std::fs::write(dir.path().join("a/b/deep.dat"), [0u8; 50]).unwrap();
        let info = space_of(dir.path(), 1000);
        assert_eq!((info.used, info.total_free, info.total), (150, 850, 1000));
        assert!(info.available_free <= 850, "{info:?}");
        // No quota (a title without a manifest entry): nothing is free.
        let none = space_of(dir.path(), 0);
        assert_eq!(
            (none.used, none.total_free, none.available_free),
            (150, 0, 0)
        );
        // A folder that is not there holds nothing.
        let absent = space_of(&dir.path().join("absent"), 1000);
        assert_eq!((absent.used, absent.total_free), (0, 1000));
    }

    /// The quota is not enforced on writes (see the module's first lines):
    /// a write past it succeeds, and the account then shows the real use
    /// and nothing free, not a difference that wrapped.
    #[test]
    fn a_write_past_the_quota_succeeds_and_leaves_nothing_free() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("save.dat"), [0u8; 900]).unwrap();
        assert_eq!(space_of(dir.path(), 1000).total_free, 100);
        std::fs::write(dir.path().join("more.dat"), [0u8; 1100]).unwrap();
        let info = space_of(dir.path(), 1000);
        assert_eq!(
            (info.used, info.total_free, info.available_free, info.total),
            (2000, 0, 0, 1000)
        );
    }

    /// The same through the calls a title makes: the file layer's
    /// `CreateFileW` on the storage drive and the host's write.
    #[test]
    #[cfg(windows)]
    fn a_title_write_past_the_quota_succeeds_and_leaves_nothing_free() {
        use super::super::files;
        use std::io::Write;
        use std::os::windows::io::FromRawHandle;
        const GENERIC_WRITE: u32 = 0x4000_0000;
        const CREATE_ALWAYS: u32 = 2;
        let dir = tempfile::tempdir().unwrap();
        let letter = files::mount(dir.path()).expect("a free drive letter");
        let path: Vec<u16> = format!("{letter}:\\big.dat\0").encode_utf16().collect();
        let null = std::ptr::null_mut();
        let file = files::CreateFileW(
            path.as_ptr(),
            GENERIC_WRITE,
            0,
            null,
            CREATE_ALWAYS,
            0,
            null,
        );
        assert_ne!(file as isize, -1);
        // SAFETY: a file handle opened above and owned from here on.
        let mut file = unsafe { std::fs::File::from_raw_handle(file) };
        let written = file.write_all(&[7u8; 2000]);
        drop(file);
        assert_eq!(
            written.map_err(|e| e.kind()),
            Ok(()),
            "the write is not refused"
        );
        files::unmount(letter);
        let info = space_of(dir.path(), 1000);
        assert_eq!(
            (info.used, info.total_free, info.available_free, info.total),
            (2000, 0, 0, 1000)
        );
    }

    #[test]
    fn a_sum_of_sizes_stops_at_the_largest_number() {
        assert_eq!(total([1, 2, 3]), 6);
        assert_eq!(total([u64::MAX, 1]), u64::MAX);
        assert_eq!(total([u64::MAX - 1, 1, 1]), u64::MAX);
        assert_eq!(total([]), 0);
    }

    /// A link to a folder inside the tree, leading back to the tree's
    /// root. Followed, it has every file counted again on each turn, until
    /// the host refuses the path.
    #[test]
    fn a_link_loop_does_not_hang_the_size_walk() {
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("inner");
        std::fs::create_dir(&inner).unwrap();
        std::fs::write(dir.path().join("save.dat"), [0u8; 100]).unwrap();
        std::fs::write(inner.join("more.dat"), [0u8; 20]).unwrap();
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(dir.path(), inner.join("loop"));
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_dir(dir.path(), inner.join("loop"));
        if let Err(error) = linked {
            // Windows wants a privilege or developer mode for links.
            return crate::testing::skip(&format!("no folder links here: {error}"));
        }
        assert!(
            inner.join("loop/inner/more.dat").is_file(),
            "the link works"
        );
        assert_eq!(used_bytes(dir.path()), 120);
        // A link to a file counts nothing either: its target is counted
        // where it lies, or lies outside the title's storage.
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path().join("save.dat"), inner.join("alias")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(dir.path().join("save.dat"), inner.join("alias"))
            .unwrap();
        assert_eq!(used_bytes(dir.path()), 120);
    }
}
