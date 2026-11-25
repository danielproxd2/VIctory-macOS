//! Virtual drives over host folders: on the console, title storage is a
//! drive of its own, and titles build paths on that assumption.
//!
//! The title imports that take a path are bound here: the Win32 and C
//! runtime functions `module` lists (a test names them one by one). A path
//! on a virtual drive (`X:\..`, `X:/..`, `X:`, `\\?\X:\..`) is rewritten to
//! the host folder behind it and handed to the host's own function; any
//! other path goes to the host unchanged. A rewritten narrow path goes to
//! the host's wide twin, so host folders outside the ANSI code page work;
//! never to our wide wrapper, which would rewrite the host path a second
//! time when its drive letter is mounted too. Letters are chosen among
//! those the host does not use, so no host drive is hidden.
//!
//! The other direction: where the host hands a path back (the current
//! directory, a full path, the path of an open file, the temporary
//! folder), a path inside a mounted folder is given to the title as the
//! path on its drive, so the title never learns the host folder from
//! those calls.
//!
//! Not bound, and so answered by the host as they are: the narrow
//! `FindFirstFile` family of the C runtime (`_findfirst64` and its
//! siblings), `GetTempPathA`, `GetFinalPathNameByHandleA`, the short and
//! long path-name conversions, and the transacted functions.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::too_many_arguments)]

pub(crate) mod paths;

use std::ffi::{CStr, c_char, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use windows_sys::Win32::Foundation::SetLastError;
use windows_sys::Win32::Globalization::{CP_ACP, MultiByteToWideChar, WideCharToMultiByte};
use windows_sys::Win32::Storage::FileSystem::{
    FindClose, GetLogicalDrives, WIN32_FIND_DATAA, WIN32_FIND_DATAW,
};

use self::paths::SEPARATORS;
use crate::{HleExport, HleModule, hle_export};

pub(crate) use crate::host::host;

/// Every host function this file binds with `host!`, sorted: checked
/// against the host at start-up (`crate::host::missing_exports`) and
/// against this file by a test.
pub(crate) const HOST_IMPORTS: &[(&str, &str)] = &[
    ("kernel32.dll", "CloseHandle"),
    ("kernel32.dll", "CopyFile2"),
    ("kernel32.dll", "CopyFileW"),
    ("kernel32.dll", "CreateDirectoryA"),
    ("kernel32.dll", "CreateDirectoryW"),
    ("kernel32.dll", "CreateFile2"),
    ("kernel32.dll", "CreateFileA"),
    ("kernel32.dll", "CreateFileW"),
    ("kernel32.dll", "CreateHardLinkW"),
    ("kernel32.dll", "DeleteFileA"),
    ("kernel32.dll", "DeleteFileW"),
    ("kernel32.dll", "FindFirstFileA"),
    ("kernel32.dll", "FindFirstFileExA"),
    ("kernel32.dll", "FindFirstFileExW"),
    ("kernel32.dll", "FindFirstFileW"),
    ("kernel32.dll", "GetCurrentDirectoryA"),
    ("kernel32.dll", "GetCurrentDirectoryW"),
    ("kernel32.dll", "GetDiskFreeSpaceExW"),
    ("kernel32.dll", "GetDiskFreeSpaceW"),
    ("kernel32.dll", "GetFileAttributesA"),
    ("kernel32.dll", "GetFileAttributesExA"),
    ("kernel32.dll", "GetFileAttributesExW"),
    ("kernel32.dll", "GetFileAttributesW"),
    ("kernel32.dll", "GetFinalPathNameByHandleW"),
    ("kernel32.dll", "GetFullPathNameA"),
    ("kernel32.dll", "GetFullPathNameW"),
    ("kernel32.dll", "GetTempPathW"),
    ("kernel32.dll", "GetVolumeInformationW"),
    ("kernel32.dll", "MoveFileExA"),
    ("kernel32.dll", "MoveFileExW"),
    ("kernel32.dll", "MoveFileW"),
    ("kernel32.dll", "ReadFile"),
    ("kernel32.dll", "RemoveDirectoryA"),
    ("kernel32.dll", "RemoveDirectoryW"),
    ("kernel32.dll", "ReplaceFileW"),
    ("kernel32.dll", "SetCurrentDirectoryA"),
    ("kernel32.dll", "SetCurrentDirectoryW"),
    ("kernel32.dll", "SetFileAttributesA"),
    ("kernel32.dll", "SetFileAttributesW"),
    ("msvcp140.dll", "?_Fiopen@std@@YAPEAU_iobuf@@PEBDHH@Z"),
    ("msvcp140.dll", "?_Fiopen@std@@YAPEAU_iobuf@@PEB_WHH@Z"),
    ("ucrtbase.dll", "_access"),
    ("ucrtbase.dll", "_close"),
    ("ucrtbase.dll", "_findclose"),
    ("ucrtbase.dll", "_mkdir"),
    ("ucrtbase.dll", "_open"),
    ("ucrtbase.dll", "_rmdir"),
    ("ucrtbase.dll", "_stat64"),
    ("ucrtbase.dll", "_stat64i32"),
    ("ucrtbase.dll", "_waccess"),
    ("ucrtbase.dll", "_wchmod"),
    ("ucrtbase.dll", "_wfindfirst64"),
    ("ucrtbase.dll", "_wfopen"),
    ("ucrtbase.dll", "_wfopen_s"),
    ("ucrtbase.dll", "_wfsopen"),
    ("ucrtbase.dll", "_wmkdir"),
    ("ucrtbase.dll", "_wopen"),
    ("ucrtbase.dll", "_wremove"),
    ("ucrtbase.dll", "_wrename"),
    ("ucrtbase.dll", "_wrmdir"),
    ("ucrtbase.dll", "_wstat64"),
    ("ucrtbase.dll", "_wstat64i32"),
    ("ucrtbase.dll", "_wutime64"),
    ("ucrtbase.dll", "fclose"),
    ("ucrtbase.dll", "fopen"),
    ("ucrtbase.dll", "fopen_s"),
    ("ucrtbase.dll", "fread"),
    ("ucrtbase.dll", "remove"),
    ("ucrtbase.dll", "rename"),
];

/// Mounted drives: upper-case letter and host root (UTF-16, no trailing
/// separator, no NUL).
static DRIVES: Mutex<Vec<(u16, Vec<u16>)>> = Mutex::new(Vec::new());

fn drives() -> MutexGuard<'static, Vec<(u16, Vec<u16>)>> {
    DRIVES.lock().unwrap_or_else(|e| e.into_inner())
}

/// Gives the host folder `root` a drive letter (the same one if it already
/// has one): the highest letter neither the host nor another mount uses.
pub fn mount(root: &Path) -> Option<char> {
    let wide = root_wide(root);
    let mut drives = drives();
    if let Some(&(letter, _)) = drives.iter().find(|(_, r)| *r == wide) {
        return char::from_u32(letter.into());
    }
    // SAFETY: no arguments.
    let host = unsafe { GetLogicalDrives() };
    let letter = (b'D'..=b'Z').rev().find(|&l| {
        host >> (l - b'A') & 1 == 0 && !drives.iter().any(|(m, _)| *m == u16::from(l))
    })?;
    tracing::info!(drive = %char::from(letter), root = %root.display(), "virtual drive");
    drives.push((letter.into(), wide));
    Some(letter.into())
}

/// Gives the host folder `root` a drive letter the console fixes (titles
/// open their package as `G:\..` and temporary files as `T:\..`). A host
/// drive of that letter is hidden from the title.
pub fn mount_at(letter: char, root: &Path) {
    let letter = letter.to_ascii_uppercase();
    let Some(index) = u32::from(letter)
        .checked_sub(u32::from('A'))
        .filter(|i| *i < 26)
    else {
        return;
    };
    // SAFETY: no arguments.
    let hides_host_drive = unsafe { GetLogicalDrives() } >> index & 1 != 0;
    tracing::info!(drive = %letter, root = %root.display(), hides_host_drive, "virtual drive");
    let mut drives = drives();
    drives.retain(|(l, _)| u32::from(*l) != u32::from(letter));
    drives.push((letter as u16, root_wide(root)));
}

/// `root` as UTF-16, absolute, without trailing separators (a drive root
/// keeps its own). An empty root is the current directory, as it is to a
/// path joined to it: it is the title's folder when the title is run by its
/// file name alone (`victory run GTA6.exe`).
fn root_wide(root: &Path) -> Vec<u16> {
    let root = if root.as_os_str().is_empty() {
        Path::new(".")
    } else {
        root
    };
    let root = std::path::absolute(root).unwrap_or_else(|_| root.to_owned());
    let mut wide: Vec<u16> = root.as_os_str().encode_wide().collect();
    while wide.len() > 3 && wide.last().is_some_and(|c| SEPARATORS.contains(c)) {
        wide.pop();
    }
    wide
}

/// The host path, NUL-terminated, for `path` (without its NUL) if it is on
/// a virtual drive.
pub(crate) fn translate(path: &[u16]) -> Option<Vec<u16>> {
    paths::translate(&drives(), path)
}

/// The title's path (no NUL) for the host path `host` (no NUL) if it lies
/// in the folder behind a virtual drive.
fn untranslate(host: &[u16]) -> Option<Vec<u16>> {
    paths::untranslate(&drives(), host)
}

/// Takes a drive letter back. Tests run side by side in one process and
/// the letters are few.
#[cfg(test)]
pub(crate) fn unmount(letter: char) {
    drives().retain(|(l, _)| u32::from(*l) != u32::from(letter));
}

/// A title's wide path: the host path when it is on a virtual drive, else
/// the title's own pointer.
struct Wide(*const u16, Option<Vec<u16>>);

impl Wide {
    fn ptr(&self) -> *const u16 {
        self.1.as_ref().map_or(self.0, |p| p.as_ptr())
    }
}

fn wide(path: *const u16) -> Wide {
    if path.is_null() {
        return Wide(path, None);
    }
    // SAFETY: the title passes a NUL-terminated path.
    let path_slice = unsafe {
        let len = (0..).take_while(|&i| *path.add(i) != 0).count();
        std::slice::from_raw_parts(path, len)
    };
    let host = translate(path_slice);
    trace(path_slice, host.as_deref());
    Wide(path, host)
}

/// Every title path at debug level, with the host path if rewritten.
fn trace(path: &[u16], host: Option<&[u16]>) {
    tracing::debug!(
        path = %String::from_utf16_lossy(path),
        host = host.map(|h| String::from_utf16_lossy(&h[..h.len() - 1])),
        "title path"
    );
}

/// A narrow path (ANSI code page, as the host decodes it) on a virtual
/// drive, as a wide host path.
fn narrow(path: *const c_char) -> Option<Vec<u16>> {
    if path.is_null() {
        return None;
    }
    // SAFETY: the title passes a NUL-terminated path.
    let bytes = unsafe { CStr::from_ptr(path) }.to_bytes();
    // The drive part is ASCII in every code page; decode only candidates.
    match bytes.strip_prefix(br"\\?\").unwrap_or(bytes) {
        [letter, b':', ..] if letter.is_ascii_alphabetic() => {}
        _ => {
            tracing::debug!(path = %String::from_utf8_lossy(bytes), "title path");
            return None;
        }
    }
    let wide = decode(bytes)?;
    let host = translate(&wide);
    trace(&wide, host.as_deref());
    host
}

/// A narrow string (ANSI code page) as the host decodes it, without NUL.
fn decode(bytes: &[u8]) -> Option<Vec<u16>> {
    if bytes.is_empty() {
        return Some(Vec::new());
    }
    let len = i32::try_from(bytes.len()).ok()?;
    // SAFETY: sizing call, then a call with room for `n` units.
    unsafe {
        let n = MultiByteToWideChar(CP_ACP, 0, bytes.as_ptr(), len, std::ptr::null_mut(), 0);
        let mut wide = vec![0u16; usize::try_from(n).ok().filter(|n| *n > 0)?];
        MultiByteToWideChar(CP_ACP, 0, bytes.as_ptr(), len, wide.as_mut_ptr(), n);
        Some(wide)
    }
}

/// A wide string in the ANSI code page, as the host's narrow functions
/// hand names out, without NUL.
fn encode(wide: &[u16]) -> Option<Vec<u8>> {
    if wide.is_empty() {
        return Some(Vec::new());
    }
    let len = i32::try_from(wide.len()).ok()?;
    let (null, none, unused) = (std::ptr::null_mut(), std::ptr::null(), std::ptr::null_mut());
    // SAFETY: sizing call, then a call with room for `n` bytes.
    unsafe {
        let n = WideCharToMultiByte(CP_ACP, 0, wide.as_ptr(), len, null, 0, none, unused);
        let mut bytes = vec![0u8; usize::try_from(n).ok().filter(|n| *n > 0)?];
        let out = bytes.as_mut_ptr();
        WideCharToMultiByte(CP_ACP, 0, wide.as_ptr(), len, out, n, none, unused);
        Some(bytes)
    }
}

/// A title's narrow path as the host would decode it, NUL-terminated and
/// not rewritten. None for a null pointer.
fn decoded(path: *const c_char) -> Option<Vec<u16>> {
    if path.is_null() {
        return None;
    }
    // SAFETY: the title passes a NUL-terminated path.
    let mut wide = decode(unsafe { CStr::from_ptr(path) }.to_bytes())?;
    wide.push(0);
    Some(wide)
}

/// The paths of a call that takes two narrow ones. None if neither is on a
/// virtual drive: the call goes to the host's narrow function as it came.
/// Otherwise both for the host's wide function: the one on a virtual drive
/// rewritten, the other as the host would decode it (null stays null).
fn narrow_pair(a: *const c_char, b: *const c_char) -> Option<[Option<Vec<u16>>; 2]> {
    let (host_a, host_b) = (narrow(a), narrow(b));
    if host_a.is_none() && host_b.is_none() {
        return None;
    }
    Some([host_a.or_else(|| decoded(a)), host_b.or_else(|| decoded(b))])
}

/// The pointer for a path of [`narrow_pair`].
fn ptr(path: &Option<Vec<u16>>) -> *const u16 {
    path.as_ref().map_or(std::ptr::null(), |p| p.as_ptr())
}

/// What a host query for a path answers, without NUL. `ask(buffer, units)`
/// is the query: it returns the length it wrote or, for a buffer too
/// small, the size it needs; 0 says it failed.
fn asked(mut ask: impl FnMut(*mut u16, u32) -> u32) -> Option<Vec<u16>> {
    // No path the host names is longer than 32767 units.
    const LONGEST: usize = 0x8000;
    let mut buffer = vec![0u16; 520];
    for _ in 0..3 {
        let n = ask(buffer.as_mut_ptr(), buffer.len() as u32) as usize;
        if n == 0 || n > LONGEST {
            return None;
        }
        if n < buffer.len() {
            buffer.truncate(n);
            return Some(buffer);
        }
        buffer = vec![0; n + 1];
    }
    None
}

/// Hands `text` (no NUL) to a title's buffer of `len` units the way the
/// host's path queries do, by their public references: the length written,
/// not counting the NUL; or, where it does not fit, the size needed with
/// the NUL, and nothing written.
fn answer<T: Copy + Default>(text: &[T], buffer: *mut T, len: u32) -> u32 {
    let Ok(needed) = u32::try_from(text.len() + 1) else {
        return 0;
    };
    if buffer.is_null() || len < needed {
        return needed;
    }
    // SAFETY: the title's buffer holds `len` units, which is enough.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), buffer, text.len());
        buffer.add(text.len()).write(T::default());
    }
    needed - 1
}

/// Where the last name of a full path starts; None if it ends in a
/// separator (a folder), as `GetFullPathName` reports its file part.
fn name_start(path: &[u16]) -> Option<usize> {
    let start = path.iter().rposition(|c| SEPARATORS.contains(c))? + 1;
    (start < path.len()).then_some(start)
}

/// A name the host found (up to its NUL) in the ANSI code page with its
/// NUL, into the field of the narrow record. False if it does not fit.
fn narrow_name(wide: &[u16], out: &mut [u8]) -> bool {
    let len = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    match encode(&wide[..len]) {
        Some(bytes) if bytes.len() < out.len() => {
            out[..bytes.len()].copy_from_slice(&bytes);
            out[bytes.len()] = 0;
            true
        }
        _ => false,
    }
}

const ERROR_PATH_NOT_FOUND: u32 = 3;
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_MORE_DATA: u32 = 234;
const ERROR_NO_UNICODE_TRANSLATION: u32 = 1113;
const INVALID_HANDLE: Handle = std::ptr::without_provenance_mut(usize::MAX);

/// What the host's wide search found, for a title that asked with a narrow
/// path on a virtual drive: the record it expects, with the names in the
/// ANSI code page. The search handle is the same for both forms, so the
/// title's `FindNextFileA` goes on from here. A name too long for the
/// narrow record ends the search with `ERROR_MORE_DATA`.
fn found_narrow(search: Handle, found: &WIN32_FIND_DATAW, data: *mut WIN32_FIND_DATAA) -> Handle {
    if search == INVALID_HANDLE {
        return search;
    }
    let mut narrow = WIN32_FIND_DATAA {
        dwFileAttributes: found.dwFileAttributes,
        ftCreationTime: found.ftCreationTime,
        ftLastAccessTime: found.ftLastAccessTime,
        ftLastWriteTime: found.ftLastWriteTime,
        nFileSizeHigh: found.nFileSizeHigh,
        nFileSizeLow: found.nFileSizeLow,
        dwReserved0: found.dwReserved0,
        dwReserved1: found.dwReserved1,
        cFileName: [0; 260],
        cAlternateFileName: [0; 14],
    };
    let error = if data.is_null() {
        ERROR_INVALID_PARAMETER
    } else if narrow_name(&found.cFileName, bytes_of(&mut narrow.cFileName))
        && narrow_name(
            &found.cAlternateFileName,
            bytes_of(&mut narrow.cAlternateFileName),
        )
    {
        // SAFETY: the title's record, checked for null.
        unsafe { data.write_unaligned(narrow) };
        return search;
    } else {
        ERROR_MORE_DATA
    };
    // SAFETY: the search opened by the caller; the thread's last error.
    unsafe {
        FindClose(search);
        SetLastError(error);
    }
    INVALID_HANDLE
}

/// A name field of the narrow record (`CHAR`s) as bytes.
fn bytes_of<const N: usize>(field: &mut [i8; N]) -> &mut [u8] {
    // SAFETY: `i8` and `u8` have the same size and alignment.
    unsafe { std::slice::from_raw_parts_mut(field.as_mut_ptr().cast(), N) }
}

/// An ASCII mode string (`"rb"`, `"w, ccs=UTF-8"`) for a wide CRT call.
fn widen(mode: *const c_char) -> Vec<u16> {
    if mode.is_null() {
        return vec![0];
    }
    // SAFETY: a NUL-terminated mode string.
    let bytes = unsafe { CStr::from_ptr(mode) }.to_bytes();
    bytes.iter().map(|&b| u16::from(b)).chain([0]).collect()
}

type Handle = *mut c_void;
type File = *mut c_void;

// kernel32 and its file API sets.

pub extern "system" fn CreateFileW(
    path: *const u16,
    access: u32,
    share: u32,
    security: *const c_void,
    disposition: u32,
    flags: u32,
    template: Handle,
) -> Handle {
    let p = wide(path);
    host!("kernel32.dll", "CreateFileW": extern "system" fn(*const u16, u32, u32, *const c_void, u32, u32, Handle) -> Handle)(
        p.ptr(),
        access,
        share,
        security,
        disposition,
        flags,
        template,
    )
}

pub extern "system" fn CreateFileA(
    path: *const c_char,
    access: u32,
    share: u32,
    security: *const c_void,
    disposition: u32,
    flags: u32,
    template: Handle,
) -> Handle {
    match narrow(path) {
        Some(p) => {
            host!("kernel32.dll", "CreateFileW": extern "system" fn(*const u16, u32, u32, *const c_void, u32, u32, Handle) -> Handle)(
                p.as_ptr(),
                access,
                share,
                security,
                disposition,
                flags,
                template,
            )
        }
        None => {
            host!("kernel32.dll", "CreateFileA": extern "system" fn(*const c_char, u32, u32, *const c_void, u32, u32, Handle) -> Handle)(
                path,
                access,
                share,
                security,
                disposition,
                flags,
                template,
            )
        }
    }
}

/// Used by Bink.
pub extern "system" fn CreateFile2(
    path: *const u16,
    access: u32,
    share: u32,
    disposition: u32,
    params: *const c_void,
) -> Handle {
    let p = wide(path);
    host!("kernel32.dll", "CreateFile2": extern "system" fn(*const u16, u32, u32, u32, *const c_void) -> Handle)(
        p.ptr(),
        access,
        share,
        disposition,
        params,
    )
}

pub extern "system" fn DeleteFileA(path: *const c_char) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("kernel32.dll", "DeleteFileW": extern "system" fn(*const u16) -> i32)(p.as_ptr())
        }
        None => {
            host!("kernel32.dll", "DeleteFileA": extern "system" fn(*const c_char) -> i32)(path)
        }
    }
}

pub extern "system" fn DeleteFileW(path: *const u16) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "DeleteFileW": extern "system" fn(*const u16) -> i32)(p.ptr())
}

pub extern "system" fn RemoveDirectoryW(path: *const u16) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "RemoveDirectoryW": extern "system" fn(*const u16) -> i32)(p.ptr())
}

pub extern "system" fn SetFileAttributesW(path: *const u16, attributes: u32) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "SetFileAttributesW": extern "system" fn(*const u16, u32) -> i32)(
        p.ptr(),
        attributes,
    )
}

pub extern "system" fn GetFileAttributesA(path: *const c_char) -> u32 {
    match narrow(path) {
        Some(p) => {
            host!("kernel32.dll", "GetFileAttributesW": extern "system" fn(*const u16) -> u32)(
                p.as_ptr(),
            )
        }
        None => {
            host!("kernel32.dll", "GetFileAttributesA": extern "system" fn(*const c_char) -> u32)(
                path,
            )
        }
    }
}

pub extern "system" fn CreateDirectoryW(path: *const u16, security: *const c_void) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "CreateDirectoryW": extern "system" fn(*const u16, *const c_void) -> i32)(
        p.ptr(),
        security,
    )
}

pub extern "system" fn GetFileAttributesExW(
    path: *const u16,
    level: i32,
    info: *mut c_void,
) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "GetFileAttributesExW": extern "system" fn(*const u16, i32, *mut c_void) -> i32)(
        p.ptr(),
        level,
        info,
    )
}

pub extern "system" fn GetFileAttributesW(path: *const u16) -> u32 {
    let p = wide(path);
    host!("kernel32.dll", "GetFileAttributesW": extern "system" fn(*const u16) -> u32)(p.ptr())
}

pub extern "system" fn GetDiskFreeSpaceExW(
    path: *const u16,
    available: *mut u64,
    total: *mut u64,
    free: *mut u64,
) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "GetDiskFreeSpaceExW": extern "system" fn(*const u16, *mut u64, *mut u64, *mut u64) -> i32)(
        p.ptr(),
        available,
        total,
        free,
    )
}

/// The path of a call that wants a volume's root: one on a virtual drive
/// becomes the root of the host volume its folder is on.
fn volume_root(root: *const u16) -> Wide {
    let mut p = wide(root);
    if let Some(host) = &mut p.1
        && let [letter, colon, sep, ..] = host[..]
        && colon == u16::from(b':')
        && SEPARATORS.contains(&sep)
    {
        *host = vec![letter, colon, sep, 0];
    }
    p
}

/// Wants a volume root: a virtual drive answers for the host volume its
/// folder is on.
pub extern "system" fn GetDiskFreeSpaceW(
    root: *const u16,
    sectors_per_cluster: *mut u32,
    bytes_per_sector: *mut u32,
    free_clusters: *mut u32,
    clusters: *mut u32,
) -> i32 {
    let p = volume_root(root);
    host!("kernel32.dll", "GetDiskFreeSpaceW": extern "system" fn(*const u16, *mut u32, *mut u32, *mut u32, *mut u32) -> i32)(
        p.ptr(),
        sectors_per_cluster,
        bytes_per_sector,
        free_clusters,
        clusters,
    )
}

pub extern "system" fn FindFirstFileW(path: *const u16, data: *mut c_void) -> Handle {
    let p = wide(path);
    host!("kernel32.dll", "FindFirstFileW": extern "system" fn(*const u16, *mut c_void) -> Handle)(
        p.ptr(),
        data,
    )
}

pub extern "system" fn FindFirstFileExW(
    path: *const u16,
    level: i32,
    data: *mut c_void,
    search: i32,
    filter: *mut c_void,
    flags: u32,
) -> Handle {
    let p = wide(path);
    host!("kernel32.dll", "FindFirstFileExW": extern "system" fn(*const u16, i32, *mut c_void, i32, *mut c_void, u32) -> Handle)(
        p.ptr(),
        level,
        data,
        search,
        filter,
        flags,
    )
}

pub extern "system" fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32 {
    let (from, to) = (wide(from), wide(to));
    host!("kernel32.dll", "MoveFileExW": extern "system" fn(*const u16, *const u16, u32) -> i32)(
        from.ptr(),
        to.ptr(),
        flags,
    )
}

pub extern "system" fn SetCurrentDirectoryA(path: *const c_char) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("kernel32.dll", "SetCurrentDirectoryW": extern "system" fn(*const u16) -> i32)(
                p.as_ptr(),
            )
        }
        None => {
            host!("kernel32.dll", "SetCurrentDirectoryA": extern "system" fn(*const c_char) -> i32)(
                path,
            )
        }
    }
}

pub extern "system" fn CreateDirectoryA(path: *const c_char, security: *const c_void) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("kernel32.dll", "CreateDirectoryW": extern "system" fn(*const u16, *const c_void) -> i32)(
                p.as_ptr(),
                security,
            )
        }
        None => {
            host!("kernel32.dll", "CreateDirectoryA": extern "system" fn(*const c_char, *const c_void) -> i32)(
                path, security,
            )
        }
    }
}

pub extern "system" fn RemoveDirectoryA(path: *const c_char) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("kernel32.dll", "RemoveDirectoryW": extern "system" fn(*const u16) -> i32)(
                p.as_ptr(),
            )
        }
        None => {
            host!("kernel32.dll", "RemoveDirectoryA": extern "system" fn(*const c_char) -> i32)(
                path,
            )
        }
    }
}

pub extern "system" fn SetFileAttributesA(path: *const c_char, attributes: u32) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("kernel32.dll", "SetFileAttributesW": extern "system" fn(*const u16, u32) -> i32)(
                p.as_ptr(),
                attributes,
            )
        }
        None => {
            host!("kernel32.dll", "SetFileAttributesA": extern "system" fn(*const c_char, u32) -> i32)(
                path, attributes,
            )
        }
    }
}

/// The record has no names in it: the wide twin fills the same one.
pub extern "system" fn GetFileAttributesExA(
    path: *const c_char,
    level: i32,
    info: *mut c_void,
) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("kernel32.dll", "GetFileAttributesExW": extern "system" fn(*const u16, i32, *mut c_void) -> i32)(
                p.as_ptr(),
                level,
                info,
            )
        }
        None => {
            host!("kernel32.dll", "GetFileAttributesExA": extern "system" fn(*const c_char, i32, *mut c_void) -> i32)(
                path, level, info,
            )
        }
    }
}

pub extern "system" fn FindFirstFileA(path: *const c_char, data: *mut WIN32_FIND_DATAA) -> Handle {
    match narrow(path) {
        Some(p) => {
            let mut found = WIN32_FIND_DATAW::default();
            let search = host!("kernel32.dll", "FindFirstFileW": extern "system" fn(*const u16, *mut WIN32_FIND_DATAW) -> Handle)(
                p.as_ptr(),
                &mut found,
            );
            found_narrow(search, &found, data)
        }
        None => {
            host!("kernel32.dll", "FindFirstFileA": extern "system" fn(*const c_char, *mut WIN32_FIND_DATAA) -> Handle)(
                path, data,
            )
        }
    }
}

/// Both levels of information fill the same record (the basic one leaves
/// the short name out).
pub extern "system" fn FindFirstFileExA(
    path: *const c_char,
    level: i32,
    data: *mut WIN32_FIND_DATAA,
    search: i32,
    filter: *mut c_void,
    flags: u32,
) -> Handle {
    match narrow(path) {
        Some(p) => {
            let mut found = WIN32_FIND_DATAW::default();
            let handle = host!("kernel32.dll", "FindFirstFileExW": extern "system" fn(*const u16, i32, *mut WIN32_FIND_DATAW, i32, *mut c_void, u32) -> Handle)(
                p.as_ptr(),
                level,
                &mut found,
                search,
                filter,
                flags,
            );
            found_narrow(handle, &found, data)
        }
        None => {
            host!("kernel32.dll", "FindFirstFileExA": extern "system" fn(*const c_char, i32, *mut WIN32_FIND_DATAA, i32, *mut c_void, u32) -> Handle)(
                path, level, data, search, filter, flags,
            )
        }
    }
}

pub extern "system" fn MoveFileW(from: *const u16, to: *const u16) -> i32 {
    let (from, to) = (wide(from), wide(to));
    host!("kernel32.dll", "MoveFileW": extern "system" fn(*const u16, *const u16) -> i32)(
        from.ptr(),
        to.ptr(),
    )
}

/// `to` may be null (a delete at the next start, which the host decides on).
pub extern "system" fn MoveFileExA(from: *const c_char, to: *const c_char, flags: u32) -> i32 {
    match narrow_pair(from, to) {
        Some([a, b]) => {
            host!("kernel32.dll", "MoveFileExW": extern "system" fn(*const u16, *const u16, u32) -> i32)(
                ptr(&a),
                ptr(&b),
                flags,
            )
        }
        None => {
            host!("kernel32.dll", "MoveFileExA": extern "system" fn(*const c_char, *const c_char, u32) -> i32)(
                from, to, flags,
            )
        }
    }
}

pub extern "system" fn ReplaceFileW(
    replaced: *const u16,
    replacement: *const u16,
    backup: *const u16,
    flags: u32,
    exclude: *mut c_void,
    reserved: *mut c_void,
) -> i32 {
    let (replaced, replacement, backup) = (wide(replaced), wide(replacement), wide(backup));
    host!("kernel32.dll", "ReplaceFileW": extern "system" fn(*const u16, *const u16, *const u16, u32, *mut c_void, *mut c_void) -> i32)(
        replaced.ptr(),
        replacement.ptr(),
        backup.ptr(),
        flags,
        exclude,
        reserved,
    )
}

pub extern "system" fn CopyFileW(from: *const u16, to: *const u16, fail_if_exists: i32) -> i32 {
    let (from, to) = (wide(from), wide(to));
    host!("kernel32.dll", "CopyFileW": extern "system" fn(*const u16, *const u16, i32) -> i32)(
        from.ptr(),
        to.ptr(),
        fail_if_exists,
    )
}

/// Returns an `HRESULT`.
pub extern "system" fn CopyFile2(from: *const u16, to: *const u16, params: *const c_void) -> i32 {
    let (from, to) = (wide(from), wide(to));
    host!("kernel32.dll", "CopyFile2": extern "system" fn(*const u16, *const u16, *const c_void) -> i32)(
        from.ptr(),
        to.ptr(),
        params,
    )
}

pub extern "system" fn CreateHardLinkW(
    link: *const u16,
    existing: *const u16,
    security: *const c_void,
) -> i32 {
    let (link, existing) = (wide(link), wide(existing));
    host!("kernel32.dll", "CreateHardLinkW": extern "system" fn(*const u16, *const u16, *const c_void) -> i32)(
        link.ptr(),
        existing.ptr(),
        security,
    )
}

pub extern "system" fn SetCurrentDirectoryW(path: *const u16) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "SetCurrentDirectoryW": extern "system" fn(*const u16) -> i32)(p.ptr())
}

/// As [`GetDiskFreeSpaceW`]: a virtual drive answers for the host volume
/// its folder is on (its name, serial number and file system), since that
/// is where the title's files are.
pub extern "system" fn GetVolumeInformationW(
    root: *const u16,
    name: *mut u16,
    name_len: u32,
    serial: *mut u32,
    longest_name: *mut u32,
    flags: *mut u32,
    file_system: *mut u16,
    file_system_len: u32,
) -> i32 {
    let p = volume_root(root);
    host!("kernel32.dll", "GetVolumeInformationW": extern "system" fn(*const u16, *mut u16, u32, *mut u32, *mut u32, *mut u32, *mut u16, u32) -> i32)(
        p.ptr(),
        name,
        name_len,
        serial,
        longest_name,
        flags,
        file_system,
        file_system_len,
    )
}

// Paths the host hands back. One inside a mounted folder is given to the
// title as the path on its drive; any other as the host gave it, by the
// host's own call with the title's arguments.

pub extern "system" fn GetCurrentDirectoryW(len: u32, buffer: *mut u16) -> u32 {
    let get =
        host!("kernel32.dll", "GetCurrentDirectoryW": extern "system" fn(u32, *mut u16) -> u32);
    match asked(|b, n| get(n, b)).and_then(|path| untranslate(&path)) {
        Some(path) => answer(&path, buffer, len),
        None => get(len, buffer),
    }
}

pub extern "system" fn GetCurrentDirectoryA(len: u32, buffer: *mut u8) -> u32 {
    let wide =
        host!("kernel32.dll", "GetCurrentDirectoryW": extern "system" fn(u32, *mut u16) -> u32);
    match asked(|b, n| wide(n, b)).and_then(|path| untranslate(&path)) {
        Some(path) => match encode(&path) {
            Some(path) => answer(&path, buffer, len),
            None => fail(ERROR_NO_UNICODE_TRANSLATION),
        },
        None => {
            host!("kernel32.dll", "GetCurrentDirectoryA": extern "system" fn(u32, *mut u8) -> u32)(
                len, buffer,
            )
        }
    }
}

/// Fails a call that returns 0 for failure, with `error` for the thread.
fn fail(error: u32) -> u32 {
    // SAFETY: the thread's last error.
    unsafe { SetLastError(error) };
    0
}

/// The host works on the text alone, so a path on a virtual drive comes
/// back on that drive without our doing. A relative path is completed with
/// the current directory, which may be a mounted folder.
pub extern "system" fn GetFullPathNameW(
    path: *const u16,
    len: u32,
    buffer: *mut u16,
    file_part: *mut *mut u16,
) -> u32 {
    let get = host!("kernel32.dll", "GetFullPathNameW": extern "system" fn(*const u16, u32, *mut u16, *mut *mut u16) -> u32);
    let none = std::ptr::null_mut();
    let full = if path.is_null() {
        None
    } else {
        asked(|b, n| get(path, n, b, none)).and_then(|full| untranslate(&full))
    };
    let Some(full) = full else {
        return get(path, len, buffer, file_part);
    };
    let written = answer(&full, buffer, len);
    if written as usize == full.len() && !file_part.is_null() {
        // SAFETY: an offset inside the path just written; the title's
        // place for the pointer, checked for null.
        unsafe {
            file_part
                .write(name_start(&full).map_or(std::ptr::null_mut(), |start| buffer.add(start)))
        };
    }
    written
}

pub extern "system" fn GetFullPathNameA(
    path: *const c_char,
    len: u32,
    buffer: *mut u8,
    file_part: *mut *mut u8,
) -> u32 {
    let none = std::ptr::null_mut();
    let full = decoded(path).and_then(|path| {
        let get = host!("kernel32.dll", "GetFullPathNameW": extern "system" fn(*const u16, u32, *mut u16, *mut *mut u16) -> u32);
        asked(|b, n| get(path.as_ptr(), n, b, none)).and_then(|full| untranslate(&full))
    });
    let Some(full) = full else {
        return host!("kernel32.dll", "GetFullPathNameA": extern "system" fn(*const c_char, u32, *mut u8, *mut *mut u8) -> u32)(
            path, len, buffer, file_part,
        );
    };
    // The file part is found in the wide text: in a narrow one, the second
    // byte of a character can look like a separator.
    let start = name_start(&full);
    let (Some(text), Some(start)) = (
        encode(&full),
        start.map_or(Some(None), |start| {
            encode(&full[..start]).map(|head| Some(head.len()))
        }),
    ) else {
        return fail(ERROR_NO_UNICODE_TRANSLATION);
    };
    let written = answer(&text, buffer, len);
    if written as usize == text.len() && !file_part.is_null() {
        // SAFETY: as in the wide twin.
        unsafe { file_part.write(start.map_or(std::ptr::null_mut(), |start| buffer.add(start))) };
    }
    written
}

/// The host's temporary folder, which the console does not have: named by
/// its drive where it lies in a mounted folder, else as the host names it.
pub extern "system" fn GetTempPathW(len: u32, buffer: *mut u16) -> u32 {
    let get = host!("kernel32.dll", "GetTempPathW": extern "system" fn(u32, *mut u16) -> u32);
    match asked(|b, n| get(n, b)).and_then(|path| untranslate(&path)) {
        Some(path) => answer(&path, buffer, len),
        None => get(len, buffer),
    }
}

const FILE_NAME_OPENED: u32 = 8;
const VOLUME_NAME_DOS: u32 = 0;
const VOLUME_NAME_GUID: u32 = 1;
const VOLUME_NAME_NT: u32 = 2;
const VOLUME_NAME_NONE: u32 = 4;

type FinalPath = extern "system" fn(Handle, *mut u16, u32, u32) -> u32;

/// The mounted drives by the names the host gives their folders as open
/// files (links and short names resolved, the `\\?\` form), then by the
/// names they were mounted with.
fn final_roots(get: FinalPath, opened: u32) -> Vec<(u16, Vec<u16>)> {
    const SHARE_ALL: u32 = 7;
    const OPEN_EXISTING: u32 = 3;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let open = host!("kernel32.dll", "CreateFileW": extern "system" fn(*const u16, u32, u32, *const c_void, u32, u32, Handle) -> Handle);
    let close = host!("kernel32.dll", "CloseHandle": extern "system" fn(Handle) -> i32);
    let mounted = drives().clone();
    let mut roots = Vec::new();
    for (letter, root) in &mounted {
        let name: Vec<u16> = root.iter().copied().chain([0]).collect();
        let (none, null) = (std::ptr::null(), std::ptr::null_mut());
        let flags = FILE_FLAG_BACKUP_SEMANTICS;
        let folder = open(
            name.as_ptr(),
            0,
            SHARE_ALL,
            none,
            OPEN_EXISTING,
            flags,
            null,
        );
        if folder != INVALID_HANDLE {
            roots.extend(asked(|b, n| get(folder, b, n, opened)).map(|path| (*letter, path)));
            close(folder);
        }
    }
    roots.extend(mounted);
    roots
}

/// The path of an open file. For a file in a mounted folder: with the
/// drive letter (the default) it is the path on the virtual drive, in the
/// `\\?\` form this function answers in; with no volume name, the path below
/// the drive's root. A virtual drive has no volume GUID path and no NT
/// device, so those two fail with `ERROR_PATH_NOT_FOUND`, the documented
/// error for a volume that has no name of the kind asked for.
/// <https://learn.microsoft.com/windows/win32/api/fileapi/nf-fileapi-getfinalpathnamebyhandlew>
pub extern "system" fn GetFinalPathNameByHandleW(
    file: Handle,
    buffer: *mut u16,
    len: u32,
    flags: u32,
) -> u32 {
    let get: FinalPath = host!("kernel32.dll", "GetFinalPathNameByHandleW": extern "system" fn(Handle, *mut u16, u32, u32) -> u32);
    const VOLUME_NAMES: u32 = VOLUME_NAME_GUID | VOLUME_NAME_NT | VOLUME_NAME_NONE;
    let opened = flags & FILE_NAME_OPENED;
    let volume = flags & VOLUME_NAMES;
    let known = flags & !(VOLUME_NAMES | FILE_NAME_OPENED) == 0
        && matches!(
            volume,
            VOLUME_NAME_DOS | VOLUME_NAME_GUID | VOLUME_NAME_NT | VOLUME_NAME_NONE
        );
    let path = if known {
        asked(|b, n| get(file, b, n, opened))
            .and_then(|path| paths::untranslate(&final_roots(get, opened), &path))
    } else {
        None
    };
    let Some(path) = path else {
        return get(file, buffer, len, flags);
    };
    match volume {
        VOLUME_NAME_DOS => {
            let prefix: Vec<u16> = r"\\?\".encode_utf16().collect();
            answer(&[prefix, path].concat(), buffer, len)
        }
        // `X:\tail` without `X:`.
        VOLUME_NAME_NONE => answer(&path[2..], buffer, len),
        _ => fail(ERROR_PATH_NOT_FOUND),
    }
}

// The C runtime (ucrtbase, which the title's CRT imports bind to).

pub extern "C" fn _wfopen(path: *const u16, mode: *const u16) -> File {
    let p = wide(path);
    host!("ucrtbase.dll", "_wfopen": extern "C" fn(*const u16, *const u16) -> File)(p.ptr(), mode)
}

pub extern "C" fn _wfopen_s(out: *mut File, path: *const u16, mode: *const u16) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wfopen_s": extern "C" fn(*mut File, *const u16, *const u16) -> i32)(
        out,
        p.ptr(),
        mode,
    )
}

pub extern "C" fn _wfsopen(path: *const u16, mode: *const u16, share: i32) -> File {
    let p = wide(path);
    host!("ucrtbase.dll", "_wfsopen": extern "C" fn(*const u16, *const u16, i32) -> File)(
        p.ptr(),
        mode,
        share,
    )
}

// Kernel reads into title memory. The GPU side leaves blocks of title GPU
// buffers read-only to see the title's next write, and the kernel's writes
// fail there instead of faulting into its handler; Grand Theft Auto VI's
// synchronous reads (title+0x313fc0) read straight into 4 KiB-aligned
// destinations and retry a failed `ReadFile` forever. So the destination
// is opened first.

pub extern "system" fn ReadFile(
    file: *mut c_void,
    buffer: *mut u8,
    len: u32,
    read: *mut u32,
    overlapped: *mut c_void,
) -> i32 {
    let _writing = victory_gpu::kernel_will_write(buffer as usize, len as usize);
    host!("kernel32.dll", "ReadFile": extern "system" fn(*mut c_void, *mut u8, u32, *mut u32, *mut c_void) -> i32)(
        file, buffer, len, read, overlapped,
    )
}

/// The host CRT reads large requests straight into the caller's buffer.
pub extern "C" fn fread(buffer: *mut u8, size: usize, count: usize, stream: File) -> usize {
    let _writing = victory_gpu::kernel_will_write(buffer as usize, size.saturating_mul(count));
    host!("ucrtbase.dll", "fread": extern "C" fn(*mut u8, usize, usize, File) -> usize)(
        buffer, size, count, stream,
    )
}

pub extern "C" fn fopen(path: *const c_char, mode: *const c_char) -> File {
    match narrow(path) {
        Some(p) => host!("ucrtbase.dll", "_wfopen": extern "C" fn(*const u16, *const u16) -> File)(
            p.as_ptr(),
            widen(mode).as_ptr(),
        ),
        None => {
            host!("ucrtbase.dll", "fopen": extern "C" fn(*const c_char, *const c_char) -> File)(
                path, mode,
            )
        }
    }
}

pub extern "C" fn fopen_s(out: *mut File, path: *const c_char, mode: *const c_char) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("ucrtbase.dll", "_wfopen_s": extern "C" fn(*mut File, *const u16, *const u16) -> i32)(
                out,
                p.as_ptr(),
                widen(mode).as_ptr(),
            )
        }
        None => {
            host!("ucrtbase.dll", "fopen_s": extern "C" fn(*mut File, *const c_char, *const c_char) -> i32)(
                out, path, mode,
            )
        }
    }
}

/// `_open(path, flags[, mode])` is variadic; on x64 the optional integer
/// travels in the same register either way, so it is passed on as is.
pub extern "C" fn _open(path: *const c_char, flags: i32, mode: i32) -> i32 {
    match narrow(path) {
        Some(p) => host!("ucrtbase.dll", "_wopen": extern "C" fn(*const u16, i32, i32) -> i32)(
            p.as_ptr(),
            flags,
            mode,
        ),
        None => host!("ucrtbase.dll", "_open": extern "C" fn(*const c_char, i32, i32) -> i32)(
            path, flags, mode,
        ),
    }
}

/// See [`_open`]; used by libHttpClient.
pub extern "C" fn _wopen(path: *const u16, flags: i32, mode: i32) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wopen": extern "C" fn(*const u16, i32, i32) -> i32)(
        p.ptr(),
        flags,
        mode,
    )
}

pub extern "C" fn _wstat64(path: *const u16, stat: *mut c_void) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wstat64": extern "C" fn(*const u16, *mut c_void) -> i32)(p.ptr(), stat)
}

pub extern "C" fn _stat64i32(path: *const c_char, stat: *mut c_void) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("ucrtbase.dll", "_wstat64i32": extern "C" fn(*const u16, *mut c_void) -> i32)(
                p.as_ptr(),
                stat,
            )
        }
        None => {
            host!("ucrtbase.dll", "_stat64i32": extern "C" fn(*const c_char, *mut c_void) -> i32)(
                path, stat,
            )
        }
    }
}

pub extern "C" fn remove(path: *const c_char) -> i32 {
    match narrow(path) {
        Some(p) => host!("ucrtbase.dll", "_wremove": extern "C" fn(*const u16) -> i32)(p.as_ptr()),
        None => host!("ucrtbase.dll", "remove": extern "C" fn(*const c_char) -> i32)(path),
    }
}

pub extern "C" fn _wchmod(path: *const u16, mode: i32) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wchmod": extern "C" fn(*const u16, i32) -> i32)(p.ptr(), mode)
}

pub extern "C" fn _wutime64(path: *const u16, times: *mut c_void) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wutime64": extern "C" fn(*const u16, *mut c_void) -> i32)(
        p.ptr(),
        times,
    )
}

pub extern "C" fn _wrename(from: *const u16, to: *const u16) -> i32 {
    let (from, to) = (wide(from), wide(to));
    host!("ucrtbase.dll", "_wrename": extern "C" fn(*const u16, *const u16) -> i32)(
        from.ptr(),
        to.ptr(),
    )
}

pub extern "C" fn rename(from: *const c_char, to: *const c_char) -> i32 {
    match narrow_pair(from, to) {
        Some([a, b]) => {
            host!("ucrtbase.dll", "_wrename": extern "C" fn(*const u16, *const u16) -> i32)(
                ptr(&a),
                ptr(&b),
            )
        }
        None => {
            host!("ucrtbase.dll", "rename": extern "C" fn(*const c_char, *const c_char) -> i32)(
                from, to,
            )
        }
    }
}

pub extern "C" fn _wremove(path: *const u16) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wremove": extern "C" fn(*const u16) -> i32)(p.ptr())
}

pub extern "C" fn _wmkdir(path: *const u16) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wmkdir": extern "C" fn(*const u16) -> i32)(p.ptr())
}

pub extern "C" fn _mkdir(path: *const c_char) -> i32 {
    match narrow(path) {
        Some(p) => host!("ucrtbase.dll", "_wmkdir": extern "C" fn(*const u16) -> i32)(p.as_ptr()),
        None => host!("ucrtbase.dll", "_mkdir": extern "C" fn(*const c_char) -> i32)(path),
    }
}

pub extern "C" fn _wrmdir(path: *const u16) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wrmdir": extern "C" fn(*const u16) -> i32)(p.ptr())
}

pub extern "C" fn _rmdir(path: *const c_char) -> i32 {
    match narrow(path) {
        Some(p) => host!("ucrtbase.dll", "_wrmdir": extern "C" fn(*const u16) -> i32)(p.as_ptr()),
        None => host!("ucrtbase.dll", "_rmdir": extern "C" fn(*const c_char) -> i32)(path),
    }
}

pub extern "C" fn _waccess(path: *const u16, mode: i32) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_waccess": extern "C" fn(*const u16, i32) -> i32)(p.ptr(), mode)
}

pub extern "C" fn _access(path: *const c_char, mode: i32) -> i32 {
    match narrow(path) {
        Some(p) => host!("ucrtbase.dll", "_waccess": extern "C" fn(*const u16, i32) -> i32)(
            p.as_ptr(),
            mode,
        ),
        None => {
            host!("ucrtbase.dll", "_access": extern "C" fn(*const c_char, i32) -> i32)(path, mode)
        }
    }
}

/// The search handle is the host's; `_wfindnext64` and `_findclose` take it
/// as it is.
pub extern "C" fn _wfindfirst64(spec: *const u16, data: *mut c_void) -> isize {
    let p = wide(spec);
    host!("ucrtbase.dll", "_wfindfirst64": extern "C" fn(*const u16, *mut c_void) -> isize)(
        p.ptr(),
        data,
    )
}

/// `struct _stat64` is the same for both forms of the path.
pub extern "C" fn _stat64(path: *const c_char, stat: *mut c_void) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("ucrtbase.dll", "_wstat64": extern "C" fn(*const u16, *mut c_void) -> i32)(
                p.as_ptr(),
                stat,
            )
        }
        None => host!("ucrtbase.dll", "_stat64": extern "C" fn(*const c_char, *mut c_void) -> i32)(
            path, stat,
        ),
    }
}

pub extern "C" fn _wstat64i32(path: *const u16, stat: *mut c_void) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wstat64i32": extern "C" fn(*const u16, *mut c_void) -> i32)(
        p.ptr(),
        stat,
    )
}

// The C++ library: `std::basic_filebuf::open` goes through `_Fiopen`.

const FIOPEN: &str = "?_Fiopen@std@@YAPEAU_iobuf@@PEBDHH@Z";
const FIOPEN_WIDE: &str = "?_Fiopen@std@@YAPEAU_iobuf@@PEB_WHH@Z";

/// `_Fiopen(const wchar_t* path, ios_base::openmode, int protection)`.
extern "C" fn fiopen_wide(path: *const u16, mode: i32, protection: i32) -> File {
    let p = wide(path);
    host!("msvcp140.dll", "?_Fiopen@std@@YAPEAU_iobuf@@PEB_WHH@Z": extern "C" fn(*const u16, i32, i32) -> File)(
        p.ptr(),
        mode,
        protection,
    )
}

/// `_Fiopen(const char* path, ios_base::openmode, int protection)`.
extern "C" fn fiopen(path: *const c_char, mode: i32, protection: i32) -> File {
    match narrow(path) {
        Some(p) => {
            host!("msvcp140.dll", "?_Fiopen@std@@YAPEAU_iobuf@@PEB_WHH@Z": extern "C" fn(*const u16, i32, i32) -> File)(
                p.as_ptr(),
                mode,
                protection,
            )
        }
        None => {
            host!("msvcp140.dll", "?_Fiopen@std@@YAPEAU_iobuf@@PEBDHH@Z": extern "C" fn(*const c_char, i32, i32) -> File)(
                path, mode, protection,
            )
        }
    }
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "files",
        dlls: &[
            "kernel32.dll",
            "kernelbase.dll",
            "api-ms-win-core-file-*",
            // CopyFileW, MoveFileW and MoveFileExA, by their reference pages.
            "api-ms-win-core-kernel32-legacy-*",
            "api-ms-win-core-processenvironment-*",
            "ucrtbase.dll",
            "api-ms-win-crt-stdio-*",
            "api-ms-win-crt-filesystem-*",
            "api-ms-win-crt-time-*",
            "msvcp140.dll",
        ],
        exports: vec![
            hle_export!(ReadFile),
            hle_export!(fread),
            hle_export!(CreateFileW),
            hle_export!(CreateFileA),
            hle_export!(CreateFile2),
            hle_export!(DeleteFileA),
            hle_export!(DeleteFileW),
            hle_export!(RemoveDirectoryW),
            hle_export!(SetFileAttributesW),
            hle_export!(GetFileAttributesA),
            hle_export!(CreateDirectoryW),
            hle_export!(GetFileAttributesW),
            hle_export!(GetFileAttributesExW),
            hle_export!(GetDiskFreeSpaceExW),
            hle_export!(GetDiskFreeSpaceW),
            hle_export!(FindFirstFileW),
            hle_export!(FindFirstFileExW),
            hle_export!(MoveFileExW),
            hle_export!(SetCurrentDirectoryA),
            hle_export!(CreateDirectoryA),
            hle_export!(RemoveDirectoryA),
            hle_export!(SetFileAttributesA),
            hle_export!(GetFileAttributesExA),
            hle_export!(FindFirstFileA),
            hle_export!(FindFirstFileExA),
            hle_export!(MoveFileW),
            hle_export!(MoveFileExA),
            hle_export!(ReplaceFileW),
            hle_export!(CopyFileW),
            hle_export!(CopyFile2),
            hle_export!(CreateHardLinkW),
            hle_export!(SetCurrentDirectoryW),
            hle_export!(GetVolumeInformationW),
            hle_export!(GetCurrentDirectoryW),
            hle_export!(GetCurrentDirectoryA),
            hle_export!(GetFullPathNameW),
            hle_export!(GetFullPathNameA),
            hle_export!(GetTempPathW),
            hle_export!(GetFinalPathNameByHandleW),
            hle_export!(_wfopen),
            hle_export!(_wfopen_s),
            hle_export!(_wfsopen),
            hle_export!(fopen),
            hle_export!(fopen_s),
            hle_export!(_open),
            hle_export!(_wopen),
            hle_export!(_wstat64),
            hle_export!(_stat64i32),
            hle_export!(remove),
            hle_export!(_wchmod),
            hle_export!(_wutime64),
            hle_export!(_wrename),
            hle_export!(rename),
            hle_export!(_wremove),
            hle_export!(_wmkdir),
            hle_export!(_mkdir),
            hle_export!(_wrmdir),
            hle_export!(_rmdir),
            hle_export!(_waccess),
            hle_export!(_access),
            hle_export!(_wfindfirst64),
            hle_export!(_stat64),
            hle_export!(_wstat64i32),
            HleExport {
                name: FIOPEN,
                address: fiopen as *const () as usize,
            },
            HleExport {
                name: FIOPEN_WIDE,
                address: fiopen_wide as *const () as usize,
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::paths::verbatim;
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    #[test]
    fn kernel_reads_reach_the_buffer() {
        use std::os::windows::io::AsRawHandle;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        std::fs::write(&path, b"0123456789").unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let (mut out, mut n) = ([0u8; 10], 0);
        let handle = file.as_raw_handle().cast();
        assert_ne!(
            ReadFile(handle, out.as_mut_ptr(), 10, &mut n, std::ptr::null_mut()),
            0
        );
        assert_eq!((&out, n), (b"0123456789", 10));
    }

    fn text(v: &[u16]) -> String {
        String::from_utf16_lossy(&v[..v.len() - 1])
    }

    #[test]
    fn rewrites_only_paths_on_mounted_drives() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_string_lossy().into_owned();
        let letter = mount(dir.path()).unwrap();
        assert_eq!(
            mount(dir.path()),
            Some(letter),
            "mounting twice reuses the letter"
        );
        let tr = |s: String| translate(&w(&s)[..s.encode_utf16().count()]).map(|v| text(&v));
        assert_eq!(
            tr(format!("{letter}:/a/b.txt")),
            Some(format!("{root}/a/b.txt"))
        );
        let lower = letter.to_ascii_lowercase();
        assert_eq!(tr(format!("{lower}:\\a")), Some(format!("{root}\\a")));
        assert_eq!(tr(format!("{letter}:")), Some(format!("{root}\\")));
        assert_eq!(
            tr(format!(r"\\?\{letter}:/a/b")),
            Some(format!(r"\\?\{root}\a\b"))
        );
        assert_eq!(tr(r"C:\Windows".into()), None);
        assert_eq!(tr("relative/path".into()), None);
        assert_eq!(tr(String::new()), None);

        // Fixed letters, as the console's package drive; a later mount of
        // the same letter replaces the earlier one.
        let package = tempfile::tempdir().unwrap();
        mount_at('g', dir.path());
        mount_at('G', package.path());
        assert_eq!(
            tr(r"G:\appdata0.rpf".into()),
            Some(format!(r"{}\appdata0.rpf", package.path().display()))
        );
        assert_ne!(mount(dir.path()), Some('G'), "free letters skip fixed ones");

        // Long ones take the `\\?\` form, cleaned up as the host would.
        let long = "d".repeat(250);
        assert_eq!(
            tr(format!(r"G:/a/./b/../{long}//f")),
            Some(format!(r"\\?\{}\a\{long}\f", package.path().display()))
        );
        assert_eq!(
            verbatim(&w(r"\\server\share\")[..15], &w("/x/../../y")[..10]),
            w(r"\\?\UNC\server\share\y")[..22].to_vec()
        );
    }

    /// A title run by its file name alone has an empty folder: the drive
    /// over it is the current directory, not the current drive's root.
    #[test]
    fn an_empty_root_is_the_current_directory() {
        let letter = mount(Path::new("")).unwrap();
        let path = format!(r"{letter}:\a.rpf");
        let host = translate(&w(&path)[..path.len()]).map(|v| text(&v));
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(host, Some(cwd.join("a.rpf").display().to_string()));
    }

    #[test]
    fn dot_dot_stops_at_the_drive_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_string_lossy().into_owned();
        let letter = mount(dir.path()).unwrap();
        let tr = |s: String| translate(&w(&s)[..s.encode_utf16().count()]).map(|v| text(&v));
        let inside = Some(format!(r"\\?\{root}\Windows\x"));
        assert_eq!(tr(format!(r"{letter}:\..\..\Windows\x")), inside);
        assert_eq!(tr(format!(r"{letter}:/a/../../Windows/x")), inside);
        assert_eq!(tr(format!(r"{letter}:..\Windows\x")), inside);
        // Within the folder, `..` is still resolved.
        assert_eq!(
            tr(format!(r"{letter}:\a\..\b")),
            Some(format!(r"\\?\{root}\b"))
        );
    }

    /// The table lists what this file binds, and the host has all of it.
    #[test]
    fn every_host_import_exists_or_has_a_fallback() {
        crate::host::check_bindings(include_str!("files.rs"), HOST_IMPORTS, &[]);
    }

    #[test]
    fn long_title_paths_work_on_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let letter = mount(dir.path()).unwrap();
        let deep = format!("{letter}:/{}/{}", "a".repeat(200), "b".repeat(100));
        let null = std::ptr::null_mut();
        let first = w(&deep[..203]);
        assert_ne!(CreateDirectoryW(first.as_ptr(), null), 0);
        assert_ne!(CreateDirectoryW(w(&deep).as_ptr(), null), 0);
        let file = format!("{deep}/../{}/f.txt", "b".repeat(100));
        let f = _wfopen(w(&file).as_ptr(), w("wb").as_ptr());
        assert!(!f.is_null(), "{file}");
        host!("ucrtbase.dll", "fclose": extern "C" fn(File) -> i32)(f);
        assert_ne!(DeleteFileW(w(&format!("{deep}/f.txt")).as_ptr()), 0);
        assert_ne!(RemoveDirectoryW(w(&deep).as_ptr()), 0);
    }

    #[test]
    fn title_file_calls_land_in_the_host_folder() {
        let dir = tempfile::tempdir().unwrap();
        let letter = mount(dir.path()).unwrap();
        const GENERIC_WRITE: u32 = 0x4000_0000;
        const CREATE_ALWAYS: u32 = 2;
        let close = host!("kernel32.dll", "CloseHandle": extern "system" fn(Handle) -> i32);
        let fclose = host!("ucrtbase.dll", "fclose": extern "C" fn(File) -> i32);

        let path = w(&format!(r"{letter}:\created.bin"));
        let null = std::ptr::null_mut();
        let h = CreateFileW(
            path.as_ptr(),
            GENERIC_WRITE,
            0,
            null,
            CREATE_ALWAYS,
            0,
            null,
        );
        assert_ne!(h as isize, -1);
        close(h);
        assert!(dir.path().join("created.bin").is_file());

        let path = format!("{letter}:/opened.txt\0");
        let f = fopen(path.as_ptr().cast(), c"wb".as_ptr());
        assert!(!f.is_null());
        fclose(f);
        assert!(dir.path().join("opened.txt").is_file());

        let path = format!("{letter}:/stream.txt\0");
        const OUT: i32 = 2;
        let f = fiopen(path.as_ptr().cast(), OUT, 0x40);
        assert!(!f.is_null());
        fclose(f);
        assert!(dir.path().join("stream.txt").is_file());

        assert_eq!(remove(path.as_ptr().cast()), 0);
        assert!(!dir.path().join("stream.txt").exists());
        let f = fiopen_wide(w(&format!("{letter}:/wide.txt")).as_ptr(), OUT, 0x40);
        assert!(!f.is_null());
        fclose(f);
        assert!(dir.path().join("wide.txt").is_file());

        const OPEN_EXISTING: u32 = 3;
        let path = w(&format!(r"{letter}:\created.bin"));
        let h = CreateFile2(path.as_ptr(), GENERIC_WRITE, 0, OPEN_EXISTING, null);
        assert_ne!(h as isize, -1);
        close(h);
        const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;
        assert_ne!(GetFileAttributesW(path.as_ptr()), INVALID_FILE_ATTRIBUTES);

        const O_WRONLY: i32 = 1;
        const O_CREAT: i32 = 0x100;
        const S_IWRITE: i32 = 0x80;
        let path = format!("{letter}:/low.bin\0");
        let fd = _open(path.as_ptr().cast(), O_WRONLY | O_CREAT, S_IWRITE);
        assert!(fd >= 0);
        host!("ucrtbase.dll", "_close": extern "C" fn(i32) -> i32)(fd);
        assert!(dir.path().join("low.bin").is_file());

        // Other paths pass through untouched.
        let host_path = format!("{}\0", dir.path().join("plain.txt").display());
        let f = fopen(host_path.as_ptr().cast(), c"wb".as_ptr());
        assert!(!f.is_null());
        fclose(f);
        assert!(dir.path().join("plain.txt").is_file());
    }

    /// A narrow path is rewritten once, as its wide twin is. The inner
    /// drive's folder lies on the outer virtual drive, as the title folder
    /// does when it sits on a host `G:` drive and is mounted as `G:`. The
    /// outer letter is no host drive, so the host finds nothing there; a
    /// narrow call that reaches the outer folder rewrote the path twice.
    #[test]
    fn narrow_paths_are_rewritten_once() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("inner");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("present.txt"), b"").unwrap();
        let outer = mount(dir.path()).unwrap();
        let inner = mount(Path::new(&format!(r"{outer}:\inner"))).unwrap();
        let path = |name: &str| format!("{inner}:/{name}\0");

        const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;
        let wide = GetFileAttributesW(w(&format!("{inner}:/present.txt")).as_ptr());
        assert_eq!(wide, INVALID_FILE_ATTRIBUTES);
        let narrow = GetFileAttributesA(path("present.txt").as_ptr().cast());
        assert_eq!(narrow, wide, "GetFileAttributesA agrees with its wide twin");

        const GENERIC_WRITE: u32 = 0x4000_0000;
        const CREATE_ALWAYS: u32 = 2;
        let null = std::ptr::null_mut();
        let h = CreateFileA(
            path("a.bin").as_ptr().cast(),
            GENERIC_WRITE,
            0,
            null,
            CREATE_ALWAYS,
            0,
            null,
        );
        assert_eq!(h as isize, -1);
        assert!(fopen(path("b.txt").as_ptr().cast(), c"wb".as_ptr()).is_null());
        let mut f: File = std::ptr::null_mut();
        let error = fopen_s(&mut f, path("c.txt").as_ptr().cast(), c"wb".as_ptr());
        assert_ne!(error, 0);
        const OUT: i32 = 2;
        assert!(fiopen(path("d.txt").as_ptr().cast(), OUT, 0x40).is_null());

        // The narrow functions bound later, each against the file that is
        // there or a name that is free.
        let present = path("present.txt");
        let present = present.as_ptr().cast::<c_char>();
        assert_eq!(CreateDirectoryA(path("e").as_ptr().cast(), null), 0);
        assert_eq!(RemoveDirectoryA(path("e").as_ptr().cast()), 0);
        assert_eq!(SetFileAttributesA(present, 0x80), 0);
        let mut info = [0u32; 9];
        assert_eq!(
            GetFileAttributesExA(present, 0, info.as_mut_ptr().cast()),
            0
        );
        let mut data = WIN32_FIND_DATAA::default();
        assert_eq!(FindFirstFileA(present, &mut data), INVALID_HANDLE);
        assert_eq!(
            FindFirstFileExA(present, 0, &mut data, 0, null, 0),
            INVALID_HANDLE
        );
        assert_eq!(
            MoveFileExA(present, path("moved.txt").as_ptr().cast(), 0),
            0
        );
        assert_eq!(rename(present, path("renamed.txt").as_ptr().cast()), -1);
        assert_eq!(_mkdir(path("f").as_ptr().cast()), -1);
        assert_eq!(_rmdir(path("f").as_ptr().cast()), -1);
        assert_eq!(_access(present, 0), -1);
        let mut stat = [0u64; 7];
        assert_eq!(_stat64(present, stat.as_mut_ptr().cast()), -1);

        // Nothing was made behind a path rewritten twice.
        let names: Vec<_> = std::fs::read_dir(&folder)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["present.txt"]);
    }

    /// A temporary folder on a drive letter of its own, which it gives
    /// back at the end.
    struct Drive {
        dir: tempfile::TempDir,
        letter: char,
    }

    impl Drive {
        fn new() -> Drive {
            let dir = tempfile::tempdir().unwrap();
            let letter = mount(dir.path()).expect("a free drive letter");
            Drive { dir, letter }
        }

        /// `tail` on the drive, as a title's wide path.
        fn wide(&self, tail: &str) -> Vec<u16> {
            w(&format!(r"{}:\{tail}", self.letter))
        }

        /// `tail` on the drive, as a title's narrow path.
        fn narrow(&self, tail: &str) -> std::ffi::CString {
            std::ffi::CString::new(format!(r"{}:\{tail}", self.letter)).unwrap()
        }

        /// `tail` in the host folder.
        fn host(&self, tail: &str) -> std::path::PathBuf {
            self.dir.path().join(tail)
        }

        fn read(&self, tail: &str) -> Option<Vec<u8>> {
            std::fs::read(self.host(tail)).ok()
        }
    }

    impl Drop for Drive {
        fn drop(&mut self) {
            unmount(self.letter);
        }
    }

    /// The first `n` units of a buffer a path query filled.
    fn units(buffer: &[u16], n: u32) -> String {
        String::from_utf16_lossy(&buffer[..n as usize])
    }

    /// Each wide function bound for a title, called with paths on a
    /// virtual drive, by what it leaves in the host folder. Every one that
    /// fails is named.
    #[test]
    fn every_bound_path_api_lands_in_the_host_folder() {
        let d = Drive::new();
        let null = std::ptr::null_mut::<c_void>();
        let mut failed: Vec<&str> = Vec::new();
        let mut check = |name: &'static str, ok: bool| {
            if !ok {
                failed.push(name);
            }
        };
        let abc = Some(b"abc".to_vec());
        std::fs::write(d.host("a.txt"), b"abc").unwrap();

        let moved = MoveFileW(d.wide("a.txt").as_ptr(), d.wide("b.txt").as_ptr());
        let there = !d.host("a.txt").exists() && d.read("b.txt") == abc;
        check("MoveFileW", moved != 0 && there);

        let (b, c) = (d.wide("b.txt"), d.wide("c.txt"));
        let copied = CopyFileW(b.as_ptr(), c.as_ptr(), 1);
        check("CopyFileW", copied != 0 && d.read("c.txt") == abc);
        let again = CopyFileW(b.as_ptr(), c.as_ptr(), 1);
        check("CopyFileW refuses to overwrite", again == 0);

        let result = CopyFile2(b.as_ptr(), d.wide("d.txt").as_ptr(), null);
        check("CopyFile2", result == 0 && d.read("d.txt") == abc);

        let linked = CreateHardLinkW(d.wide("e.txt").as_ptr(), b.as_ptr(), null);
        check("CreateHardLinkW", linked != 0 && d.read("e.txt") == abc);

        std::fs::write(d.host("new.txt"), b"new").unwrap();
        let none = std::ptr::null();
        let replaced = ReplaceFileW(c.as_ptr(), d.wide("new.txt").as_ptr(), none, 0, null, null);
        let there = d.read("c.txt") == Some(b"new".to_vec()) && !d.host("new.txt").exists();
        check("ReplaceFileW", replaced != 0 && there);

        // The host volume the folder is on answers for the drive.
        let (mut longest, mut file_system) = (0u32, [0u16; 64]);
        let no = std::ptr::null_mut();
        let ok = GetVolumeInformationW(
            d.wide("").as_ptr(),
            std::ptr::null_mut(),
            0,
            no,
            &mut longest,
            no,
            file_system.as_mut_ptr(),
            64,
        );
        check(
            "GetVolumeInformationW",
            ok != 0 && longest > 0 && file_system[0] != 0,
        );

        // The C runtime.
        let sub = d.wide("sub");
        check(
            "_wmkdir",
            _wmkdir(sub.as_ptr()) == 0 && d.host("sub").is_dir(),
        );
        let absent = d.wide("absent");
        check(
            "_waccess",
            _waccess(sub.as_ptr(), 0) == 0 && _waccess(absent.as_ptr(), 0) == -1,
        );
        let inside = d.wide(r"sub\b.txt");
        let renamed = _wrename(b.as_ptr(), inside.as_ptr());
        check("_wrename", renamed == 0 && d.read("sub/b.txt") == abc);
        // `struct _stat64i32` (48 bytes): its 32-bit `st_size` is at byte 20.
        let mut stat = [0u64; 8];
        let ok = _wstat64i32(inside.as_ptr(), stat.as_mut_ptr().cast());
        check("_wstat64i32", ok == 0 && (stat[2] >> 32) == 3);
        // `struct _wfinddata64_t` (560 bytes): the size at byte 32, the name
        // from byte 40.
        let mut found = [0u64; 70];
        let search = _wfindfirst64(d.wide(r"sub\*.txt").as_ptr(), found.as_mut_ptr().cast());
        let name: Vec<u16> = found[5..]
            .iter()
            .flat_map(|q| (0..4).map(move |i| (q >> (16 * i)) as u16))
            .take_while(|&c| c != 0)
            .collect();
        check(
            "_wfindfirst64",
            search != -1 && found[4] == 3 && String::from_utf16_lossy(&name) == "b.txt",
        );
        if search != -1 {
            host!("ucrtbase.dll", "_findclose": extern "C" fn(isize) -> i32)(search);
        }
        let removed = _wremove(inside.as_ptr());
        check("_wremove", removed == 0 && !d.host("sub/b.txt").exists());
        check(
            "_wrmdir",
            _wrmdir(sub.as_ptr()) == 0 && !d.host("sub").exists(),
        );

        assert_eq!(failed, [] as [&str; 0], "these did not reach the folder");
    }

    /// The name in a narrow search record.
    fn found_name(data: &WIN32_FIND_DATAA) -> String {
        let name: Vec<u8> = data.cFileName.iter().map(|&c| c as u8).collect();
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        String::from_utf8_lossy(&name[..len]).into_owned()
    }

    /// The narrow twins and the narrow C runtime functions, as the wide
    /// ones above.
    #[test]
    fn narrow_and_crt_variants_are_rewritten() {
        use windows_sys::Win32::Storage::FileSystem::FindNextFileA;
        let d = Drive::new();
        let null = std::ptr::null_mut::<c_void>();
        let mut failed: Vec<&str> = Vec::new();
        let mut check = |name: &'static str, ok: bool| {
            if !ok {
                failed.push(name);
            }
        };
        std::fs::write(d.host("f1.txt"), b"one!").unwrap();
        std::fs::write(d.host("f2.txt"), b"two").unwrap();

        let made = d.narrow("made");
        let created = CreateDirectoryA(made.as_ptr(), null);
        check("CreateDirectoryA", created != 0 && d.host("made").is_dir());

        const READONLY: u32 = 1;
        const NORMAL: u32 = 0x80;
        let f1 = d.narrow("f1.txt");
        let set = SetFileAttributesA(f1.as_ptr(), READONLY);
        let read_only = std::fs::metadata(d.host("f1.txt")).map(|m| m.permissions().readonly());
        check(
            "SetFileAttributesA",
            set != 0 && read_only.ok() == Some(true),
        );
        // WIN32_FILE_ATTRIBUTE_DATA: the attributes first, the low half of
        // the size last.
        let mut info = [0u32; 9];
        let got = GetFileAttributesExA(f1.as_ptr(), 0, info.as_mut_ptr().cast());
        check(
            "GetFileAttributesExA",
            got != 0 && info[0] & READONLY != 0 && info[8] == 4,
        );
        SetFileAttributesA(f1.as_ptr(), NORMAL);

        // The first record is ours, made from the host's wide one; the
        // host's own FindNextFileA goes on with the same search.
        let mut data = WIN32_FIND_DATAA::default();
        let search = FindFirstFileA(d.narrow("f*.txt").as_ptr(), &mut data);
        let mut names = vec![found_name(&data)];
        let first_size = data.nFileSizeLow;
        // SAFETY: the search handle (or the invalid one, which fails) and
        // a record to fill.
        let more = unsafe { FindNextFileA(search, &mut data) };
        names.push(found_name(&data));
        names.sort();
        let sizes = first_size + data.nFileSizeLow;
        check(
            "FindFirstFileA",
            search != INVALID_HANDLE && more != 0 && names == ["f1.txt", "f2.txt"] && sizes == 7,
        );
        if search != INVALID_HANDLE {
            // SAFETY: the search opened above.
            unsafe { FindClose(search) };
        }
        let mut data = WIN32_FIND_DATAA::default();
        let search = FindFirstFileExA(f1.as_ptr(), 0, &mut data, 0, null, 0);
        check(
            "FindFirstFileExA",
            search != INVALID_HANDLE && found_name(&data) == "f1.txt" && data.nFileSizeLow == 4,
        );
        if search != INVALID_HANDLE {
            // SAFETY: the search opened above.
            unsafe { FindClose(search) };
        }
        let nothing = FindFirstFileA(d.narrow("absent*").as_ptr(), &mut data);
        check("FindFirstFileA finds nothing", nothing == INVALID_HANDLE);

        let g1 = d.narrow("g1.txt");
        let moved = MoveFileExA(f1.as_ptr(), g1.as_ptr(), 0);
        let there = !d.host("f1.txt").exists() && d.host("g1.txt").is_file();
        check("MoveFileExA", moved != 0 && there);
        // One path on the drive and one host path: both go to the host's
        // wide function, the second as the host would have decoded it.
        match d.host("h1.txt").to_str().filter(|path| path.is_ascii()) {
            Some(host) => {
                let host = std::ffi::CString::new(host).unwrap();
                let moved = MoveFileExA(g1.as_ptr(), host.as_ptr(), 0);
                let there = d.host("h1.txt").is_file();
                check("MoveFileExA to a host path", moved != 0 && there);
            }
            None => crate::testing::skip("the temporary folder's path is not ASCII"),
        }

        let renamed = rename(d.narrow("f2.txt").as_ptr(), d.narrow("r2.txt").as_ptr());
        check("rename", renamed == 0 && d.host("r2.txt").is_file());
        let crt = d.narrow("crt");
        check(
            "_mkdir",
            _mkdir(crt.as_ptr()) == 0 && d.host("crt").is_dir(),
        );
        let absent = d.narrow("absent");
        check(
            "_access",
            _access(crt.as_ptr(), 0) == 0 && _access(absent.as_ptr(), 0) == -1,
        );
        // `struct _stat64` (56 bytes): its 64-bit `st_size` is at byte 24.
        let mut stat = [0u64; 7];
        let ok = _stat64(d.narrow("r2.txt").as_ptr(), stat.as_mut_ptr().cast());
        check("_stat64", ok == 0 && stat[3] == 3);
        check(
            "_rmdir",
            _rmdir(crt.as_ptr()) == 0 && !d.host("crt").exists(),
        );
        let removed = RemoveDirectoryA(made.as_ptr());
        check("RemoveDirectoryA", removed != 0 && !d.host("made").exists());

        assert_eq!(failed, [] as [&str; 0], "these did not reach the folder");
    }

    /// A name that does not fit the narrow record is refused, not cut.
    #[test]
    fn a_found_name_must_fit_the_narrow_record() {
        let name = w("save.dat");
        let mut field = [0xffu8; 9];
        assert!(narrow_name(&name, &mut field));
        assert_eq!(&field, b"save.dat\0");
        let mut short = [0xffu8; 8];
        assert!(!narrow_name(&name, &mut short), "no room for the NUL");
        // Only up to the name's NUL; an empty name is an empty string.
        let mut field = [0xffu8; 4];
        assert!(narrow_name(&w("ab\0cdefgh")[..], &mut field));
        assert_eq!(&field[..3], b"ab\0");
        assert!(narrow_name(&[0u16; 14], &mut field));
        assert_eq!(field[0], 0);

        // A search whose name does not fit ends, with the error for the
        // thread; so does one the title gave no record for.
        use windows_sys::Win32::Foundation::GetLastError;
        let d = Drive::new();
        std::fs::write(d.host("f.txt"), b"").unwrap();
        let mut found = WIN32_FIND_DATAW::default();
        let search = FindFirstFileW(d.wide("f.txt").as_ptr(), (&raw mut found).cast());
        assert_ne!(search, INVALID_HANDLE);
        assert_eq!(
            found_narrow(search, &found, std::ptr::null_mut()),
            INVALID_HANDLE
        );
        // SAFETY: the thread's last error.
        assert_eq!(unsafe { GetLastError() }, ERROR_INVALID_PARAMETER);
    }

    #[test]
    fn final_path_names_use_the_drive_letter() {
        use windows_sys::Win32::Foundation::GetLastError;
        const GENERIC_READ: u32 = 0x8000_0000;
        const SHARE_ALL: u32 = 7;
        const OPEN_EXISTING: u32 = 3;
        let d = Drive::new();
        std::fs::create_dir(d.host("sub")).unwrap();
        std::fs::write(d.host("sub/f.bin"), b"x").unwrap();
        let close = host!("kernel32.dll", "CloseHandle": extern "system" fn(Handle) -> i32);
        let null = std::ptr::null_mut();
        let open = |path: &[u16]| {
            let (access, share) = (GENERIC_READ, SHARE_ALL);
            CreateFileW(path.as_ptr(), access, share, null, OPEN_EXISTING, 0, null)
        };
        let file = open(&d.wide(r"sub\f.bin"));
        assert_ne!(file, INVALID_HANDLE);

        // With the drive letter (the default), in the `\\?\` form.
        let mut buffer = [0u16; 600];
        let n = GetFinalPathNameByHandleW(file, buffer.as_mut_ptr(), 600, 0);
        let full = format!(r"\\?\{}:\sub\f.bin", d.letter);
        assert_eq!(units(&buffer, n), full);
        assert_eq!(buffer[n as usize], 0, "NUL-terminated");
        // A buffer too small by the NUL: the size needed, nothing written.
        buffer[0] = 7;
        assert_eq!(
            GetFinalPathNameByHandleW(file, buffer.as_mut_ptr(), n, 0),
            n + 1
        );
        assert_eq!(buffer[0], 7);
        assert_eq!(
            GetFinalPathNameByHandleW(file, std::ptr::null_mut(), 0, 0),
            n + 1
        );
        // The name it was opened by is on the drive too.
        let n = GetFinalPathNameByHandleW(file, buffer.as_mut_ptr(), 600, FILE_NAME_OPENED);
        assert_eq!(units(&buffer, n), full);
        // Without a volume name: the path below the drive's root.
        let n = GetFinalPathNameByHandleW(file, buffer.as_mut_ptr(), 600, VOLUME_NAME_NONE);
        assert_eq!(units(&buffer, n), r"\sub\f.bin");
        // A virtual drive has no volume GUID path and no NT device.
        for flags in [
            VOLUME_NAME_GUID,
            VOLUME_NAME_NT,
            VOLUME_NAME_GUID | FILE_NAME_OPENED,
        ] {
            // SAFETY: the thread's last error.
            unsafe { SetLastError(0) };
            let n = GetFinalPathNameByHandleW(file, buffer.as_mut_ptr(), 600, flags);
            // SAFETY: as above.
            assert_eq!((n, unsafe { GetLastError() }), (0, 3), "flags {flags}");
        }
        // The answer names the file for the file layer.
        let again = open(&w(&full));
        assert_ne!(again, INVALID_HANDLE);
        close(again);
        close(file);

        // A file outside every drive: the host's own answer.
        let exe: Vec<u16> = std::env::current_exe()
            .unwrap()
            .as_os_str()
            .encode_wide()
            .chain([0])
            .collect();
        let file = open(&exe);
        assert_ne!(file, INVALID_HANDLE);
        let get = host!("kernel32.dll", "GetFinalPathNameByHandleW": extern "system" fn(Handle, *mut u16, u32, u32) -> u32);
        let mut theirs = [0u16; 600];
        let theirs_n = get(file, theirs.as_mut_ptr(), 600, 0);
        let n = GetFinalPathNameByHandleW(file, buffer.as_mut_ptr(), 600, 0);
        close(file);
        assert_ne!(theirs_n, 0);
        assert_eq!(units(&buffer, n), units(&theirs, theirs_n));
    }

    /// Puts the process's current directory back.
    struct Cwd(std::path::PathBuf);

    impl Drop for Cwd {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

    #[test]
    fn current_directory_round_trips_on_a_virtual_drive() {
        let d = Drive::new();
        std::fs::create_dir(d.host("sub")).unwrap();
        // Dropped before `d`: the folder cannot go while the process is in it.
        let _back = Cwd(std::env::current_dir().unwrap());
        let sub = format!(r"{}:\sub", d.letter);
        assert_ne!(SetCurrentDirectoryW(w(&sub).as_ptr()), 0);
        assert!(
            std::env::current_dir().unwrap().ends_with("sub"),
            "the host is in the folder"
        );
        assert!(std::fs::write("relative.txt", b"r").is_ok());
        assert!(d.host("sub/relative.txt").is_file());

        // The title is told its own path, not the host's.
        let mut buffer = [0u16; 300];
        let n = GetCurrentDirectoryW(300, buffer.as_mut_ptr());
        assert_eq!(units(&buffer, n), sub);
        assert_eq!(buffer[n as usize], 0, "NUL-terminated");
        assert_eq!(GetCurrentDirectoryW(0, std::ptr::null_mut()), n + 1);
        buffer[0] = 7;
        assert_eq!(GetCurrentDirectoryW(n, buffer.as_mut_ptr()), n + 1);
        assert_eq!(buffer[0], 7, "nothing written to a buffer too small");
        let mut narrow = [0u8; 300];
        let n = GetCurrentDirectoryA(300, narrow.as_mut_ptr());
        assert_eq!(std::str::from_utf8(&narrow[..n as usize]), Ok(sub.as_str()));
        assert_eq!(GetCurrentDirectoryA(n, narrow.as_mut_ptr()), n + 1);

        // A relative path is completed on the drive; the file part points
        // at the name.
        let mut part = std::ptr::null_mut();
        let rel = w("rel.txt");
        let n = GetFullPathNameW(rel.as_ptr(), 300, buffer.as_mut_ptr(), &mut part);
        assert_eq!(units(&buffer, n), format!(r"{sub}\rel.txt"));
        assert_eq!(part, buffer[n as usize - 7..].as_mut_ptr());
        assert_eq!(
            GetFullPathNameW(rel.as_ptr(), n, buffer.as_mut_ptr(), &mut part),
            n + 1,
            "too small by the NUL"
        );
        // A folder has no file part.
        let n = GetFullPathNameW(w(r".\").as_ptr(), 300, buffer.as_mut_ptr(), &mut part);
        assert_eq!(units(&buffer, n), format!(r"{sub}\"));
        assert!(part.is_null());
        // A path on the drive is completed by its text.
        let dotted = w(&format!(r"{}:\a\..\b\c.txt", d.letter));
        let n = GetFullPathNameW(dotted.as_ptr(), 300, buffer.as_mut_ptr(), &mut part);
        assert_eq!(units(&buffer, n), format!(r"{}:\b\c.txt", d.letter));
        // A host path stays one.
        let host = w(r"C:\Windows\..\x.txt");
        let n = GetFullPathNameW(host.as_ptr(), 300, buffer.as_mut_ptr(), &mut part);
        assert_eq!(units(&buffer, n), r"C:\x.txt");

        let mut part = std::ptr::null_mut();
        let n = GetFullPathNameA(c"rel.txt".as_ptr(), 300, narrow.as_mut_ptr(), &mut part);
        let expected = format!(r"{sub}\rel.txt");
        assert_eq!(
            std::str::from_utf8(&narrow[..n as usize]),
            Ok(expected.as_str())
        );
        assert_eq!(part, narrow[n as usize - 7..].as_mut_ptr());
        let n = GetFullPathNameA(c".\\".as_ptr(), 300, narrow.as_mut_ptr(), &mut part);
        assert_eq!(narrow[n as usize - 1], b'\\');
        assert!(part.is_null());

        // The drive's root, set through the narrow function.
        assert_ne!(SetCurrentDirectoryA(d.narrow("").as_ptr()), 0);
        let n = GetCurrentDirectoryW(300, buffer.as_mut_ptr());
        assert_eq!(units(&buffer, n), format!(r"{}:\", d.letter));
    }

    #[test]
    fn the_temporary_folder_is_named_by_its_drive_when_mounted() {
        let get = host!("kernel32.dll", "GetTempPathW": extern "system" fn(u32, *mut u16) -> u32);
        let (mut theirs, mut ours) = ([0u16; 600], [0u16; 600]);
        let n = get(600, theirs.as_mut_ptr());
        assert_ne!(n, 0);
        // No test mounts the folder that holds every test's own: the
        // host's answer passes through.
        let m = GetTempPathW(600, ours.as_mut_ptr());
        assert_eq!(units(&ours, m), units(&theirs, n));

        let letter = mount(Path::new(&units(&theirs, n))).expect("a free drive letter");
        let m = GetTempPathW(600, ours.as_mut_ptr());
        let too_small = GetTempPathW(3, ours[300..].as_mut_ptr());
        unmount(letter);
        assert_eq!(units(&ours, m), format!(r"{letter}:\"));
        assert_eq!(too_small, 4, "the size with the NUL");
    }

    /// Each function this file binds for a title resolves to it, under the
    /// DLL names a title imports it by.
    #[test]
    fn every_bound_name_is_registered() {
        use victory_loader::Provenance;
        const WIN32: &[&str] = &[
            "CopyFile2",
            "CopyFileW",
            "CreateDirectoryA",
            "CreateDirectoryW",
            "CreateFile2",
            "CreateFileA",
            "CreateFileW",
            "CreateHardLinkW",
            "DeleteFileA",
            "DeleteFileW",
            "FindFirstFileA",
            "FindFirstFileExA",
            "FindFirstFileExW",
            "FindFirstFileW",
            "GetCurrentDirectoryA",
            "GetCurrentDirectoryW",
            "GetDiskFreeSpaceExW",
            "GetDiskFreeSpaceW",
            "GetFileAttributesA",
            "GetFileAttributesExA",
            "GetFileAttributesExW",
            "GetFileAttributesW",
            "GetFinalPathNameByHandleW",
            "GetFullPathNameA",
            "GetFullPathNameW",
            "GetTempPathW",
            "GetVolumeInformationW",
            "MoveFileExA",
            "MoveFileExW",
            "MoveFileW",
            "RemoveDirectoryA",
            "RemoveDirectoryW",
            "ReplaceFileW",
            "SetCurrentDirectoryA",
            "SetCurrentDirectoryW",
            "SetFileAttributesA",
            "SetFileAttributesW",
        ];
        const CRT: &[&str] = &[
            "_access",
            "_mkdir",
            "_rmdir",
            "_stat64",
            "_stat64i32",
            "_waccess",
            "_wchmod",
            "_wfindfirst64",
            "_wmkdir",
            "_wremove",
            "_wrename",
            "_wrmdir",
            "_wstat64",
            "_wstat64i32",
            "remove",
            "rename",
        ];
        // The API sets are the ones the functions' reference pages name.
        const API_SETS: &[(&str, &str)] = &[
            ("api-ms-win-core-file-l1-2-2.dll", "GetFullPathNameW"),
            (
                "api-ms-win-core-file-l1-2-2.dll",
                "GetFinalPathNameByHandleW",
            ),
            ("api-ms-win-core-file-l2-1-2.dll", "CopyFileW"),
            ("api-ms-win-core-kernel32-legacy-l1-1-0.dll", "CopyFileW"),
            ("api-ms-win-core-kernel32-legacy-l1-1-0.dll", "MoveFileW"),
            ("api-ms-win-core-kernel32-legacy-l1-1-0.dll", "MoveFileExA"),
            (
                "api-ms-win-core-processenvironment-l1-2-0.dll",
                "GetCurrentDirectoryW",
            ),
        ];
        let resolver = crate::HleResolver::default();
        let ours = |dll: &str, name: &str| {
            let symbol = victory_pe::ImportSymbol::Name {
                hint: 0,
                name: name.into(),
            };
            resolver.classify(dll, &symbol) == Some(Provenance::Hle)
        };
        let mut unbound: Vec<String> = Vec::new();
        let pairs = (WIN32.iter().map(|name| ("kernel32.dll", *name)))
            .chain(WIN32.iter().map(|name| ("kernelbase.dll", *name)))
            .chain(CRT.iter().map(|name| ("ucrtbase.dll", *name)))
            .chain(
                CRT.iter()
                    .map(|name| ("api-ms-win-crt-filesystem-l1-1-0.dll", *name)),
            )
            .chain(API_SETS.iter().copied());
        for (dll, name) in pairs {
            if !ours(dll, name) {
                unbound.push(format!("{dll}!{name}"));
            }
        }
        assert_eq!(unbound, [] as [String; 0]);
    }
}
