//! Compiled host pipelines kept on disk, and the title pipelines worth
//! starting early (decision 0007, "Pipeline cache").
//!
//! 1. Every host pipeline goes into the host's pipeline library, named by
//!    a hash of all that its host description holds (the translated
//!    shaders' code, which changes when a translation does, our root
//!    signature, the graphics state). The library is saved under the cache
//!    folder; the host refuses a library of another adapter or driver, and
//!    an entry whose description differs, so neither is ever used.
//! 2. The title pipelines a run drew or dispatched with are listed by a key
//!    known when the title creates them (their shader containers and
//!    graphics state); the next run hands those to the workers at
//!    creation, during loading.
//!
//! All library calls are serialised: the interface does not promise that
//! stores, loads and serialising may overlap.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{HSTRING, Interface, PCWSTR};

static DIR: OnceLock<PathBuf> = OnceLock::new();

/// Keeps the pipeline library and the early list in `dir` from now on.
pub fn use_pipeline_cache(dir: PathBuf) {
    let _ = DIR.set(dir);
}

/// Host pipelines created, loaded from the library, and title pipelines
/// started at creation, since the start.
pub(crate) static MADE: AtomicU64 = AtomicU64::new(0);
pub(crate) static LOADED: AtomicU64 = AtomicU64::new(0);
pub(crate) static EARLY: AtomicU64 = AtomicU64::new(0);

struct Library {
    library: ID3D12PipelineLibrary1,
    /// The blob the library was made from: it must outlive the library.
    _blob: Vec<u8>,
    /// Pipelines stored since the last save.
    added: u64,
}

// SAFETY: D3D12 objects are free-threaded; every call is made under the
// mutex that holds the library.
unsafe impl Send for Library {}

const LIBRARY_FILE: &str = "host-pipelines.bin";
const EARLY_FILE: &str = "early-pipelines.txt";
/// Title pipelines listed for an early start, at most.
const EARLY_MAX: usize = 8192;

#[cfg(test)]
thread_local! {
    /// How often this thread has asked for the library, which opens it
    /// (and with it the host device) the first time in the process.
    static ASKED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn library() -> Option<&'static Mutex<Library>> {
    static LIBRARY: OnceLock<Option<Mutex<Library>>> = OnceLock::new();
    #[cfg(test)]
    ASKED.with(|asked| asked.set(asked.get() + 1));
    LIBRARY
        .get_or_init(|| {
            let device = crate::host::host()?.device.cast::<ID3D12Device1>().ok()?;
            let blob = DIR
                .get()
                .and_then(|d| std::fs::read(d.join(LIBRARY_FILE)).ok())
                .and_then(unseal)
                .unwrap_or_default();
            let library = open(&device, blob);
            if library.is_none() {
                tracing::warn!("no host pipeline library: pipelines are not kept");
            }
            library.map(Mutex::new)
        })
        .as_ref()
}

/// The file's mark: "VPL1" and then an FNV-1a checksum of the library.
const MAGIC: &[u8; 4] = b"VPL1";

fn checksum(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    })
}

/// The library bytes with the mark in front.
fn seal(blob: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(blob.len() + 12);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&checksum(blob).to_le_bytes());
    out.extend_from_slice(blob);
    out
}

/// The library bytes of a sealed file, or `None` if the mark or checksum is
/// wrong. Not every host checks the bytes itself (D3DMetal accepts any), so
/// a damaged file is refused here before the host sees it.
fn unseal(file: Vec<u8>) -> Option<Vec<u8>> {
    let (head, blob) = file.split_at_checked(12)?;
    let sum = u64::from_le_bytes(head[4..12].try_into().ok()?);
    (&head[..4] == MAGIC && sum == checksum(blob)).then(|| blob.to_vec())
}

/// The library in `blob`, or an empty one if the host rejects the blob (an
/// adapter or driver change, damage).
fn open(device: &ID3D12Device1, blob: Vec<u8>) -> Option<Library> {
    let make = |bytes: &[u8]| -> windows::core::Result<ID3D12PipelineLibrary1> {
        // SAFETY: the bytes outlive the library (kept beside it).
        unsafe {
            device
                .CreatePipelineLibrary::<ID3D12PipelineLibrary>(bytes)?
                .cast()
        }
    };
    if !blob.is_empty() {
        match make(&blob) {
            Ok(library) => {
                tracing::info!(bytes = blob.len(), "host pipeline library loaded");
                return Some(Library {
                    library,
                    _blob: blob,
                    added: 0,
                });
            }
            Err(e) => {
                tracing::info!(error = %e, "host pipeline library rejected; starting an empty one")
            }
        }
    } else {
        tracing::info!("no host pipeline library yet; starting an empty one");
    }
    let library = make(&[]).ok()?;
    Some(Library {
        library,
        _blob: Vec::new(),
        added: 0,
    })
}

/// A pipeline's name in the library: the hash of what its host
/// description holds.
pub(crate) fn name(kind: &str, root: &[u8], code: &[&[u8]], state: &str) -> String {
    let mut h = blake3::Hasher::new();
    for part in [kind.as_bytes(), root]
        .into_iter()
        .chain(code.iter().copied())
        .chain([state.as_bytes()])
    {
        h.update(&(part.len() as u64).to_le_bytes());
        h.update(part);
    }
    h.finalize().to_hex()[..32].to_owned()
}

/// The pipeline named `name`: loaded from the library by `load` if it is
/// there with the same description, else made by `create` and stored.
pub(crate) fn pipeline(
    name: &str,
    load: impl FnOnce(&ID3D12PipelineLibrary1, PCWSTR) -> windows::core::Result<ID3D12PipelineState>,
    create: impl FnOnce() -> windows::core::Result<ID3D12PipelineState>,
) -> windows::core::Result<ID3D12PipelineState> {
    let wide = HSTRING::from(name);
    let Some(library) = library() else {
        MADE.fetch_add(1, Ordering::Relaxed);
        return create();
    };
    if let Ok(pso) = load(
        &library.lock().unwrap_or_else(|e| e.into_inner()).library,
        PCWSTR(wide.as_ptr()),
    ) {
        LOADED.fetch_add(1, Ordering::Relaxed);
        return Ok(pso);
    }
    // Compiled outside the lock: compiles take up to seconds.
    let pso = create()?;
    MADE.fetch_add(1, Ordering::Relaxed);
    let mut l = library.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: a live pipeline; a name already stored (a description that
    // differs) is refused, which leaves the library as it was.
    if unsafe { l.library.StorePipeline(PCWSTR(wide.as_ptr()), &pso) }.is_ok() {
        l.added += 1;
    }
    Ok(pso)
}

/// Writes the library and the early list if pipelines were added since the
/// last save and `SAVE_EVERY` has passed (or `now`).
pub(crate) fn save(now: bool) {
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    const SAVE_EVERY: std::time::Duration = std::time::Duration::from_secs(20);
    let Some((dir, library)) = save_target(DIR.get(), library) else {
        return;
    };
    {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if !now && last.is_some_and(|t| t.elapsed() < SAVE_EVERY) {
            return;
        }
        *last = Some(std::time::Instant::now());
    }
    let blob = {
        let mut l = library.lock().unwrap_or_else(|e| e.into_inner());
        if l.added == 0 && !USED_CHANGED.swap(false, Ordering::Relaxed) {
            return;
        }
        // SAFETY: a buffer of the size the library asks for.
        let bytes = unsafe {
            let mut bytes = vec![0u8; l.library.GetSerializedSize()];
            l.library.Serialize(&mut bytes).map(|()| bytes)
        };
        match bytes {
            Ok(bytes) => {
                l.added = 0;
                bytes
            }
            Err(e) => {
                tracing::warn!(error = %e, "host pipeline library not serialised");
                return;
            }
        }
    };
    let used: Vec<String> = USED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .flatten()
        .cloned()
        .collect();
    let written = std::fs::create_dir_all(dir)
        .and_then(|()| write_atomically(&dir.join(LIBRARY_FILE), &seal(&blob)))
        .and_then(|()| write_atomically(&dir.join(EARLY_FILE), used.join("\n").as_bytes()));
    match written {
        Ok(()) => tracing::info!(
            bytes = blob.len(),
            early = used.len(),
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            "host pipeline library saved"
        ),
        Err(e) => tracing::warn!(error = %e, "host pipeline library not saved"),
    }
}

/// Writes `bytes` to a temporary file beside `path`, then renames it over
/// `path`: a reader never sees a part.
fn write_atomically(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Title pipeline keys drawn or dispatched with (this run and the last).
static USED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
static USED_CHANGED: AtomicBool = AtomicBool::new(false);

fn used() -> std::sync::MutexGuard<'static, Option<HashSet<String>>> {
    let mut used = USED.lock().unwrap_or_else(|e| e.into_inner());
    if used.is_none() {
        let listed = DIR
            .get()
            .and_then(|d| std::fs::read_to_string(d.join(EARLY_FILE)).ok())
            .unwrap_or_default();
        let set: HashSet<String> = listed
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect();
        tracing::info!(pipelines = set.len(), "title pipelines to start early");
        *used = Some(set);
    }
    used
}

/// Whether the title pipeline with `key` was drawn with in an earlier run.
pub(crate) fn wanted_early(key: &str) -> bool {
    used().as_ref().is_some_and(|s| s.contains(key))
}

/// Where a save goes and what it saves. Without a cache folder there is
/// nothing to save, and the library is not asked for: asking opens it,
/// which makes the host device, and the workers save after every pipeline,
/// also one that needed no device.
fn save_target<L>(
    dir: Option<&PathBuf>,
    library: impl FnOnce() -> Option<L>,
) -> Option<(&PathBuf, L)> {
    let dir = dir?;
    Some((dir, library()?))
}

/// The title pipeline with `key` was drawn or dispatched with.
pub(crate) fn note_used(key: &str) {
    let mut used = used();
    let set = used.get_or_insert_with(HashSet::new);
    if set.len() < EARLY_MAX && set.insert(key.to_owned()) {
        USED_CHANGED.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_change_with_what_goes_into_the_pipeline() {
        let a = name("graphics", b"root", &[b"vs", b"ps"], "state");
        assert_eq!(a, name("graphics", b"root", &[b"vs", b"ps"], "state"));
        assert_ne!(a, name("graphics", b"root", &[b"vs", b"ps2"], "state"));
        assert_ne!(a, name("graphics", b"root", &[b"vs", b"ps"], "state2"));
        assert_ne!(a, name("graphics", b"root2", &[b"vs", b"ps"], "state"));
        assert_ne!(a, name("mesh", b"root", &[b"vs", b"ps"], "state"));
        // Parts do not run into each other.
        assert_ne!(a, name("graphics", b"root", &[b"vsp", b"s"], "state"));
    }

    #[test]
    fn sealed_files_round_trip_and_damage_is_refused() {
        let blob = b"library bytes".to_vec();
        let mut file = seal(&blob);
        assert_eq!(unseal(file.clone()), Some(blob));
        file[20] ^= 1;
        assert_eq!(unseal(file), None);
        assert_eq!(unseal(b"short".to_vec()), None);
    }

    /// A save without a cache folder must not open the library (and with
    /// it the host device): `pipeline::make`'s workers save after every
    /// pipeline, on their own threads, whatever test or title queued it.
    #[test]
    fn saving_without_a_cache_folder_opens_no_library() {
        let opened = std::cell::Cell::new(0);
        let open = || {
            opened.set(opened.get() + 1);
            Some("library")
        };
        assert_eq!(save_target(None, open), None);
        assert_eq!(opened.get(), 0);
        let dir = PathBuf::from("cache");
        assert_eq!(save_target(Some(&dir), open), Some((&dir, "library")));
        assert_eq!(opened.get(), 1);
        assert_eq!(save_target(Some(&dir), || None::<&str>), None);
    }

    fn asked() -> u32 {
        ASKED.with(std::cell::Cell::get)
    }

    /// The rule above where it is applied: `save` itself, with no cache
    /// folder (the tests set none), does not ask for the library. The
    /// workers call it after every pipeline, outside the device tests'
    /// lock; were it to ask, any test that queues a pipeline would make
    /// the host device on a worker thread.
    #[test]
    fn a_save_without_a_cache_folder_asks_for_no_library() {
        // Held in case the save does ask: that reaches the device.
        let _serial = crate::testing::serial();
        assert_eq!(DIR.get(), None, "the tests set no cache folder");
        let before = asked();
        save(true);
        save(false);
        assert_eq!(asked(), before, "a save asked for the library");
    }

    /// What the test above counts is counted: asking for the library is
    /// seen on the asking thread, and only there.
    #[test]
    fn asking_for_the_library_is_counted() {
        let Some((_gpu, _host)) = crate::testing::gpu() else {
            return;
        };
        let before = asked();
        let first = library().map(std::ptr::from_ref);
        assert_eq!(asked(), before + 1);
        assert_eq!(library().map(std::ptr::from_ref), first);
        assert_eq!(asked(), before + 2);
        let elsewhere = std::thread::spawn(asked).join();
        assert_eq!(elsewhere.ok(), Some(0));
    }

    #[test]
    fn a_rejected_blob_gives_an_empty_library() {
        let Some((_gpu, host)) = crate::testing::gpu() else {
            return;
        };
        let device: ID3D12Device1 = host.device.cast().expect("a device with libraries");
        // A damaged file is refused by the mark check before the host sees
        // it, because not every host checks the bytes itself.
        let blob = unseal(b"not a pipeline library".repeat(16)).unwrap_or_default();
        let library = open(&device, blob).expect("an empty library");
        // SAFETY: a plain query on a live library.
        assert!(unsafe { library.library.GetSerializedSize() } > 0);
        assert!(library._blob.is_empty());
    }
}
