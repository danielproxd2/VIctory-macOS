//! Loading a prepared image as a real Windows module and binding it.

use std::collections::HashSet;
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use anyhow::{Context, Result, bail};
use victory_pe::{ExportInfo, ImageInfo, ImportSymbol};
use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::LoadLibraryExW;
use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};

use crate::thunks::{ImportSite, ThunkBlock, UnimplementedPolicy};
use crate::{
    DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH, ImportResolver, PreparedImage, Provenance, Resolution,
    prepare,
};

/// A title image mapped into this process. A clone names the same mapping
/// (no reference of its own): unload one copy only.
#[derive(Debug, Clone)]
pub struct LoadedImage {
    module: HMODULE,
    path: PathBuf,
    info: ImageInfo,
    /// See [`LoadedImage::redirected_tls_loads`].
    redirected_tls_loads: usize,
}

// SAFETY: HMODULE is a process-wide handle (the base address); using it from
// any thread is fine.
unsafe impl Send for LoadedImage {}
// SAFETY: as above; `LoadedImage` has no interior mutability.
unsafe impl Sync for LoadedImage {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingImport {
    pub dll: String,
    pub symbol: String,
}

/// Outcome of [`LoadedImage::bind_imports`].
#[derive(Debug, Default, Clone)]
pub struct BindReport {
    pub hle: usize,
    pub host: usize,
    pub title: usize,
    pub missing: Vec<MissingImport>,
}

/// Writes the prepared copy into `dir` and maps it with the Windows loader.
///
/// `dir` receives derived game data, so it must be outside the repository.
pub fn load(prepared: &PreparedImage, dir: &Path) -> Result<LoadedImage> {
    victory_core::ensure_outside_repo(dir)?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(prepared.file_name());
    write_if_changed(&path, &prepared.bytes)?;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: `wide` is a NUL-terminated path. The image has no entry point
    // and no imports, so loading it runs no title code.
    let module = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), 0) };
    if module.is_null() {
        bail!(
            "LoadLibraryExW({}) failed: {}{}",
            path.display(),
            std::io::Error::last_os_error(),
            if prepared.info.has_relocations {
                ""
            } else {
                " (image has no relocations; its preferred base may be taken)"
            }
        );
    }
    let mut loaded = LoadedImage {
        module,
        path,
        info: prepared.info.clone(),
        redirected_tls_loads: 0,
    };
    // A DLL indexes its TLS properly; only an exe assumes index 0.
    if !loaded.info.is_dll() {
        // SAFETY: just mapped, and no title code has run.
        loaded.redirected_tls_loads = unsafe { crate::tls::redirect(loaded.base(), &loaded.info) }?;
    }
    tracing::info!(
        base = format_args!("{:#x}", loaded.base()),
        preferred = format_args!("{:#x}", loaded.info.image_base),
        path = %loaded.path.display(),
        "image mapped"
    );
    crate::threads::mapped(&loaded);
    Ok(loaded)
}

/// Maps the DLLs the title ships in `title_dir` that `main` imports,
/// directly or through each other, in initialisation order (dependencies
/// first). Only DLL names `wanted` accepts are considered, so DLLs the host
/// or our runtime provide stay with them, and only plain file names: an
/// import table cannot name a file outside `title_dir`. Imports are not
/// bound.
pub fn load_title_dlls(
    main: &ImageInfo,
    title_dir: &Path,
    cache_dir: &Path,
    wanted: &dyn Fn(&str) -> bool,
) -> Result<Vec<(String, LoadedImage)>> {
    let mut loaded = Vec::new();
    let dirs = (title_dir, cache_dir);
    map_imports(main, dirs, wanted, &mut HashSet::new(), &mut loaded)?;
    Ok(loaded)
}

/// Maps the DLL `name` from `title_dir` like [`load_title_dlls`] does, with
/// the title DLLs it imports (dependencies first, `name` last); empty if
/// `wanted` refuses it, the folder has no such file or `name` is not the
/// plain name of a file (a path, a drive, a device). For DLLs a title
/// loads itself; `wanted` must refuse those already mapped.
pub fn load_title_dll(
    name: &str,
    title_dir: &Path,
    cache_dir: &Path,
    wanted: &dyn Fn(&str) -> bool,
) -> Result<Vec<(String, LoadedImage)>> {
    let mut loaded = Vec::new();
    let dirs = (title_dir, cache_dir);
    map_dll(name, dirs, wanted, &mut HashSet::new(), &mut loaded)?;
    Ok(loaded)
}

fn map_imports(
    info: &ImageInfo,
    dirs: (&Path, &Path),
    wanted: &dyn Fn(&str) -> bool,
    seen: &mut HashSet<String>,
    loaded: &mut Vec<(String, LoadedImage)>,
) -> Result<()> {
    for module in info.imports.iter().chain(&info.delay_imports) {
        map_dll(&module.dll, dirs, wanted, seen, loaded)?;
    }
    Ok(())
}

fn map_dll(
    name: &str,
    dirs: (&Path, &Path),
    wanted: &dyn Fn(&str) -> bool,
    seen: &mut HashSet<String>,
    loaded: &mut Vec<(String, LoadedImage)>,
) -> Result<()> {
    let name = name.to_ascii_lowercase();
    if !seen.insert(name.clone()) || !wanted(&name) {
        return Ok(());
    }
    // The name is the title's: only a plain file name is looked for, and
    // only in the title's folder.
    let Some(path) = crate::names::title_dll_path(dirs.0, &name) else {
        tracing::warn!(dll = name, "not a file name: no title DLL is looked for");
        return Ok(());
    };
    if !path.is_file() {
        return Ok(());
    }
    let original = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    // Named: the same bytes under another name are another module.
    let prepared = prepare(&original)
        .with_context(|| format!("preparing {}", path.display()))?
        .named(&name);
    let image = load(&prepared, dirs.1).with_context(|| format!("loading {}", path.display()))?;
    map_imports(&image.info, dirs, wanted, seen, loaded)?;
    loaded.push((name, image));
    Ok(())
}

/// How many forwarders in a row are followed before the export counts as
/// missing: forwarders that name each other never end.
const MAX_FORWARDS: usize = 8;

/// Resolves through `inner`, then through the exports of the title's DLLs.
///
/// A forwarded export of a title DLL (`other.Function`, `other.#7`) is
/// resolved here like an import of `other.dll` by that DLL: by `inner`,
/// then by the title's DLLs, whichever of them goes by that name. The
/// Windows loader is never asked: it would look for a file of that name,
/// and map the title's unprepared DLL (or none) if it found one.
pub struct WithTitleDlls<'a> {
    pub inner: &'a dyn ImportResolver,
    pub dlls: &'a [(String, LoadedImage)],
}

impl WithTitleDlls<'_> {
    fn resolve_through(&self, dll: &str, symbol: &ImportSymbol, forwards: usize) -> Resolution {
        let found = self.inner.resolve(dll, symbol);
        if found != Resolution::Missing {
            return found;
        }
        let named = |(name, _): &&(String, LoadedImage)| name.eq_ignore_ascii_case(dll);
        let Some((_, image)) = self.dlls.iter().find(named) else {
            return Resolution::Missing;
        };
        let Some(export) = image.export_entry(symbol) else {
            return Resolution::Missing;
        };
        if let Some(rva) = export.rva {
            return Resolution::Found {
                address: image.base() + rva as usize,
                provenance: Provenance::Title,
            };
        }
        let forward = export.forward.as_deref().unwrap_or_default();
        let Some((target, target_symbol)) = crate::names::forward_target(forward) else {
            tracing::warn!(dll, %symbol, forward, "forwarder string not understood");
            return Resolution::Missing;
        };
        if forwards == MAX_FORWARDS {
            tracing::warn!(dll, %symbol, forward, "too many forwarders in a row");
            return Resolution::Missing;
        }
        self.resolve_through(&target, &target_symbol, forwards + 1)
    }
}

impl ImportResolver for WithTitleDlls<'_> {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        self.resolve_through(dll, symbol, 0)
    }
}

/// Puts `bytes` at `path`, where another process of ours may be putting
/// the same bytes at the same moment (the name says what is in the file),
/// or may have the file mapped already.
fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<()> {
    let there = || std::fs::read(path).is_ok_and(|existing| existing == bytes);
    if there() {
        return Ok(());
    }
    // Written under a name no other writer has, then renamed, so that the
    // file at `path` is whole at every moment: two writers that shared one
    // temporary file wrote into each other's, and one of them then renamed
    // a file the other was still writing.
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let unique = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("{}-{unique}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    // A file that is open or mapped elsewhere may refuse to be replaced.
    // That is no failure once it holds these bytes, and a reader that was
    // comparing it is gone a moment later.
    let mut tries = 0;
    let renamed = loop {
        match std::fs::rename(&tmp, path) {
            Err(_) if there() => {
                let _ = std::fs::remove_file(&tmp);
                break Ok(());
            }
            Err(_) if tries < RENAME_TRIES => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => {
                let _ = std::fs::remove_file(&tmp);
                break Err(error);
            }
            Ok(()) => break Ok(()),
        }
    };
    renamed.with_context(|| format!("renaming to {}", path.display()))
}

/// How often [`write_if_changed`] tries again to replace a file in use.
const RENAME_TRIES: u32 = 20;

impl LoadedImage {
    pub fn base(&self) -> usize {
        self.module as usize
    }

    /// Parsed from the original image (imports, entry point, TLS callbacks).
    pub fn info(&self) -> &ImageInfo {
        &self.info
    }

    /// How many loads of the TLS array in the image's code were rewritten
    /// when it was mapped (see `tls`): 0 for a DLL, for an image without
    /// static TLS, and for an executable the OS gave TLS index 0.
    pub fn redirected_tls_loads(&self) -> usize {
        self.redirected_tls_loads
    }

    /// Address of an export of the image, if it has one by that name with
    /// code or data of its own; `None` for a forwarded export, which only a
    /// resolver can follow (see [`WithTitleDlls`]).
    pub fn export(&self, name: &str) -> Option<usize> {
        self.export_symbol(&ImportSymbol::Name {
            hint: 0,
            name: name.to_owned(),
        })
    }

    /// Address of an export, by name or ordinal; as [`Self::export`].
    pub fn export_symbol(&self, symbol: &ImportSymbol) -> Option<usize> {
        let rva = self.export_entry(symbol)?.rva?;
        Some(self.base() + rva as usize)
    }

    /// The export table's entry for `symbol`, read from the image's file
    /// rather than asked of the host (`GetProcAddress`), which would load
    /// the DLL a forwarder names. Names are compared exactly, as there.
    fn export_entry(&self, symbol: &ImportSymbol) -> Option<&ExportInfo> {
        self.info.exports.iter().find(|export| match symbol {
            ImportSymbol::Name { name, .. } => export.name.as_deref() == Some(name.as_str()),
            ImportSymbol::Ordinal(ordinal) => export.ordinal == *ordinal,
        })
    }

    /// Fills every IAT slot of the image.
    ///
    /// Each import goes to `resolver`; whatever it cannot resolve gets an
    /// unimplemented-import stub that behaves according to `policy`.
    pub fn bind_imports(
        &self,
        resolver: &dyn ImportResolver,
        policy: UnimplementedPolicy,
    ) -> Result<BindReport> {
        let mut report = BindReport::default();
        let mut slots: Vec<(u32, usize)> = Vec::with_capacity(self.info.import_count());
        let mut unresolved: Vec<(u32, &str, &ImportSymbol)> = Vec::new();

        // Delay-load slots are bound eagerly too, so the title's own
        // delay-load helper (which would ask the host) never runs.
        for module in self.info.imports.iter().chain(&self.info.delay_imports) {
            for entry in &module.entries {
                match resolver.resolve(&module.dll, &entry.symbol) {
                    Resolution::Found {
                        address,
                        provenance,
                    } => {
                        match provenance {
                            Provenance::Hle => report.hle += 1,
                            Provenance::Host => report.host += 1,
                            Provenance::Title => report.title += 1,
                        }
                        slots.push((entry.iat_slot_rva, address));
                    }
                    Resolution::Missing => {
                        unresolved.push((entry.iat_slot_rva, &module.dll, &entry.symbol));
                    }
                }
            }
        }

        if !unresolved.is_empty() {
            let mut block = ThunkBlock::new(unresolved.len())?;
            for (slot, dll, symbol) in unresolved {
                let site: &'static ImportSite = Box::leak(Box::new(ImportSite {
                    dll: dll.to_owned(),
                    symbol: symbol.to_string(),
                    policy,
                    calls: AtomicU64::new(0),
                }));
                slots.push((slot, block.emit(site)?));
                report.missing.push(MissingImport {
                    dll: site.dll.clone(),
                    symbol: site.symbol.clone(),
                });
            }
            block.seal()?;
        }

        self.write_slots(&slots)?;
        tracing::info!(
            hle = report.hle,
            host = report.host,
            missing = report.missing.len(),
            "imports bound"
        );
        Ok(report)
    }

    /// Protects and restores each slot on its own: import tables can sit in
    /// sections with different protections (delay-load tables in `.didat`).
    fn write_slots(&self, slots: &[(u32, usize)]) -> Result<()> {
        for &(rva, address) in slots {
            let slot = (self.base() + rva as usize) as *mut u64;
            let mut old = 0;
            // SAFETY: the parser only yields slots inside the image.
            if unsafe { VirtualProtect(slot as *const c_void, 8, PAGE_READWRITE, &mut old) } == 0 {
                bail!(
                    "VirtualProtect(IAT slot {rva:#x}) failed: {}",
                    std::io::Error::last_os_error()
                );
            }
            // SAFETY: inside the image, now writable, 8-byte aligned by the PE format.
            unsafe { slot.write_volatile(address as u64) };
            let mut ignored = 0;
            // SAFETY: restoring the protection we just changed.
            unsafe { VirtualProtect(slot as *const c_void, 8, old, &mut ignored) };
        }
        Ok(())
    }

    /// Calls the image's TLS callbacks with `reason` (e.g. `DLL_PROCESS_ATTACH`).
    ///
    /// # Safety
    /// Runs title code. Imports must be bound first.
    pub unsafe fn run_tls_callbacks(&self, reason: u32) {
        // SAFETY: forwarded from the caller.
        unsafe { self.tls_callbacks(reason, std::ptr::null_mut()) }
    }

    /// # Safety
    /// See [`Self::run_tls_callbacks`].
    unsafe fn tls_callbacks(&self, reason: u32, reserved: *mut c_void) {
        let Some(tls) = &self.info.tls else { return };
        for &rva in &tls.callback_rvas {
            type TlsCallback = unsafe extern "system" fn(*mut c_void, u32, *mut c_void);
            // SAFETY: the RVA came from the image's own TLS directory.
            let callback: TlsCallback = unsafe { std::mem::transmute(self.base() + rva as usize) };
            // SAFETY: the caller guarantees imports are bound.
            unsafe { callback(self.module, reason, reserved) };
        }
    }

    /// Initialises a DLL as the Windows loader would: its TLS callbacks,
    /// then `DllMain(DLL_PROCESS_ATTACH)`. From then on it hears of every
    /// thread start and end (see `threads`).
    ///
    /// # Safety
    /// Runs title code. Imports must be bound first, and the DLLs this one
    /// depends on attached.
    pub unsafe fn attach(&self) -> Result<()> {
        // SAFETY: forwarded from the caller.
        if unsafe { self.notify(DLL_PROCESS_ATTACH, true, std::ptr::null_mut()) } == Some(0) {
            bail!("DllMain returned FALSE");
        }
        crate::threads::attached(self.base());
        Ok(())
    }

    /// `DLL_PROCESS_DETACH` as `FreeLibrary` sends it (TLS callbacks, then
    /// `DllMain`); the image hears of no more threads.
    ///
    /// # Safety
    /// Runs title code. The image must have been attached (or its attach
    /// failed), and no other thread may be using it.
    pub unsafe fn detach(&self) {
        crate::threads::detached(self.base());
        // SAFETY: forwarded from the caller.
        unsafe { self.notify(DLL_PROCESS_DETACH, true, std::ptr::null_mut()) };
    }

    /// Runs the image's TLS callbacks with `reason`, then, if `dll_main` and
    /// the image is a DLL with an entry point, its `DllMain`, whose result
    /// is returned. `reserved` is the entry points' third argument.
    ///
    /// # Safety
    /// Runs title code. Imports must be bound first.
    pub(crate) unsafe fn notify(
        &self,
        reason: u32,
        dll_main: bool,
        reserved: *mut c_void,
    ) -> Option<i32> {
        // SAFETY: forwarded from the caller.
        unsafe { self.tls_callbacks(reason, reserved) };
        if !dll_main || !self.info.is_dll() || self.info.entry_rva == 0 {
            return None;
        }
        type DllMain = unsafe extern "system" fn(*mut c_void, u32, *mut c_void) -> i32;
        // SAFETY: the entry RVA came from the original image header.
        let main: DllMain =
            unsafe { std::mem::transmute(self.base() + self.info.entry_rva as usize) };
        // SAFETY: forwarded from the caller.
        Some(unsafe { main(self.module, reason, reserved) })
    }

    /// Starts the title: on a new thread with the stack size from the image
    /// header, attaches `dlls` in order (see [`Self::attach`]), runs the
    /// image's TLS callbacks (`DLL_PROCESS_ATTACH`), then its original entry
    /// point. Returns the thread so the caller can join it.
    ///
    /// # Safety
    /// Runs title code. Imports of the image and of `dlls` must be bound.
    pub unsafe fn spawn_main(
        self: Arc<Self>,
        dlls: Vec<(String, LoadedImage)>,
    ) -> Result<std::thread::JoinHandle<Result<u32>>> {
        if self.info.entry_rva == 0 {
            bail!("image has no entry point");
        }
        let stack = usize::try_from(self.info.stack_reserve)
            .unwrap_or(usize::MAX)
            .max(1 << 20);
        std::thread::Builder::new()
            .name("title-main".into())
            .stack_size(stack)
            .spawn(move || {
                for (name, dll) in &dlls {
                    // SAFETY: forwarded from the caller of `spawn_main`.
                    unsafe { dll.attach() }.with_context(|| format!("attaching {name}"))?;
                }
                // SAFETY: forwarded from the caller of `spawn_main`.
                unsafe { self.run_tls_callbacks(DLL_PROCESS_ATTACH) };
                crate::threads::attached(self.base());
                type Entry = unsafe extern "system" fn(*mut c_void) -> u32;
                // SAFETY: the entry RVA came from the original image header.
                let entry: Entry =
                    unsafe { std::mem::transmute(self.base() + self.info.entry_rva as usize) };
                // SAFETY: forwarded from the caller of `spawn_main`.
                Ok(unsafe { entry(std::ptr::null_mut()) })
            })
            .context("spawning title main thread")
    }

    /// Unmaps the image.
    ///
    /// # Safety
    /// No title code may be running or run later, and no pointer into the
    /// image may be used afterwards.
    pub unsafe fn unload(self) {
        crate::threads::unmapped(self.base());
        // SAFETY: guaranteed by the caller.
        unsafe { FreeLibrary(self.module) };
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::*;

    /// How many files the two writers of the test below write, and how far
    /// apart in time.
    const ROUNDS: u64 = 8;
    const ROUND: Duration = Duration::from_millis(100);

    /// What an image file holds here: 4 MiB written by a rule, so that a
    /// file that is not whole is seen.
    fn contents() -> Vec<u8> {
        (0..4u32 << 20).map(|i| (i ^ (i >> 11)) as u8).collect()
    }

    fn round_file(dir: &Path, round: u64) -> PathBuf {
        dir.join(format!("victory-image-{round:016x}.dll"))
    }

    /// In a child process of the next test: writes each round's file at the
    /// moment the round starts, and says how it went.
    #[test]
    fn write_child() {
        let (Some(dir), Some(start)) = (
            std::env::var_os("VICTORY_WRITE_CHILD"),
            std::env::var("VICTORY_WRITE_START").ok(),
        ) else {
            return;
        };
        let start = UNIX_EPOCH + Duration::from_millis(start.parse().unwrap());
        let bytes = contents();
        println!();
        for round in 0..ROUNDS {
            let at = start + ROUND * round as u32;
            while SystemTime::now() < at {
                std::thread::yield_now();
            }
            match write_if_changed(&round_file(Path::new(&dir), round), &bytes) {
                Ok(()) => println!("round {round}: written"),
                Err(error) => println!("round {round}: FAILED {error:#}"),
            }
        }
    }

    /// Two processes that put the same image at the same place at the same
    /// time (two runs of one title) both succeed, and the file is whole.
    #[test]
    fn concurrent_writers_leave_a_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        // What a run that was cut short left behind.
        for round in 0..ROUNDS {
            std::fs::write(round_file(dir.path(), round), b"cut short").unwrap();
        }
        // Late enough for both children to be running.
        let start = SystemTime::now() + Duration::from_secs(3);
        let start = start.duration_since(UNIX_EPOCH).unwrap().as_millis();
        let child = || {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "windows::tests::write_child", "--nocapture"])
                .env("VICTORY_WRITE_CHILD", dir.path())
                .env("VICTORY_WRITE_START", start.to_string())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        };
        let children = [child(), child()];
        let bytes = contents();
        for child in children {
            let out = child.wait_with_output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout);
            let written: Vec<_> = text.lines().filter(|l| l.starts_with("round ")).collect();
            let expected: Vec<_> = (0..ROUNDS)
                .map(|round| format!("round {round}: written"))
                .collect();
            assert_eq!(written, expected, "{text}");
            assert!(out.status.success(), "{text}");
        }
        for round in 0..ROUNDS {
            let file = std::fs::read(round_file(dir.path(), round)).unwrap();
            assert!(file == bytes, "round {round}: {} bytes", file.len());
        }
        // Nothing is left beside the files themselves.
        let mut left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        let mut files: Vec<_> = (0..ROUNDS)
            .map(|round| format!("victory-image-{round:016x}.dll"))
            .collect();
        files.sort();
        assert_eq!(left, files);
    }

    /// A file that already holds the bytes is left as it is, and one that
    /// holds others is replaced.
    #[test]
    fn write_if_changed_replaces_only_other_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.dll");
        write_if_changed(&path, b"first").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        let written = std::fs::metadata(&path).unwrap().modified().unwrap();
        write_if_changed(&path, b"first").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            written
        );
        write_if_changed(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["image.dll"]);
    }
}
