//! XGameSave: the user's game saves, kept on the host under the title's
//! data directory, next to persistent local storage (`runtime::Title::
//! saves`), never in the cache. A provider is a folder per user and
//! service configuration, a container a folder in it.
//!
//! An update is all or nothing, as documented: each one writes a complete
//! new version of the container (`v<N>.new/`, unchanged blobs hard-linked
//! from the previous version), flushed to disk, and one directory rename
//! commits it (`v<N>`); the highest committed version is the container.
//! A crash leaves the old version or the new one, never a mix; leftovers
//! are removed on sight. Deleting a container renames it away first.
//!
//! Names are exact: a version's `manifest` holds the container's name, its
//! display name and the blob names (blob `i` is the file `i`), so case,
//! characters the host forbids, reserved device names and length never
//! reach a file name. A container's folder is its name made safe (and
//! shortened) plus a hash of the exact name, so names differing only in
//! case get different folders. Offline there is no cloud copy, so nothing
//! ever needs a sync.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{CString, c_char, c_void};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::xasync::{self, AsyncBlock, E_INVALIDARG};
use super::xgameruntime::{
    Guid, HRESULT, Interface, Methods, Object, S_OK, add_ref, missing, put, text,
};
use super::{storage, xuser};
use crate::runtime;

pub const PROVIDER: Guid = Guid::new(
    0x704c_3f58,
    0xe629,
    0x4cc2,
    [0xb1, 0x97, 0x30, 0x51, 0x1b, 0x99, 0x6f, 0xe2],
);
/// As Grand Theft Auto VI (2026) asks for it; every slot it calls takes
/// the public API's arguments in the same order as under the provider's
/// own id (older titles).
pub const IID_2026: Guid = Guid::new(
    0x1bff_f3af,
    0xf14a,
    0x40a3,
    [0x8e, 0x35, 0x9a, 0xda, 0x90, 0x65, 0x93, 0xf9],
);

pub static OBJECT: Object = Object::new("game save", &VTABLE);
pub const INTERFACES: &[Interface] =
    &[(PROVIDER, PROVIDER, &OBJECT), (PROVIDER, IID_2026, &OBJECT)];

// The public API's error codes.
pub const INVALID_CONTAINER_NAME: HRESULT = 0x8083_0001_u32 as i32;
pub const NO_ACCESS: HRESULT = 0x8083_0002_u32 as i32;
pub const OUT_OF_LOCAL_STORAGE: HRESULT = 0x8083_0003_u32 as i32;
pub const UPDATE_TOO_BIG: HRESULT = 0x8083_0005_u32 as i32;
pub const QUOTA_EXCEEDED: HRESULT = 0x8083_0006_u32 as i32;
pub const BUFFER_TOO_SMALL: HRESULT = 0x8083_0007_u32 as i32;
pub const BLOB_NOT_FOUND: HRESULT = 0x8083_0008_u32 as i32;

/// Per user and title, as documented.
const QUOTA: u64 = 256 << 20;
/// What one update may write in all, and so the most one blob may hold:
/// the public documentation gives both the same limit, `GS_MAX_BLOB_SIZE`
/// (16 MB), and `E_GS_UPDATE_TOO_BIG` as the error past it.
const MAX_UPDATE: usize = 16 << 20;
/// Plain COM failure, for a result that is not what we built.
const E_FAIL: HRESULT = 0x8000_4005_u32 as i32;
const MANIFEST: &str = "manifest";
/// Readable part of a container's folder name, at most.
const READABLE: usize = 100;

/// Every save operation, so none sees another's half-made version.
static SAVES: Mutex<()> = Mutex::new(());

fn saves_lock() -> std::sync::MutexGuard<'static, ()> {
    SAVES.lock().unwrap_or_else(|e| e.into_inner())
}

/// `name` as a file name: letters, digits, `-` and `_` stay, every other
/// byte becomes `%XX`. The result never contains a dot.
fn file_name(name: &str) -> String {
    name.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The name behind a file name from [`file_name`]; `None` for other files.
fn decode(file: &str) -> Option<String> {
    if file.contains('.') {
        return None;
    }
    let mut out = Vec::new();
    let mut bytes = file.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hex = [bytes.next()?, bytes.next()?];
            out.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            out.push(b);
        }
    }
    String::from_utf8(out).ok()
}

/// The container's folder: its name made safe, cut short, then `~` and a
/// hash of the exact name (64-bit FNV-1a, stable across builds).
fn container_dir(root: &Path, name: &str) -> PathBuf {
    let hash = name.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    });
    let mut readable = file_name(name);
    readable.truncate(READABLE); // ASCII only
    root.join(format!("{readable}~{hash:016x}"))
}

/// Documented: the path part (up to the last `/`) letters, digits, `_` and
/// `/`; the file part letters, digits, `_`, `.` and `-`, not empty, not
/// ending in `.`, no `..`; 256 characters at most.
fn valid_container(name: &str) -> bool {
    let (path, file) = name.rsplit_once('/').unwrap_or(("", name));
    name.len() <= 256
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'/')
        && !file.is_empty()
        && !file.ends_with('.')
        && !file.contains("..")
        && file
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// A committed version of a container.
struct Version {
    dir: PathBuf,
    number: u64,
    name: String,
    display: String,
    /// Blob names; blob `i` is the file `i`.
    blobs: Vec<String>,
}

impl Version {
    fn blob(&self, i: usize) -> PathBuf {
        self.dir.join(i.to_string())
    }

    fn sizes(&self) -> Vec<(String, u64)> {
        let size = |i| std::fs::metadata(self.blob(i)).map_or(0, |m| m.len());
        let blobs = self.blobs.iter().enumerate();
        blobs.map(|(i, n)| (n.clone(), size(i))).collect()
    }
}

/// The manifest: length-prefixed strings (u32 LE): container name, display
/// name, then each blob name.
fn manifest(name: &str, display: &str, blobs: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for s in [name, display].into_iter().chain(blobs.iter().copied()) {
        out.extend_from_slice(&(s.len() as u32).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    out
}

fn parse_manifest(bytes: &[u8]) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let (len, tail) = rest.split_first_chunk::<4>()?;
        let len = u32::from_le_bytes(*len) as usize;
        out.push(String::from_utf8(tail.get(..len)?.to_vec()).ok()?);
        rest = &tail[len..];
    }
    (out.len() >= 2).then_some(out)
}

/// The container's newest committed version, if any. Removes what a crash
/// left: unfinished versions and older ones.
fn current(dir: &Path) -> std::io::Result<Option<Version>> {
    let mut committed = Vec::new();
    for entry in std::fs::read_dir(dir)?.flatten() {
        let file = entry.file_name();
        let file = file.to_string_lossy();
        if file.ends_with(".new") {
            std::fs::remove_dir_all(entry.path())?;
        } else if let Some(n) = file.strip_prefix('v').and_then(|n| n.parse::<u64>().ok()) {
            committed.push(n);
        }
    }
    committed.sort_unstable();
    let Some(&number) = committed.last() else {
        return Ok(None);
    };
    for old in &committed[..committed.len() - 1] {
        std::fs::remove_dir_all(dir.join(format!("v{old}")))?;
    }
    let version = dir.join(format!("v{number}"));
    let bad = || std::io::Error::new(std::io::ErrorKind::InvalidData, "bad save manifest");
    let mut names = parse_manifest(&std::fs::read(version.join(MANIFEST))?).ok_or_else(bad)?;
    let blobs = names.split_off(2);
    let [name, display] = <[String; 2]>::try_from(names).map_err(|_| bad())?;
    Ok(Some(Version {
        dir: version,
        number,
        name,
        display,
        blobs,
    }))
}

/// The version of container `name` in `dir`; none for a missing container
/// or another name that hashes alike.
fn current_named(dir: &Path, name: &str) -> std::io::Result<Option<Version>> {
    match current(dir) {
        Ok(v) => Ok(v.filter(|v| v.name == name)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Where a crash test stops the process (test builds only).
fn step() {
    #[cfg(test)]
    tests::crash_point();
}

/// A blob of a new version: new data, or the previous version's file.
enum Source<'a> {
    Data(&'a [u8]),
    Kept(PathBuf),
}

/// Writes version `number` of the container in `dir` and commits it.
fn commit(
    dir: &Path,
    number: u64,
    name: &str,
    display: &str,
    blobs: &[(&str, Source<'_>)],
) -> std::io::Result<()> {
    let staged = dir.join(format!("v{number}.new"));
    if staged.exists() {
        std::fs::remove_dir_all(&staged)?;
    }
    std::fs::create_dir_all(&staged)?;
    let write = |path: PathBuf, data: &[u8]| -> std::io::Result<()> {
        let mut file = std::fs::File::create(path)?;
        file.write_all(data)?;
        file.sync_all()
    };
    for (i, (_, source)) in blobs.iter().enumerate() {
        let path = staged.join(i.to_string());
        match source {
            Source::Data(data) => write(path, data)?,
            // Files are never changed in place, so sharing one is safe.
            Source::Kept(from) => {
                std::fs::hard_link(from, &path).or_else(|_| std::fs::copy(from, &path).map(drop))?
            }
        }
        step();
    }
    let names: Vec<&str> = blobs.iter().map(|(n, _)| *n).collect();
    write(staged.join(MANIFEST), &manifest(name, display, &names))?;
    step();
    std::fs::rename(&staged, dir.join(format!("v{number}")))?;
    step();
    Ok(())
}

/// The containers under a provider's root, by exact name; containers in
/// the old one-file-per-blob layout are converted first.
fn containers(root: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let path = entry.path();
        let file = entry.file_name().to_string_lossy().into_owned();
        if !path.is_dir() {
            continue;
        }
        if file.ends_with(".gone") {
            let _ = std::fs::remove_dir_all(&path);
        } else if file.contains('~') {
            if let Ok(Some(v)) = current(&path) {
                out.push((v.name, path));
            }
        } else if let Some(name) = decode(&file)
            && let Some(dir) = migrate(root, &name, &path)
        {
            out.push((name, dir));
        }
    }
    out.sort();
    out
}

/// Moves a container of the old layout (blob files named by encoded name,
/// a `.display` file) into a committed version, then removes it.
fn migrate(root: &Path, name: &str, old: &Path) -> Option<PathBuf> {
    let dir = container_dir(root, name);
    let done = current_named(&dir, name).ok()?.is_some(); // a crash after the commit
    if !done {
        let display = std::fs::read_to_string(old.join(".display")).unwrap_or_default();
        let mut blobs: Vec<(String, Vec<u8>)> = std::fs::read_dir(old)
            .ok()?
            .flatten()
            .filter_map(|e| {
                let blob = decode(e.file_name().to_str()?)?;
                Some((blob, std::fs::read(e.path()).ok()?))
            })
            .collect();
        blobs.sort();
        let sources: Vec<_> = blobs
            .iter()
            .map(|(n, d)| (n.as_str(), Source::Data(d)))
            .collect();
        commit(&dir, 1, name, &display, &sources).ok()?;
    }
    std::fs::remove_dir_all(old).ok()?;
    tracing::info!(container = name, "game save moved to the versioned layout");
    Some(dir)
}

fn used_bytes(root: &Path) -> u64 {
    containers(root)
        .iter()
        .filter_map(|(name, dir)| current_named(dir, name).ok().flatten())
        .map(|v| v.sizes().iter().map(|(_, size)| size).sum::<u64>())
        .sum()
}

struct Provider {
    root: PathBuf,
}
struct Container {
    name: String,
    dir: PathBuf,
}
struct Update {
    name: String,
    dir: PathBuf,
    display: Option<String>,
    /// Blob writes (`Some`) and deletions (`None`), in order.
    changes: Vec<(String, Option<Vec<u8>>)>,
}

/// A handle we gave out.
fn get<'a, T>(handle: *const T) -> Option<&'a T> {
    // SAFETY: handles are boxes we created and the title has not closed.
    unsafe { handle.as_ref() }
}
fn give<T>(value: T) -> *mut T {
    Box::into_raw(Box::new(value))
}
fn take<T>(handle: *mut T) {
    if !handle.is_null() {
        // SAFETY: as in `get`; closing ends the handle.
        drop(unsafe { Box::from_raw(handle) });
    }
}

impl Provider {
    /// The provider for the only user and `config` (the service
    /// configuration id), under `saves`. An empty id names no
    /// configuration, like none: as a folder name it would be the user's
    /// folder itself, where the other configurations' folders are.
    fn open(saves: &Path, config: Option<&str>) -> Result<Provider, HRESULT> {
        let config = config.filter(|c| !c.is_empty()).unwrap_or("default");
        let root = saves
            .join(format!("{:016X}", xuser::XUID))
            .join(file_name(config));
        std::fs::create_dir_all(&root).map_err(|_| NO_ACCESS)?;
        let _saves = saves_lock();
        containers(&root); // converts old containers now
        Ok(Provider { root })
    }

    fn remaining(&self) -> u64 {
        let _saves = saves_lock();
        remaining(&self.root)
    }

    fn container(&self, name: &str) -> Container {
        Container {
            name: name.to_owned(),
            dir: container_dir(&self.root, name),
        }
    }
}

/// Quota left under a provider's root, or less if the host disk is fuller.
fn remaining(root: &Path) -> u64 {
    QUOTA
        .saturating_sub(used_bytes(root))
        .min(storage::disk_free(root))
}

/// Why an update writing `written` bytes, `replaced` of them over blobs it
/// replaces, does not fit: past the per-user quota (`used` bytes kept), or
/// past the host disk's `free` bytes, which is not the user's quota (the
/// new version is written before the old one goes, so nothing it replaces
/// counts there).
fn no_room(written: u64, replaced: u64, used: u64, free: u64) -> Option<HRESULT> {
    if written > QUOTA.saturating_sub(used) + replaced {
        Some(QUOTA_EXCEEDED)
    } else if written > free {
        Some(OUT_OF_LOCAL_STORAGE)
    } else {
        None
    }
}

fn io_error(dir: &Path, e: &std::io::Error) -> HRESULT {
    tracing::warn!(dir = %dir.display(), %e, "game save");
    if e.kind() == std::io::ErrorKind::StorageFull {
        OUT_OF_LOCAL_STORAGE
    } else {
        NO_ACCESS
    }
}

impl Update {
    /// Applies the update as one new version of the container.
    fn submit(&self) -> HRESULT {
        let _saves = saves_lock();
        let current = match current(&self.dir) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return io_error(&self.dir, &e),
        };
        if current.as_ref().is_some_and(|v| v.name != self.name) {
            // Another container's name hashes alike: never mix the two.
            return NO_ACCESS;
        }
        // The blobs after the update: name → new data or the kept file.
        let mut blobs: Vec<(&str, Source<'_>)> = current
            .iter()
            .flat_map(|v| {
                let kept = v.blobs.iter().enumerate();
                kept.map(|(i, n)| (n.as_str(), Source::Kept(v.blob(i))))
            })
            .collect();
        for (name, data) in &self.changes {
            blobs.retain(|(n, _)| n != name);
            if let Some(data) = data {
                blobs.push((name, Source::Data(data)));
            }
        }
        blobs.sort_by(|a, b| a.0.cmp(b.0));
        let written: usize = blobs
            .iter()
            .map(|(_, s)| match s {
                Source::Data(d) => d.len(),
                Source::Kept(_) => 0,
            })
            .sum();
        if written > MAX_UPDATE {
            return UPDATE_TOO_BIG;
        }
        let replaced: u64 = current.as_ref().map_or(0, |v| {
            let sizes = v.sizes();
            let changed = sizes
                .iter()
                .filter(|(n, _)| self.changes.iter().any(|(c, _)| c == n));
            changed.map(|(_, size)| size).sum()
        });
        let root = self.dir.parent().unwrap_or(&self.dir);
        let free = storage::disk_free(root);
        if let Some(hr) = no_room(written as u64, replaced, used_bytes(root), free) {
            return hr;
        }
        let display = self.display.as_deref();
        let display = display.or(current.as_ref().map(|v| v.display.as_str()));
        let number = current.as_ref().map_or(1, |v| v.number + 1);
        if let Err(e) = commit(&self.dir, number, &self.name, display.unwrap_or(""), &blobs) {
            return io_error(&self.dir, &e);
        }
        // The old version is garbage now; a crash here leaves it for later.
        if let Some(old) = &current {
            let _ = std::fs::remove_dir_all(&old.dir);
            step();
        }
        S_OK
    }
}

/// Deletes a container: renamed away (one step), then removed.
fn delete(root: &Path, name: &str) -> HRESULT {
    let _saves = saves_lock();
    let dir = container_dir(root, name);
    match current_named(&dir, name) {
        Ok(Some(_)) => {}
        Ok(None) => return S_OK,
        Err(e) => return io_error(&dir, &e),
    }
    let gone = dir.with_extension("gone");
    let _ = std::fs::remove_dir_all(&gone);
    step();
    if let Err(e) = std::fs::rename(&dir, &gone) {
        return io_error(&dir, &e);
    }
    step();
    let _ = std::fs::remove_dir_all(&gone);
    S_OK
}

/// Reads the named blobs (all of them for `None`) of a container.
fn read(c: &Container, names: Option<Vec<String>>) -> Result<Vec<(String, Vec<u8>)>, HRESULT> {
    let _saves = saves_lock();
    let version = current_named(&c.dir, &c.name)
        .map_err(|e| io_error(&c.dir, &e))?
        .ok_or(BLOB_NOT_FOUND)?;
    let names = names.unwrap_or_else(|| version.blobs.clone());
    names
        .into_iter()
        .map(|name| {
            let i = version
                .blobs
                .iter()
                .position(|b| *b == name)
                .ok_or(BLOB_NOT_FOUND)?;
            let file = version.blob(i);
            let io = |e| io_error(&c.dir, &e);
            // Before it is read: a file no blob can be is not loaded.
            blob_size(std::fs::metadata(&file).map_err(io)?.len())?;
            Ok((name, std::fs::read(file).map_err(io)?))
        })
        .collect()
}

/// One blob as the public API lays it out: name, size, data.
const BLOB: usize = 24;

/// A blob's size as the public API states it, in 32 bits. Nothing we
/// write is larger (an update holds 16 MB at most), so a file that is was
/// not written by us: the container is unusable rather than the size cut
/// short.
fn blob_size(bytes: u64) -> Result<u32, HRESULT> {
    u32::try_from(bytes).map_err(|_| NO_ACCESS)
}

/// Blobs laid out as the title reads them: the blob array, then each name
/// (NUL-terminated) and its data, with pointers `base` plus offset.
fn layout(blobs: &[(String, Vec<u8>)], base: usize) -> Result<Vec<u8>, HRESULT> {
    let mut out = vec![0u8; blobs.len() * BLOB];
    for (i, (name, data)) in blobs.iter().enumerate() {
        let size = blob_size(data.len() as u64)?;
        let name_at = base.wrapping_add(out.len());
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        let data_at = base.wrapping_add(out.len());
        out.extend_from_slice(data);
        let entry = &mut out[i * BLOB..(i + 1) * BLOB];
        entry[..8].copy_from_slice(&name_at.to_le_bytes());
        entry[8..12].copy_from_slice(&size.to_le_bytes());
        entry[16..].copy_from_slice(&data_at.to_le_bytes());
    }
    Ok(out)
}

/// Makes the pointers of a [`layout`] with base 0 point into the buffer at
/// `base` that holds it, and counts its blobs. `None`, with nothing
/// changed, unless `bytes` is such a layout: the blob array (the first
/// name follows it, so its offset gives the count), then every name and
/// every blob's data inside `bytes`.
fn rebase(bytes: &mut [u8], base: usize) -> Option<u32> {
    let word = |bytes: &[u8], at: usize| {
        let word = bytes.get(at..at.checked_add(8)?)?;
        usize::try_from(u64::from_le_bytes(word.try_into().ok()?)).ok()
    };
    if bytes.is_empty() {
        return Some(0);
    }
    let array = word(bytes, 0)?;
    if array == 0 || array % BLOB != 0 || array > bytes.len() {
        return None;
    }
    let blobs = array / BLOB;
    for i in 0..blobs {
        let (name, data) = (word(bytes, i * BLOB)?, word(bytes, i * BLOB + 16)?);
        let size = bytes[i * BLOB + 8..i * BLOB + 12].try_into().ok()?;
        let size = u32::from_le_bytes(size) as usize;
        // The name and its NUL come before the data, the data ends inside.
        if name < array || name >= data || data.checked_add(size)? > bytes.len() {
            return None;
        }
    }
    for i in 0..blobs {
        for field in [i * BLOB, i * BLOB + 16] {
            let moved = word(bytes, field)?.wrapping_add(base);
            bytes[field..field + 8].copy_from_slice(&(moved as u64).to_le_bytes());
        }
    }
    u32::try_from(blobs).ok()
}

/// The names a title passes: all blobs for a null array, else `count`
/// strings, none of them null.
fn names(names: *const *const c_char, count: u32) -> Result<Option<Vec<String>>, HRESULT> {
    if names.is_null() {
        return Ok(None);
    }
    let name = |i| {
        // SAFETY: the title passes `count` strings; only as many pointers
        // are read as it takes to find one that is no string.
        text(unsafe { names.add(i).read_unaligned() }).ok_or(E_INVALIDARG)
    };
    (0..count as usize)
        .map(name)
        .collect::<Result<_, _>>()
        .map(Some)
}

type This = *const Object;
type ContainerCallback = extern "system" fn(*const ContainerInfo, *mut c_void) -> bool;
type BlobCallback = extern "system" fn(*const BlobInfo, *mut c_void) -> bool;

#[repr(C)]
pub struct ContainerInfo {
    pub name: *const c_char,
    pub display_name: *const c_char,
    pub blob_count: u32,
    pub total_size: u64,
    pub last_modified: i64,
    pub needs_sync: bool,
}

#[repr(C)]
pub struct BlobInfo {
    pub name: *const c_char,
    pub size: u32,
}

/// Calls `callback` with the information of each container that passes
/// `keep` until it returns false; never under our lock (it may call us).
fn report(
    root: &Path,
    keep: impl Fn(&str) -> bool,
    context: *mut c_void,
    callback: ContainerCallback,
) {
    let infos: Vec<(CString, CString, u32, u64, i64)> = {
        let _saves = saves_lock();
        containers(root)
            .into_iter()
            .filter(|(name, _)| keep(name))
            .filter_map(|(name, dir)| current_named(&dir, &name).ok().flatten())
            .map(|v| {
                let modified = std::fs::metadata(v.dir.join(MANIFEST))
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs() as i64);
                let size = v.sizes().iter().map(|(_, s)| s).sum();
                let count = u32::try_from(v.blobs.len()).unwrap_or(u32::MAX);
                (cstring(&v.name), cstring(&v.display), count, size, modified)
            })
            .collect()
    };
    for (name, display, blob_count, total_size, last_modified) in &infos {
        let info = ContainerInfo {
            name: name.as_ptr(),
            display_name: display.as_ptr(),
            blob_count: *blob_count,
            total_size: *total_size,
            last_modified: *last_modified,
            needs_sync: false,
        };
        if !callback(&info, context) {
            break;
        }
    }
}

fn cstring(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}

static INIT: u8 = 0;
static QUOTA_CALL: u8 = 0;
static DELETE: u8 = 0;
static READ: u8 = 0;
static SUBMIT: u8 = 0;

extern "system" fn initialize_async(
    _: This,
    _user: *const c_void,
    config: *const c_char,
    _sync_on_demand: bool,
    block: *mut AsyncBlock,
) -> HRESULT {
    let saves = runtime::title().saves();
    let result = if saves.parent().is_none_or(|p| p.as_os_str().is_empty()) {
        Err(NO_ACCESS)
    } else {
        Provider::open(&saves, text(config).as_deref()).map(|p| xasync::bytes(give(p)))
    };
    xasync::finished_with(block, (&raw const INIT).cast(), result)
}
extern "system" fn initialize_result(
    _: This,
    block: *mut AsyncBlock,
    out: *mut *mut Provider,
) -> HRESULT {
    xasync::result(block, (&raw const INIT).cast(), out)
}
extern "system" fn close_provider(_: This, provider: *mut Provider) {
    take(provider);
}

extern "system" fn quota_async(
    _: This,
    provider: *const Provider,
    block: *mut AsyncBlock,
) -> HRESULT {
    let result = get(provider)
        .ok_or(E_INVALIDARG)
        .map(|p| xasync::bytes(p.remaining() as i64));
    xasync::finished_with(block, (&raw const QUOTA_CALL).cast(), result)
}
extern "system" fn quota_result(_: This, block: *mut AsyncBlock, out: *mut i64) -> HRESULT {
    xasync::result(block, (&raw const QUOTA_CALL).cast(), out)
}

extern "system" fn delete_async(
    _: This,
    provider: *const Provider,
    name: *const c_char,
    block: *mut AsyncBlock,
) -> HRESULT {
    let hr = match (get(provider), text(name)) {
        (Some(p), Some(name)) if valid_container(&name) => delete(&p.root, &name),
        (Some(_), Some(_)) => INVALID_CONTAINER_NAME,
        _ => E_INVALIDARG,
    };
    xasync::finished(block, (&raw const DELETE).cast(), hr)
}
extern "system" fn delete_result(_: This, block: *mut AsyncBlock) -> HRESULT {
    let none = std::ptr::null_mut();
    xasync::XAsyncGetResult(block, (&raw const DELETE).cast(), 0, none, none.cast())
}

/// Reports the container if it exists; a missing one is no error, the
/// callback just does not run.
extern "system" fn container_info(
    _: This,
    provider: *const Provider,
    name: *const c_char,
    context: *mut c_void,
    callback: Option<ContainerCallback>,
) -> HRESULT {
    let (Some(p), Some(name), Some(callback)) = (get(provider), text(name), callback) else {
        return E_INVALIDARG;
    };
    if !valid_container(&name) {
        return INVALID_CONTAINER_NAME;
    }
    report(&p.root, |n| n == name, context, callback);
    S_OK
}

extern "system" fn enumerate_containers(
    _: This,
    provider: *const Provider,
    context: *mut c_void,
    callback: Option<ContainerCallback>,
) -> HRESULT {
    let (Some(p), Some(callback)) = (get(provider), callback) else {
        return E_INVALIDARG;
    };
    report(&p.root, |_| true, context, callback);
    S_OK
}

/// The container exists once an update to it is submitted.
extern "system" fn create_container(
    _: This,
    provider: *const Provider,
    name: *const c_char,
    out: *mut *mut Container,
) -> HRESULT {
    let (Some(p), Some(name)) = (get(provider), text(name)) else {
        return E_INVALIDARG;
    };
    if !valid_container(&name) {
        return INVALID_CONTAINER_NAME;
    }
    put(out, give(p.container(&name)))
}
extern "system" fn close_container(_: This, container: *mut Container) {
    take(container);
}

extern "system" fn enumerate_blobs(
    _: This,
    container: *const Container,
    prefix: *const c_char,
    context: *mut c_void,
    callback: Option<BlobCallback>,
) -> HRESULT {
    let (Some(c), Some(callback)) = (get(container), callback) else {
        return E_INVALIDARG;
    };
    let prefix = text(prefix).unwrap_or_default();
    let blobs = {
        let _saves = saves_lock();
        let version = current_named(&c.dir, &c.name).ok().flatten();
        version.map(|v| v.sizes()).unwrap_or_default()
    };
    // Sizes first: a container with a file no blob can be reports none.
    let mut infos = Vec::new();
    for (name, size) in blobs.into_iter().filter(|(n, _)| n.starts_with(&prefix)) {
        match blob_size(size) {
            Ok(size) => infos.push((cstring(&name), size)),
            Err(hr) => return hr,
        }
    }
    for (name, size) in &infos {
        let info = BlobInfo {
            name: name.as_ptr(),
            size: *size,
        };
        if !callback(&info, context) {
            break;
        }
    }
    S_OK
}

extern "system" fn read_blobs(
    _: This,
    container: *const Container,
    blob_names: *const *const c_char,
    count: *mut u32,
    size: usize,
    out: *mut u8,
) -> HRESULT {
    // SAFETY: the title passes its count, which is also where the number
    // read goes.
    let (Some(c), Some(&wanted)) = (get(container), unsafe { count.as_ref() }) else {
        return E_INVALIDARG;
    };
    let read = names(blob_names, wanted).and_then(|names| read(c, names));
    let laid_out = read.and_then(|blobs| {
        let read = u32::try_from(blobs.len()).map_err(|_| E_FAIL)?;
        Ok((layout(&blobs, out as usize)?, read))
    });
    let (bytes, read) = match laid_out {
        Ok(done) => done,
        Err(hr) => return hr,
    };
    if out.is_null() || size < bytes.len() {
        return BUFFER_TOO_SMALL;
    }
    // SAFETY: the buffer holds `size` bytes, checked above.
    unsafe { out.copy_from_nonoverlapping(bytes.as_ptr(), bytes.len()) };
    put(count, read)
}

/// Read at once; the result holds the layout with pointers relative to the
/// title's buffer, fixed up when the title takes it.
extern "system" fn read_async(
    _: This,
    container: *const Container,
    blob_names: *const *const c_char,
    count: u32,
    block: *mut AsyncBlock,
) -> HRESULT {
    let result = match get(container) {
        Some(c) => names(blob_names, count)
            .and_then(|names| read(c, names))
            .and_then(|blobs| layout(&blobs, 0)),
        None => Err(E_INVALIDARG),
    };
    xasync::finished_with(block, (&raw const READ).cast(), result)
}
extern "system" fn read_result(
    _: This,
    block: *mut AsyncBlock,
    size: usize,
    out: *mut u8,
    count: *mut u32,
) -> HRESULT {
    // Checked first: taking the result ends the call, and without a place
    // for the count the title could not use what it took.
    if count.is_null() {
        return E_INVALIDARG;
    }
    let mut used = 0;
    let hr = xasync::XAsyncGetResult(block, (&raw const READ).cast(), size, out.cast(), &mut used);
    // The documented error for a small buffer; the result stays for a
    // retry with a larger one.
    if hr == xasync::E_NOT_SUFFICIENT_BUFFER {
        return BUFFER_TOO_SMALL;
    }
    if hr < 0 {
        return hr;
    }
    if used == 0 {
        return put(count, 0);
    }
    // SAFETY: the call just wrote `used` bytes to `out`, having checked
    // that the title's buffer holds as many.
    let bytes = unsafe { std::slice::from_raw_parts_mut(out, used) };
    match rebase(bytes, out as usize) {
        Some(blobs) => put(count, blobs),
        None => E_FAIL,
    }
}

extern "system" fn create_update(
    _: This,
    container: *const Container,
    display: *const c_char,
    out: *mut *mut Update,
) -> HRESULT {
    let Some(c) = get(container) else {
        return E_INVALIDARG;
    };
    put(
        out,
        give(Update {
            name: c.name.clone(),
            dir: c.dir.clone(),
            display: text(display),
            changes: Vec::new(),
        }),
    )
}
extern "system" fn close_update(_: This, update: *mut Update) {
    take(update);
}

extern "system" fn write_blob(
    _: This,
    update: *mut Update,
    name: *const c_char,
    data: *const u8,
    size: usize,
) -> HRESULT {
    // SAFETY: a handle we gave out; the title does not use it concurrently.
    let (Some(update), Some(name)) = (unsafe { update.as_mut() }, text(name)) else {
        return E_INVALIDARG;
    };
    if name.is_empty() || (data.is_null() && size > 0) {
        return E_INVALIDARG;
    }
    if size > MAX_UPDATE {
        return UPDATE_TOO_BIG;
    }
    let data = if size == 0 {
        Vec::new()
    } else {
        // SAFETY: the title passes `size` bytes.
        unsafe { std::slice::from_raw_parts(data, size) }.to_vec()
    };
    update.changes.push((name, Some(data)));
    S_OK
}

extern "system" fn delete_blob(_: This, update: *mut Update, name: *const c_char) -> HRESULT {
    // SAFETY: as in `write_blob`.
    match (unsafe { update.as_mut() }, text(name)) {
        (Some(update), Some(name)) if !name.is_empty() => {
            update.changes.push((name, None));
            S_OK
        }
        _ => E_INVALIDARG,
    }
}

extern "system" fn submit_async(_: This, update: *const Update, block: *mut AsyncBlock) -> HRESULT {
    let hr = get(update).map_or(E_INVALIDARG, Update::submit);
    xasync::finished(block, (&raw const SUBMIT).cast(), hr)
}
extern "system" fn submit_result(_: This, block: *mut AsyncBlock) -> HRESULT {
    let none = std::ptr::null_mut();
    xasync::XAsyncGetResult(block, (&raw const SUBMIT).cast(), 0, none, none.cast())
}

static VTABLE: Methods<30> = Methods([
    missing::<0> as *const (),
    add_ref as *const (),
    add_ref as *const (),
    missing::<3> as *const (),         // 0x18 initialize (blocking)
    initialize_async as *const (),     // 0x20
    initialize_result as *const (),    // 0x28
    close_provider as *const (),       // 0x30
    missing::<7> as *const (),         // 0x38 remaining quota (blocking)
    quota_async as *const (),          // 0x40
    quota_result as *const (),         // 0x48
    missing::<10> as *const (),        // 0x50 delete (blocking)
    delete_async as *const (),         // 0x58
    delete_result as *const (),        // 0x60
    container_info as *const (),       // 0x68
    enumerate_containers as *const (), // 0x70
    missing::<15> as *const (),        // 0x78 by name prefix
    create_container as *const (),     // 0x80
    close_container as *const (),      // 0x88
    missing::<18> as *const (),        // 0x90 all blob info
    enumerate_blobs as *const (),      // 0x98 by name prefix
    read_blobs as *const (),           // 0xa0
    read_async as *const (),           // 0xa8
    read_result as *const (),          // 0xb0
    create_update as *const (),        // 0xb8
    close_update as *const (),         // 0xc0
    write_blob as *const (),           // 0xc8
    delete_blob as *const (),          // 0xd0
    missing::<27> as *const (),        // 0xd8 submit (blocking)
    submit_async as *const (),         // 0xe0
    submit_result as *const (),        // 0xe8
]);

#[cfg(test)]
mod tests {
    use std::io::BufRead;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::modules::xgameruntime::method;

    fn block() -> AsyncBlock {
        AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        }
    }

    static STEPS: AtomicU32 = AtomicU32::new(0);

    /// In a child process of the crash test, stops for good at the step
    /// `VICTORY_SAVE_STOP` names and says so.
    pub(super) fn crash_point() {
        let Ok(stop) = std::env::var("VICTORY_SAVE_STOP") else {
            return;
        };
        if stop.parse() == Ok(STEPS.fetch_add(1, Ordering::SeqCst) + 1) {
            println!("STOPPED");
            let _ = std::io::stdout().flush();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
        }
    }

    /// The per-user quota and the host disk fail with their own errors.
    #[test]
    fn a_full_disk_is_not_the_quota() {
        let mb = 1 << 20;
        assert_eq!(no_room(2 * mb, 0, 0, mb), Some(OUT_OF_LOCAL_STORAGE));
        assert_eq!(
            no_room(2 * mb, 0, QUOTA - mb, 100 * mb),
            Some(QUOTA_EXCEEDED)
        );
        // Replacing a blob of the same size fits a full quota.
        assert_eq!(no_room(mb, mb, QUOTA, 100 * mb), None);
        assert_eq!(no_room(mb, 0, 0, mb), None);
    }

    fn update(root: &Path, display: &str, changes: &[(&str, Option<&str>)]) -> HRESULT {
        Update {
            name: "Slot".into(),
            dir: container_dir(root, "Slot"),
            display: Some(display.into()),
            changes: changes
                .iter()
                .map(|(n, d)| (n.to_string(), d.map(|d| d.as_bytes().to_vec())))
                .collect(),
        }
        .submit()
    }

    /// The container `Slot`: display name and blobs.
    fn state(root: &Path) -> Option<(String, Vec<(String, String)>)> {
        let c = Provider { root: root.into() }.container("Slot");
        let display = current_named(&c.dir, "Slot").unwrap()?.display;
        let blobs = read(&c, None).unwrap();
        let text = |(n, d): (String, Vec<u8>)| (n, String::from_utf8(d).unwrap());
        Some((display, blobs.into_iter().map(text).collect()))
    }

    fn old_state() -> Option<(String, Vec<(String, String)>)> {
        Some((
            "one".into(),
            vec![("a".into(), "old a".into()), ("b".into(), "old b".into())],
        ))
    }

    /// Run by the crash test in a child process; does nothing otherwise.
    #[test]
    fn crash_child() {
        let (Ok(root), Ok(op)) = (
            std::env::var("VICTORY_SAVE_CHILD"),
            std::env::var("VICTORY_SAVE_OP"),
        ) else {
            return;
        };
        let root = PathBuf::from(root);
        let hr = if op == "update" {
            update(
                &root,
                "two",
                &[("a", Some("new a")), ("b", None), ("c", Some("new c"))],
            )
        } else {
            delete(&root, "Slot")
        };
        assert_eq!(hr, S_OK);
        println!("FINISHED");
    }

    /// Kills a process in the middle of an update (and of a deletion) at
    /// every step: what is left is the whole old container or the whole
    /// new one.
    #[test]
    fn a_kill_at_any_step_leaves_old_or_new() {
        let new = Some((
            "two".into(),
            vec![("a".into(), "new a".into()), ("c".into(), "new c".into())],
        ));
        for (op, after) in [("update", new), ("delete", None)] {
            let mut seen = (0, 0);
            for stop in 1.. {
                let dir = tempfile::tempdir().unwrap();
                let root = dir.path();
                assert_eq!(
                    update(root, "one", &[("a", Some("old a")), ("b", Some("old b"))]),
                    S_OK
                );
                let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "modules::xgamesave::tests::crash_child",
                        "--nocapture",
                    ])
                    .args(["--test-threads=1"])
                    .env("VICTORY_SAVE_CHILD", root)
                    .env("VICTORY_SAVE_OP", op)
                    .env("VICTORY_SAVE_STOP", stop.to_string())
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .unwrap();
                let out = std::io::BufReader::new(child.stdout.take().unwrap());
                let mut finished = false;
                for line in out.lines() {
                    let line = line.unwrap();
                    if line.contains("STOPPED") {
                        break;
                    }
                    finished |= line.contains("FINISHED");
                }
                let _ = child.kill();
                child.wait().unwrap();
                let left = state(root);
                if left == old_state() {
                    seen.0 += 1;
                } else {
                    assert_eq!(left, after, "{op}, killed at step {stop}");
                    seen.1 += 1;
                }
                if finished {
                    break;
                }
            }
            assert!(seen.0 > 0 && seen.1 > 1, "{op}: {seen:?}");
        }
    }

    #[test]
    fn names_are_exact() {
        for name in ["Save_1", "slot/2", "a.b c%", "\u{e9}t\u{e9}"] {
            let file = file_name(name);
            assert!(!file.contains(['.', '/', ' ']), "{file}");
            assert_eq!(decode(&file).as_deref(), Some(name));
        }
        assert_eq!(decode("x.new"), None);
        for good in ["Profile/Slot_1", "a/b/Save-2.sav", "x", &"a".repeat(256)] {
            assert!(valid_container(good), "{good}");
        }
        for bad in [
            "a b",
            "",
            "a/",
            "a.",
            "a..b",
            "a.b/c",
            "a-b/c",
            &"a".repeat(257),
        ] {
            assert!(!valid_container(bad), "{bad}");
        }

        // Containers and blobs differing only in case, reserved device
        // names, characters the host forbids, long names: all distinct.
        let dir = tempfile::tempdir().unwrap();
        let p = Provider {
            root: dir.path().into(),
        };
        let long = "n".repeat(300);
        let blobs = [
            "A",
            "a",
            "CON",
            "nul.txt",
            "x/y:z*?\"<>|",
            long.as_str(),
            "end. ",
        ];
        for container in ["Save", "save", "SAVE/CON.x"] {
            let c = p.container(container);
            let u = Update {
                name: c.name.clone(),
                dir: c.dir.clone(),
                display: None,
                changes: blobs
                    .iter()
                    .map(|b| (b.to_string(), Some(format!("{container}:{b}").into_bytes())))
                    .collect(),
            };
            assert_eq!(u.submit(), S_OK, "{container}");
        }
        let _saves = saves_lock();
        let names: Vec<String> = containers(dir.path()).into_iter().map(|c| c.0).collect();
        drop(_saves);
        assert_eq!(names, ["SAVE/CON.x", "Save", "save"]);
        for container in ["Save", "save", "SAVE/CON.x"] {
            let read = read(&p.container(container), None).unwrap();
            let mut expected: Vec<_> = blobs
                .iter()
                .map(|b| (b.to_string(), format!("{container}:{b}").into_bytes()))
                .collect();
            expected.sort();
            assert_eq!(read, expected, "{container}");
        }
    }

    #[test]
    fn the_old_layout_moves_over() {
        let saves = tempfile::tempdir().unwrap();
        let root = saves
            .path()
            .join(format!("{:016X}", xuser::XUID))
            .join("scid");
        let old = root.join("Player");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("Save%20Game%20Data"), b"data").unwrap();
        std::fs::write(old.join("Save%20Game%20Header"), b"header").unwrap();
        std::fs::write(old.join("Save%20Game%20Data.new"), b"half").unwrap();
        std::fs::write(old.join(".display"), b"Chapter 1").unwrap();
        let p = Provider::open(saves.path(), Some("scid")).unwrap();
        assert!(!old.exists());
        let c = p.container("Player");
        let blobs = read(&c, None).unwrap();
        assert_eq!(
            blobs,
            [
                ("Save Game Data".into(), b"data".to_vec()),
                ("Save Game Header".into(), b"header".to_vec())
            ]
        );
        let v = current_named(&c.dir, "Player").unwrap().unwrap();
        assert_eq!((v.number, v.display.as_str()), (1, "Chapter 1"));
    }

    /// An empty configuration id names none. As a folder name it would be
    /// the folder every configuration lives in, whose folders then look
    /// like containers of the old layout and are converted away.
    #[test]
    fn an_empty_configuration_id_is_the_default_one() {
        let saves = tempfile::tempdir().unwrap();
        let scid = Provider::open(saves.path(), Some("scid")).unwrap();
        assert_eq!(update(&scid.root, "one", &[("a", Some("kept"))]), S_OK);
        let kept = Some(("one".into(), vec![("a".into(), "kept".into())]));
        assert_eq!(state(&scid.root), kept);

        let empty = Provider::open(saves.path(), Some("")).unwrap();
        assert!(scid.root.is_dir(), "another configuration's saves are gone");
        assert_eq!(state(&scid.root), kept);
        let default = Provider::open(saves.path(), None).unwrap();
        assert_eq!(empty.root, default.root);

        // Named configurations stay apart, from it and from each other.
        assert_ne!(empty.root, scid.root);
        assert_eq!(state(&empty.root), None);
        let other = Provider::open(saves.path(), Some("other")).unwrap();
        assert_eq!(state(&other.root), None);
        assert_eq!(state(&scid.root), kept);
    }

    #[test]
    fn saves_persist_across_providers() {
        let this: This = &OBJECT;
        let saves = tempfile::tempdir().unwrap();
        let provider = give(Provider::open(saves.path(), Some("scid")).unwrap());

        // Write two blobs through an update.
        let mut container = std::ptr::null_mut();
        type Create = extern "system" fn(
            This,
            *const Provider,
            *const c_char,
            *mut *mut Container,
        ) -> HRESULT;
        let create = method::<Create>(&OBJECT, 0x80);
        assert_eq!(
            create(this, provider, c"bad name".as_ptr(), &mut container),
            INVALID_CONTAINER_NAME
        );
        assert_eq!(
            create(this, provider, c"Slot_1".as_ptr(), &mut container),
            S_OK
        );
        let mut update = std::ptr::null_mut();
        type Update_ =
            extern "system" fn(This, *const Container, *const c_char, *mut *mut Update) -> HRESULT;
        assert_eq!(
            method::<Update_>(&OBJECT, 0xb8)(this, container, c"Chapter 1".as_ptr(), &mut update),
            S_OK
        );
        type Write =
            extern "system" fn(This, *mut Update, *const c_char, *const u8, usize) -> HRESULT;
        let write = method::<Write>(&OBJECT, 0xc8);
        assert_eq!(
            write(this, update, c"world".as_ptr(), b"hello".as_ptr(), 5),
            S_OK
        );
        assert_eq!(
            write(this, update, c"player.dat".as_ptr(), b"xy".as_ptr(), 2),
            S_OK
        );
        assert_eq!(
            write(this, update, c"gone".as_ptr(), b"z".as_ptr(), 1),
            S_OK
        );
        type Delete = extern "system" fn(This, *mut Update, *const c_char) -> HRESULT;
        assert_eq!(
            method::<Delete>(&OBJECT, 0xd0)(this, update, c"gone".as_ptr()),
            S_OK
        );
        type Submit = extern "system" fn(This, *const Update, *mut AsyncBlock) -> HRESULT;
        let mut b = block();
        assert_eq!(method::<Submit>(&OBJECT, 0xe0)(this, update, &mut b), S_OK);
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), S_OK);
        close_update(this, update);
        close_container(this, container);
        close_provider(this, provider);

        // A new provider sees the container and reads both blobs back.
        let provider = give(Provider::open(saves.path(), Some("scid")).unwrap());
        extern "system" fn seen(info: *const ContainerInfo, context: *mut c_void) -> bool {
            // SAFETY: valid during the callback; the context is the test's vector.
            let (info, out) = unsafe {
                (
                    &*info,
                    &mut *context.cast::<Vec<(String, String, u32, u64)>>(),
                )
            };
            out.push((
                text(info.name).unwrap(),
                text(info.display_name).unwrap(),
                info.blob_count,
                info.total_size,
            ));
            true
        }
        let mut found: Vec<(String, String, u32, u64)> = Vec::new();
        let context = (&raw mut found).cast();
        assert_eq!(
            enumerate_containers(this, provider, context, Some(seen)),
            S_OK
        );
        assert_eq!(found, [("Slot_1".into(), "Chapter 1".into(), 2, 7)]);

        let mut container = std::ptr::null_mut();
        assert_eq!(
            create_container(this, provider, c"Slot_1".as_ptr(), &mut container),
            S_OK
        );
        let mut b = block();
        let wanted = [c"world".as_ptr(), c"player.dat".as_ptr()];
        assert_eq!(
            read_async(this, container, wanted.as_ptr(), 2, &mut b),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), S_OK);
        let mut size = 0;
        assert_eq!(xasync::XAsyncGetResultSize(&mut b, &mut size), S_OK);
        assert_eq!(
            size,
            2 * BLOB + "world\0hello".len() + "player.dat\0xy".len()
        );
        let mut buffer = vec![0u64; size.div_ceil(8)];
        let mut count = 0;
        assert_eq!(
            read_result(this, &mut b, 8, buffer.as_mut_ptr().cast(), &mut count),
            BUFFER_TOO_SMALL
        );
        assert_eq!(
            read_result(this, &mut b, size, buffer.as_mut_ptr().cast(), &mut count),
            S_OK
        );
        assert_eq!(count, 2);
        let blob = |i: usize| {
            // SAFETY: the layout the call wrote.
            unsafe {
                let entry = buffer.as_ptr().cast::<u8>().add(i * BLOB);
                let name = text(entry.cast::<*const c_char>().read_unaligned()).unwrap();
                let len = entry.add(8).cast::<u32>().read_unaligned() as usize;
                let data = std::slice::from_raw_parts(
                    entry.add(16).cast::<*const u8>().read_unaligned(),
                    len,
                );
                (name, data.to_vec())
            }
        };
        assert_eq!(blob(0), ("world".into(), b"hello".to_vec()));
        assert_eq!(blob(1), ("player.dat".into(), b"xy".to_vec()));

        let missing = [c"nothing".as_ptr()];
        let mut one = 1;
        let mut small = [0u8; 8];
        let read_now = method::<
            extern "system" fn(
                This,
                *const Container,
                *const *const c_char,
                *mut u32,
                usize,
                *mut u8,
            ) -> HRESULT,
        >(&OBJECT, 0xa0);
        assert_eq!(
            read_now(
                this,
                container,
                missing.as_ptr(),
                &mut one,
                8,
                small.as_mut_ptr()
            ),
            BLOB_NOT_FOUND
        );
        assert_eq!(
            read_now(
                this,
                container,
                wanted.as_ptr(),
                &mut 2,
                8,
                small.as_mut_ptr()
            ),
            BUFFER_TOO_SMALL
        );

        // Quota counts the blobs; deleting the container frees it.
        let mut b = block();
        assert_eq!(quota_async(this, provider, &mut b), S_OK);
        let mut left = 0i64;
        assert_eq!(quota_result(this, &mut b, &mut left), S_OK);
        assert!(left as u64 <= QUOTA - 7);
        let mut b = block();
        assert_eq!(
            delete_async(this, provider, c"Slot_1".as_ptr(), &mut b),
            S_OK
        );
        assert_eq!(delete_result(this, &mut b), S_OK);
        found.clear();
        let context = (&raw mut found).cast();
        assert_eq!(
            enumerate_containers(this, provider, context, Some(seen)),
            S_OK
        );
        assert!(found.is_empty());
        close_container(this, container);
        close_provider(this, provider);
    }

    /// The manifest is length-prefixed strings with no count of its own:
    /// anything that is not whole strings, at least the two names, is no
    /// manifest.
    #[test]
    fn parse_manifest_rejects_corrupt_input() {
        let strings = ["Slot", "Chapter \u{e9}", "a", "", "b"];
        let good = manifest(strings[0], strings[1], &strings[2..]);
        assert_eq!(
            parse_manifest(&good).as_deref(),
            Some(&strings.map(String::from)[..])
        );

        // Cut anywhere: whole strings and both names, or nothing.
        let mut boundaries = vec![0];
        for s in strings {
            boundaries.push(boundaries.last().unwrap() + 4 + s.len());
        }
        for cut in 0..good.len() {
            let whole = boundaries.iter().position(|b| *b == cut);
            let expected = whole.filter(|n| *n >= 2).map(|n| strings[..n].to_vec());
            let parsed = parse_manifest(&good[..cut]);
            let parsed = parsed
                .as_ref()
                .map(|p| p.iter().map(String::as_str).collect());
            assert_eq!(parsed, expected, "cut at {cut}");
        }

        let prefixed = |len: u32, bytes: &[u8]| [&len.to_le_bytes()[..], bytes].concat();
        let name = prefixed(4, b"Slot");
        for bad in [
            Vec::new(),
            name.clone(),                                          // one string
            [&name[..], &[1, 0, 0][..]].concat(),                  // half a length
            [&name[..], &prefixed(u32::MAX, b"one")[..]].concat(), // length past the end
            [&name[..], &prefixed(4, b"one")[..]].concat(),
            [&name[..], &prefixed(2, b"\xff\xfe")[..]].concat(), // not UTF-8
            [&prefixed(1, b"\x80")[..], &name[..]].concat(),
            vec![0xff; 64],
        ] {
            assert_eq!(parse_manifest(&bad), None, "{bad:?}");
        }
    }

    fn other(root: &Path) -> Container {
        Provider { root: root.into() }.container("Other")
    }

    /// A container whose manifest is damaged is not listed, counted or
    /// read, cannot be written over, and takes nothing else with it; its
    /// files stay as they are.
    #[test]
    fn a_corrupt_manifest_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(update(root, "one", &[("a", Some("kept"))]), S_OK);
        let o = other(root);
        let u = Update {
            name: o.name.clone(),
            dir: o.dir.clone(),
            display: None,
            changes: vec![("x".into(), Some(b"other".to_vec()))],
        };
        assert_eq!(u.submit(), S_OK);
        let listed = |root: &Path| -> Vec<String> {
            let _saves = saves_lock();
            containers(root).into_iter().map(|c| c.0).collect()
        };
        assert_eq!(listed(root), ["Other", "Slot"]);
        assert_eq!(used_bytes(root), 9);

        let slot = Provider { root: root.into() }.container("Slot");
        let file = slot.dir.join("v1").join(MANIFEST);
        let good = std::fs::read(&file).unwrap();
        for bad in [
            &b""[..],
            &good[..good.len() - 1],
            &good[..5],
            b"\x04\0\0\0Slot",
            b"\x04\0\0\0Slot\x02\0\0\0\xff\xfe",
            b"\xff\xff\xff\xff",
        ] {
            std::fs::write(&file, bad).unwrap();
            assert_eq!(listed(root), ["Other"], "{bad:?}");
            assert_eq!(used_bytes(root), 5, "{bad:?}");
            assert_eq!(read(&slot, None), Err(NO_ACCESS), "{bad:?}");
            assert_eq!(update(root, "two", &[("a", Some("new"))]), NO_ACCESS);
            assert_eq!(delete(root, "Slot"), NO_ACCESS, "{bad:?}");
            assert_eq!(
                std::fs::read(slot.dir.join("v1").join("0")).unwrap(),
                b"kept"
            );
            assert_eq!(read(&o, None).unwrap(), [("x".into(), b"other".to_vec())]);
        }

        // Without its manifest a version is no container at all.
        std::fs::remove_file(&file).unwrap();
        assert_eq!(listed(root), ["Other"]);
        assert_eq!(read(&slot, None), Err(BLOB_NOT_FOUND));
        assert_eq!(read(&o, None).unwrap(), [("x".into(), b"other".to_vec())]);

        // Nothing was lost: with the manifest back, the container is.
        std::fs::write(&file, &good).unwrap();
        let kept = Some(("one".into(), vec![("a".into(), "kept".into())]));
        assert_eq!(state(root), kept);
        assert_eq!(listed(root), ["Other", "Slot"]);
    }

    /// One blob may hold what one update may, `GS_MAX_BLOB_SIZE` (16 MB),
    /// and the error past either is `E_GS_UPDATE_TOO_BIG`, as the public
    /// documentation has it. A blob past the limit is refused when it is
    /// written, before it is copied; which of the two calls reports it on
    /// the console is not documented.
    #[test]
    fn an_oversized_blob_is_refused() {
        let this: This = &OBJECT;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let slot = Provider { root: root.into() }.container("Slot");
        let new_update = || Update {
            name: slot.name.clone(),
            dir: slot.dir.clone(),
            display: None,
            changes: Vec::new(),
        };
        let data = vec![7u8; MAX_UPDATE + 1];

        let mut u = new_update();
        let write = |u: &mut Update, name: &std::ffi::CStr, size: usize| {
            write_blob(this, u, name.as_ptr(), data.as_ptr(), size)
        };
        assert_eq!(write(&mut u, c"big", MAX_UPDATE + 1), UPDATE_TOO_BIG);
        assert_eq!(write(&mut u, c"big", usize::MAX), UPDATE_TOO_BIG);
        assert!(
            u.changes.is_empty(),
            "a refused blob is not part of the update"
        );

        // Blobs that each fit but not together: the update is refused as a
        // whole and nothing of it is kept.
        assert_eq!(write(&mut u, c"big", MAX_UPDATE), S_OK);
        assert_eq!(write(&mut u, c"small", 1), S_OK);
        assert_eq!(u.submit(), UPDATE_TOO_BIG);
        assert_eq!(read(&slot, None), Err(BLOB_NOT_FOUND));
        assert_eq!(used_bytes(root), 0);

        // At the limit it is written, and read back whole.
        let mut u = new_update();
        assert_eq!(write(&mut u, c"big", MAX_UPDATE), S_OK);
        assert_eq!(u.submit(), S_OK);
        let blobs = read(&slot, None).unwrap();
        assert_eq!(blobs.len(), 1);
        assert_eq!(blobs[0].0, "big");
        assert!(blobs[0].1 == data[..MAX_UPDATE], "the blob's bytes differ");
    }

    /// The public API states a blob's size in 32 bits; a size that does
    /// not fit is an error, not its low 32 bits.
    #[test]
    fn a_size_past_32_bits_is_not_cut_short() {
        assert_eq!(blob_size(0), Ok(0));
        assert_eq!(blob_size(u64::from(u32::MAX)), Ok(u32::MAX));
        assert_eq!(blob_size(u64::from(u32::MAX) + 1), Err(NO_ACCESS));
        assert_eq!(blob_size(u64::MAX), Err(NO_ACCESS));
    }

    /// A blob file of more than 32 bits (nothing we write; made here as a
    /// sparse file, which takes no room on the disk) is not loaded and
    /// not cut short: reading it fails before the file is read, and the
    /// container lists no blobs once one of those asked for is such a
    /// file. Blobs not asked for are not looked at.
    #[test]
    fn a_blob_file_past_32_bits_is_neither_read_nor_listed() {
        extern "system" fn listed(info: *const BlobInfo, context: *mut c_void) -> bool {
            // SAFETY: valid during the callback; the context is the test's
            // vector.
            let (info, out) = unsafe { (&*info, &mut *context.cast::<Vec<(String, u32)>>()) };
            out.push((text(info.name).unwrap(), info.size));
            true
        }
        fn list(
            slot: &Container,
            prefix: Option<&std::ffi::CStr>,
        ) -> (HRESULT, Vec<(String, u32)>) {
            let mut found: Vec<(String, u32)> = Vec::new();
            let prefix = prefix.map_or(std::ptr::null(), |p| p.as_ptr());
            let context = (&raw mut found).cast();
            let hr = enumerate_blobs(&OBJECT, slot, prefix, context, Some(listed));
            (hr, found)
        }
        let this: This = &OBJECT;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(
            update(root, "one", &[("a", Some("alpha")), ("b", Some("beta"))]),
            S_OK
        );
        let slot = Provider { root: root.into() }.container("Slot");
        let both = vec![("a".to_owned(), 5), ("b".to_owned(), 4)];
        assert_eq!(list(&slot, None), (S_OK, both.clone()));

        let file = slot.dir.join("v1").join("0");
        assert_eq!(std::fs::read(&file).unwrap(), b"alpha");
        let resize = |len: u64| {
            let blob = std::fs::OpenOptions::new().write(true).open(&file).unwrap();
            blob.set_len(len).unwrap();
            assert_eq!(std::fs::metadata(&file).unwrap().len(), len);
        };
        resize(u64::from(u32::MAX) + 1);

        // What a read gave, by size: a failed assertion must not print a
        // blob of that length.
        let sizes = |names: Option<&[&str]>| {
            let names = names.map(|names| names.iter().map(|n| n.to_string()).collect());
            let size = |(name, data): (String, Vec<u8>)| (name, data.len());
            read(&slot, names).map(|blobs| blobs.into_iter().map(size).collect::<Vec<_>>())
        };
        assert_eq!(sizes(None), Err(NO_ACCESS));
        assert_eq!(sizes(Some(&["a"])), Err(NO_ACCESS));
        assert_eq!(sizes(Some(&["b", "a"])), Err(NO_ACCESS));
        assert_eq!(sizes(Some(&["b"])), Ok(vec![("b".to_owned(), 4)]));
        // Through the API: nothing is written and the count stays.
        let mut buffer = vec![0u64; 32];
        let (out, size) = (buffer.as_mut_ptr().cast::<u8>(), 32 * 8);
        let mut count = 7;
        assert_eq!(
            read_blobs(this, &slot, std::ptr::null(), &mut count, size, out),
            NO_ACCESS
        );
        assert_eq!(count, 7);
        assert!(
            buffer.iter().all(|w| *w == 0),
            "a refused read wrote something"
        );
        let mut block = block();
        assert_eq!(
            read_async(this, &slot, std::ptr::null(), 0, &mut block),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut block, true), NO_ACCESS);

        assert_eq!(list(&slot, None), (NO_ACCESS, Vec::new()));
        assert_eq!(list(&slot, Some(c"a")), (NO_ACCESS, Vec::new()));
        assert_eq!(list(&slot, Some(c"b")), (S_OK, vec![both[1].clone()]));

        // One byte less fits 32 bits: the size is reported as it is.
        resize(u64::from(u32::MAX));
        let longest = vec![("a".to_owned(), u32::MAX), both[1].clone()];
        assert_eq!(list(&slot, None), (S_OK, longest));

        // Nothing else was wrong with the container.
        resize(5);
        assert_eq!(list(&slot, None), (S_OK, both));
        let kept = vec![("a".into(), "alpha".into()), ("b".into(), "beta".into())];
        assert_eq!(state(root), Some(("one".into(), kept)));
    }

    /// Blob `i` of a result in `buffer`: its name and data, through the
    /// pointers the call left there.
    fn blob_at(buffer: &[u64], i: usize) -> (String, Vec<u8>) {
        let range = buffer.as_ptr_range();
        let inside =
            |p: usize, len: usize| p >= range.start as usize && p + len <= range.end as usize;
        // SAFETY: the layout the call wrote; every pointer is checked to
        // lie in the buffer before it is followed.
        unsafe {
            let entry = buffer.as_ptr().cast::<u8>().add(i * BLOB);
            let name = entry.cast::<*const c_char>().read_unaligned();
            let len = entry.add(8).cast::<u32>().read_unaligned() as usize;
            let data = entry.add(16).cast::<*const u8>().read_unaligned();
            assert!(inside(name as usize, 1) && inside(data as usize, len));
            (
                text(name).unwrap(),
                std::slice::from_raw_parts(data, len).to_vec(),
            )
        }
    }

    #[test]
    fn read_result_with_a_small_buffer_keeps_the_result() {
        let this: This = &OBJECT;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(
            update(root, "one", &[("a", Some("alpha")), ("b", Some(""))]),
            S_OK
        );
        let slot = Provider { root: root.into() }.container("Slot");

        let mut b = block();
        assert_eq!(read_async(this, &slot, std::ptr::null(), 0, &mut b), S_OK);
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), S_OK);
        let mut size = 0;
        assert_eq!(xasync::XAsyncGetResultSize(&mut b, &mut size), S_OK);
        assert_eq!(size, 2 * BLOB + "a\0alpha".len() + "b\0".len());

        let mut buffer = vec![0u64; size.div_ceil(8)];
        let out = buffer.as_mut_ptr().cast::<u8>();
        let mut count = 99;
        // Too small, in every way a title can get it wrong, any number of
        // times: the documented error, and nothing is written or lost.
        for _ in 0..2 {
            for small in [0, 1, BLOB, size - 1] {
                assert_eq!(
                    read_result(this, &mut b, small, out, &mut count),
                    BUFFER_TOO_SMALL
                );
            }
            let null = std::ptr::null_mut();
            assert_eq!(
                read_result(this, &mut b, size, null, &mut count),
                BUFFER_TOO_SMALL
            );
            // Nowhere to put the count: refused before the result is taken.
            assert_eq!(
                read_result(this, &mut b, size, out, null.cast()),
                E_INVALIDARG
            );
        }
        assert_eq!(count, 99);
        assert!(
            buffer.iter().all(|w| *w == 0),
            "a refused read wrote something"
        );

        assert_eq!(read_result(this, &mut b, size, out, &mut count), S_OK);
        assert_eq!(count, 2);
        assert_eq!(blob_at(&buffer, 0), ("a".into(), b"alpha".to_vec()));
        assert_eq!(blob_at(&buffer, 1), ("b".into(), Vec::new()));

        // Taken once: the call is over.
        let mut again = 99;
        assert_eq!(
            read_result(this, &mut b, size, out, &mut again),
            xasync::E_ILLEGAL_METHOD_CALL
        );
        assert_eq!(again, 99);

        // A container without blobs reads as none, with no buffer at all.
        let mut u = Update {
            name: "Empty".into(),
            dir: container_dir(root, "Empty"),
            display: None,
            changes: Vec::new(),
        };
        assert_eq!(u.submit(), S_OK);
        u.changes.clear();
        let empty = Provider { root: root.into() }.container("Empty");
        let mut b = block();
        assert_eq!(read_async(this, &empty, std::ptr::null(), 0, &mut b), S_OK);
        assert_eq!(
            read_result(this, &mut b, 0, std::ptr::null_mut(), &mut count),
            S_OK
        );
        assert_eq!(count, 0);
    }

    /// The fix-up of a read result only touches bytes that are the layout
    /// we build, whatever the buffer holds.
    #[test]
    fn a_result_that_is_no_blob_layout_is_refused() {
        let blobs = [
            ("a".to_owned(), b"alpha".to_vec()),
            ("name".to_owned(), Vec::new()),
            ("z".to_owned(), vec![0xff; 40]),
        ];
        let good = layout(&blobs, 0).unwrap();
        let base = 0x1_0000_0000;
        let mut moved = good.clone();
        assert_eq!(rebase(&mut moved, base), Some(3));
        assert_eq!(moved, layout(&blobs, base).unwrap());
        assert_eq!(rebase(&mut [], base), Some(0));
        assert_eq!(layout(&[], base).unwrap(), Vec::<u8>::new());

        let refused = |bytes: &[u8]| {
            let mut copy = bytes.to_vec();
            let count = rebase(&mut copy, base);
            assert!(count.is_some() || copy == bytes, "refused, yet changed");
            count.is_none()
        };
        // Cut short anywhere.
        for cut in 1..good.len() {
            assert!(refused(&good[..cut]), "cut at {cut}");
        }
        // Any one of its words (blob array offsets, sizes) set wrong.
        let with = |at: usize, value: u64| {
            let mut bytes = good.clone();
            bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
            bytes
        };
        let len = good.len() as u64;
        for entry in 0..blobs.len() {
            for field in [0, 16] {
                for value in [0, 1, len, len + 1, u64::MAX, u64::MAX - 7] {
                    let bytes = with(entry * BLOB + field, value);
                    // No bytes at the very end are an empty blob's data.
                    let empty_at_end = field == 16 && value == len && blobs[entry].1.is_empty();
                    assert_eq!(
                        refused(&bytes),
                        !empty_at_end,
                        "blob {entry} field {field} = {value:#x}"
                    );
                }
            }
            let mut bytes = good.clone();
            bytes[entry * BLOB + 8..entry * BLOB + 12].copy_from_slice(&u32::MAX.to_le_bytes());
            assert!(refused(&bytes), "blob {entry} size");
        }
        // And bytes that were never a layout: seeded noise, same every run.
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        for _ in 0..2000 {
            let mut noise = vec![0u8; (state % 200) as usize];
            for byte in &mut noise {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                // Small values, so that offsets often land inside.
                *byte = if state & 3 == 0 {
                    (state >> 8) as u8
                } else {
                    0
                };
            }
            let mut copy = noise.clone();
            match rebase(&mut copy, base) {
                None => assert_eq!(copy, noise),
                Some(count) => assert!(count as usize * BLOB <= noise.len()),
            }
        }
    }

    /// A null string among the blob names, or no place for the count, is
    /// an invalid argument, not a blob that was not found or a read whose
    /// count is lost.
    #[test]
    fn blob_names_and_counts_are_checked() {
        let this: This = &OBJECT;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(
            update(root, "one", &[("a", Some("alpha")), ("b", Some("beta"))]),
            S_OK
        );
        let slot = Provider { root: root.into() }.container("Slot");

        let wanted = [c"b".as_ptr(), std::ptr::null(), c"a".as_ptr()];
        assert_eq!(names(wanted.as_ptr(), 1), Ok(Some(vec!["b".into()])));
        assert_eq!(names(wanted.as_ptr(), 0), Ok(Some(Vec::new())));
        assert_eq!(names(wanted.as_ptr(), 2), Err(E_INVALIDARG));
        assert_eq!(names(wanted.as_ptr(), 3), Err(E_INVALIDARG));
        assert_eq!(names(std::ptr::null(), 3), Ok(None));

        let mut buffer = vec![0u64; 32];
        let (out, size) = (buffer.as_mut_ptr().cast::<u8>(), 32 * 8);
        let mut count = 3;
        assert_eq!(
            read_blobs(this, &slot, wanted.as_ptr(), &mut count, size, out),
            E_INVALIDARG
        );
        let none = std::ptr::null_mut();
        assert_eq!(
            read_blobs(this, &slot, wanted.as_ptr(), none, size, out),
            E_INVALIDARG
        );
        assert_eq!(
            read_blobs(this, &slot, std::ptr::null(), none, size, out),
            E_INVALIDARG
        );
        assert_eq!(count, 3);
        assert!(
            buffer.iter().all(|w| *w == 0),
            "a refused read wrote something"
        );

        let mut b = block();
        assert_eq!(read_async(this, &slot, wanted.as_ptr(), 3, &mut b), S_OK);
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), E_INVALIDARG);
        assert_eq!(
            read_result(this, &mut b, size, out, &mut count),
            E_INVALIDARG
        );

        // The names it does give are read in its order.
        count = 1;
        assert_eq!(
            read_blobs(this, &slot, wanted.as_ptr(), &mut count, size, out),
            S_OK
        );
        assert_eq!(count, 1);
        assert_eq!(blob_at(&buffer, 0), ("b".into(), b"beta".to_vec()));
        // And all of them for no names, whatever the count said.
        count = 7;
        assert_eq!(
            read_blobs(this, &slot, std::ptr::null(), &mut count, size, out),
            S_OK
        );
        assert_eq!(count, 2);
        assert_eq!(blob_at(&buffer, 0), ("a".into(), b"alpha".to_vec()));
        assert_eq!(blob_at(&buffer, 1), ("b".into(), b"beta".to_vec()));
    }
}
