use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

use crate::{Console, Unimplemented};

/// Prepares, maps and binds the title `exe`, and with `execute` starts it
/// and waits for its entry point to return. What is known before the title
/// runs, and how it ended, goes to `out`.
///
/// Returns what the title's entry point returned, if it ran: the caller
/// ends the process with it, as the exit code of a Windows process is what
/// its main function returns. (A title that calls `exit` itself ends the
/// process with its code there and then.)
///
/// The source for that, checked against the page as fetched on 2026-11-24
/// (https://learn.microsoft.com/windows/win32/api/processthreadsapi/nf-processthreadsapi-getexitcodeprocess,
/// Remarks): the status of a process that has ended is one of three values,
/// the second of them "The return value from the main or WinMain function
/// of the process." The commit that introduced this behaviour quoted the
/// sentence from memory; its wording is accurate, but for the capital of
/// the first word. The other two values are the one given to `ExitProcess`
/// or `TerminateProcess`, and the exception value of an unhandled exception
/// that ended the process.
#[cfg(windows)]
pub fn run(
    exe: &Path,
    unimplemented: Unimplemented,
    strict: bool,
    execute: bool,
    console: Console,
    out: &mut dyn Write,
) -> Result<Option<u32>> {
    use std::sync::Arc;
    use victory_hle::{HleResolver, HostPolicy};
    use victory_loader::{UnimplementedPolicy, WithTitleDlls};

    // A title that relaunched itself: the old process must be gone first.
    victory_hle::modules::lifecycle::wait_for_previous();
    let cfg = victory_core::Config::load()?;
    let original = std::fs::read(exe).with_context(|| format!("reading {}", exe.display()))?;
    let prepared = victory_loader::prepare(&original)?;
    // A DLL's entry point is its `DllMain`, which takes three arguments
    // and is not where a program starts: started as a title it would be
    // called with none and its answer taken for an exit code.
    //
    // The source, checked against the page as fetched on 2026-11-24
    // (https://learn.microsoft.com/windows/win32/debug/pe-format#characteristics):
    // of IMAGE_FILE_DLL (0x2000) it says that such files count as
    // executable files for almost all purposes, "although they cannot be
    // directly run." The commit that introduced this check quoted the last
    // four words from memory; they are accurate.
    anyhow::ensure!(
        !(execute && prepared.info.is_dll()),
        "{} is a DLL, which cannot be started as a title (without --execute it is mapped and bound)",
        exe.display()
    );
    let hash = prepared.original_hash.to_hex();
    match victory_game::identify(&hash) {
        Some(build) => writeln!(out, "build          {} ({hash})", build.label)?,
        None => writeln!(out, "build          unknown ({hash})")?,
    }

    let images = cfg.cache_subdir("images")?;
    victory_hle::modules::d3d12x::use_shader_cache(cfg.cache_subdir("shaders")?);
    let image = victory_loader::load(&prepared, &images)?;
    victory_hle::runtime::set_main_image(image.base());
    victory_hle::runtime::set_image_path(image.base(), exe);
    writeln!(
        out,
        "mapped at      {:#x} (preferred {:#x})",
        image.base(),
        image.info().image_base
    )?;
    let resolver = HleResolver::new(if strict {
        HostPolicy::none()
    } else {
        HostPolicy::default()
    });
    let wanted = |dll: &str| title_dll(&resolver.host, dll);
    let title_dir = exe.parent().unwrap_or(Path::new("."));
    // A missing manifest leaves id 0 and no storage quota, and so does
    // what the reader does not take from one that is there: said here,
    // because the title then keeps its storage under another id.
    let manifest = std::fs::read(title_dir.join("MicrosoftGame.config")).unwrap_or_default();
    let manifest = victory_hle::runtime::Title::read_manifest(&manifest);
    for refused in &manifest.refused {
        writeln!(out, "manifest       not taken as it stands: {refused}")?;
    }
    let title = manifest.title;
    let storage = cfg.data_subdir(&format!("{:08X}/storage", title.id))?;
    writeln!(
        out,
        "title          {:08X}, storage {} ({} MB)",
        title.id,
        storage.display(),
        title.storage_bytes >> 20
    )?;
    // The console's fixed drives: the package as `G:`, temporary files `T:`.
    victory_hle::modules::files::mount_at('G', title_dir);
    let temp = cfg.data_subdir(&format!("{:08X}/temp", title.id))?;
    victory_hle::modules::files::mount_at('T', &temp);
    victory_hle::runtime::set_title(victory_hle::runtime::Title { storage, ..title });
    victory_hle::console::set(match console {
        Console::SeriesX => victory_hle::console::Console::SeriesX,
        Console::SeriesS => victory_hle::console::Console::SeriesS,
    });
    writeln!(
        out,
        "console        {}",
        victory_hle::console::current().name()
    )?;
    let dlls = victory_loader::load_title_dlls(image.info(), title_dir, &images, &wanted)?;
    for (name, dll) in &dlls {
        writeln!(out, "title dll      {name} at {:#x}", dll.base())?;
        victory_hle::runtime::set_image_path(dll.base(), &title_dir.join(name));
    }

    let policy = match unimplemented {
        Unimplemented::Trap => UnimplementedPolicy::Trap,
        Unimplemented::Zero => UnimplementedPolicy::ReturnZero,
    };
    let with_dlls = WithTitleDlls {
        inner: &resolver,
        dlls: &dlls,
    };
    for (name, dll) in &dlls {
        let report = dll.bind_imports(&with_dlls, policy)?;
        out.write_all(bound(name, &report).as_bytes())?;
    }
    let exe_name = exe.file_name().unwrap_or_default().to_string_lossy();
    let report = image.bind_imports(&with_dlls, policy)?;
    out.write_all(bound(&exe_name, &report).as_bytes())?;
    if !execute {
        writeln!(out, "not executing (pass --execute to start the title)")?;
        return Ok(None);
    }
    // Whoever reads the report sees it before the title says anything.
    out.flush()?;
    victory_loader::report_crashes();
    victory_loader::log_crashes_to(temp.with_file_name("crashes.log"));
    victory_hle::runtime::use_console_timers();
    #[cfg(feature = "diagnostics")]
    diagnostics(&image, &cfg);
    let late = Arc::new(LateDlls {
        host: resolver.host.clone(),
        title_dir: title_dir.to_owned(),
        cache_dir: images,
        policy,
        main: [(exe_name.to_ascii_lowercase(), image.clone())],
        mapped: std::sync::Mutex::new(dlls.clone()),
        lock: LoaderLock::default(),
    });
    let exports = Arc::clone(&late);
    victory_hle::runtime::set_title_exports(Box::new(move |base, symbol| {
        exports.export(base, symbol)
    }));
    victory_hle::runtime::set_title_dll_loader(Box::new(move |file| {
        late.load(file).map_err(|e| format!("{e:#}"))
    }));
    // SAFETY: imports are bound; from here on we run title code.
    let main = unsafe { Arc::new(image).spawn_main(dlls)? };
    let code = main
        .join()
        .map_err(|_| anyhow::anyhow!("title main thread panicked"))??;
    writeln!(out, "title exited with {code}")?;
    Ok(Some(code))
}

/// The lines of the report about one image's imports: who provides how
/// many, and each one nobody provides, by name.
#[cfg(windows)]
fn bound(name: &str, report: &victory_loader::BindReport) -> String {
    let mut text = format!(
        "bound          {name}: {} hle, {} host, {} title, {} missing\n",
        report.hle,
        report.host,
        report.title,
        report.missing.len()
    );
    for m in &report.missing {
        text.push_str(&format!("  missing      {}!{}\n", m.dll, m.symbol));
    }
    text
}

/// The debugging aids (README): scheduled stack dumps, watched values and
/// memory saved on a crash.
#[cfg(all(windows, feature = "diagnostics"))]
fn diagnostics(image: &victory_loader::LoadedImage, cfg: &victory_core::Config) {
    victory_loader::dump_stacks_on_schedule();
    victory_hle::modules::audio::capture::start();
    watch(image.base(), image.info().size_of_image as usize);
    // Debugging aid: `VICTORY_CRASH_SAVE=<rva>:<len>,...` (hex) writes those
    // title ranges to `crash-<rva>.bin` in the cache dir on a crash.
    let size = image.info().size_of_image as usize;
    let ranges = std::env::var("VICTORY_CRASH_SAVE").unwrap_or_default();
    let hex = |s: &str| usize::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok();
    victory_loader::save_on_crash(
        ranges
            .split(',')
            .filter_map(|r| r.split_once(':'))
            .filter_map(|(rva, len)| Some((hex(rva)?, hex(len)?)))
            .filter(|&(rva, len)| rva.checked_add(len).is_some_and(|end| end <= size))
            .map(|(rva, len)| {
                let file = cfg.cache_dir.join(format!("crash-{rva:x}.bin"));
                (image.base() + rva, len, file)
            })
            .collect(),
    );
}

/// Debugging aid: `VICTORY_WATCH=<rva>,...` (hex) logs the u32 at each
/// title+rva whenever it changes, polled every 5 ms (an engine's state, say).
#[cfg(all(windows, feature = "diagnostics"))]
fn watch(base: usize, size: usize) {
    let Ok(list) = std::env::var("VICTORY_WATCH") else {
        return;
    };
    let rvas: Vec<usize> = list
        .split(',')
        .filter_map(|r| usize::from_str_radix(r.trim().trim_start_matches("0x"), 16).ok())
        .filter(|&rva| rva + 4 <= size)
        .collect();
    std::thread::spawn(move || {
        let mut seen = vec![None; rvas.len()];
        loop {
            for (&rva, seen) in rvas.iter().zip(&mut seen) {
                // SAFETY: four bytes inside the mapped image, checked above.
                let value = unsafe { ((base + rva) as *const u32).read_volatile() };
                if *seen != Some(value) {
                    eprintln!("VIctory: title+{rva:#x} = {value:#x}");
                    *seen = Some(value);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    });
}

/// DLLs the host or our runtime provide are never taken from the title.
#[cfg(windows)]
fn title_dll(host: &victory_hle::HostPolicy, dll: &str) -> bool {
    !host.allows(dll) && !victory_hle::registry().iter().any(|m| m.serves(dll))
}

/// Title DLLs the title loads itself (`LoadLibrary`) that nothing imports,
/// e.g. Grand Theft Auto VI's upscaler behind its FidelityFX loader.
#[cfg(windows)]
struct LateDlls {
    host: victory_hle::HostPolicy,
    title_dir: std::path::PathBuf,
    cache_dir: std::path::PathBuf,
    policy: victory_loader::UnimplementedPolicy,
    /// The title's executable, under its file name.
    main: [(String, victory_loader::LoadedImage); 1],
    /// Every title DLL mapped so far, to bind new ones against.
    mapped: std::sync::Mutex<Vec<(String, victory_loader::LoadedImage)>>,
    lock: LoaderLock,
}

#[cfg(windows)]
impl LateDlls {
    /// Every `GetProcAddress` of the title on one of its own images comes
    /// here: the export `symbol` of the image mapped at `base`, a forwarded
    /// one followed as at import binding (our runtime, the host under the
    /// same policy, the title's mapped DLLs).
    fn export(&self, base: usize, symbol: &victory_pe::ImportSymbol) -> Option<usize> {
        use victory_hle::runtime::title_export_in;
        let hle = victory_hle::HleResolver::new(self.host.clone());
        let mapped = self.mapped.lock().unwrap_or_else(|e| e.into_inner());
        title_export_in(&mapped, &hle, base, symbol).or_else(|| {
            let dlls = victory_loader::WithTitleDlls {
                inner: &hle,
                dlls: &mapped,
            };
            title_export_in(&self.main, &dlls, base, symbol)
        })
    }

    /// Every `LoadLibrary` of the title comes here first. Returns a title
    /// image called `file`, mapping it and the title DLLs it needs first,
    /// binding them and running their `DllMain` on the calling thread, as
    /// the Windows loader would; `None` if `file` is no title image.
    ///
    /// What it maps is reported on standard output, not to the writer
    /// `run` was given: this runs later, on the title's threads.
    fn load(&self, file: &str) -> Result<Option<usize>> {
        use victory_hle::runtime::{forget_image, image_named, set_image_path};
        // Waits while another thread initialises a DLL; a `DllMain` that
        // loads another DLL re-enters.
        self.lock.hold(|| {
            if let Some(base) = image_named(file) {
                return Ok(Some(base));
            }
            let new = {
                let mut mapped = self.mapped.lock().unwrap_or_else(|e| e.into_inner());
                let wanted = |dll: &str| title_dll(&self.host, dll) && image_named(dll).is_none();
                let new = victory_loader::load_title_dll(
                    file,
                    &self.title_dir,
                    &self.cache_dir,
                    &wanted,
                )?;
                let hle = victory_hle::HleResolver::new(self.host.clone());
                let old = victory_loader::WithTitleDlls {
                    inner: &hle,
                    dlls: &mapped,
                };
                let resolver = victory_loader::WithTitleDlls {
                    inner: &old,
                    dlls: &new,
                };
                for (name, dll) in &new {
                    println!(
                        "title dll      {name} at {:#x} (loaded by the title)",
                        dll.base()
                    );
                    let report = dll.bind_imports(&resolver, self.policy)?;
                    print!("{}", bound(name, &report));
                    set_image_path(dll.base(), &self.title_dir.join(name));
                }
                // Visible before their `DllMain`s run, which may load more.
                mapped.extend(new.iter().cloned());
                new
            };
            let failed = new.iter().enumerate().find_map(|(i, (name, dll))| {
                // SAFETY: bound above, dependencies attached first.
                let attached = unsafe { dll.attach() };
                attached
                    .err()
                    .map(|e| (i, e.context(format!("attaching {name}"))))
            });
            let Some((i, error)) = failed else {
                return Ok(new.last().map(|(_, dll)| dll.base()));
            };
            // As `LoadLibrary` does when a `DllMain` fails: detach what this
            // load attached (the failed one too) and unmap all of it.
            let bases: Vec<_> = new.iter().map(|(_, d)| d.base()).collect();
            self.mapped
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(_, d)| !bases.contains(&d.base()));
            for (_, done) in new[..=i].iter().rev() {
                // SAFETY: attached (or failing to) on this thread just now.
                unsafe { done.detach() };
            }
            for (_, dll) in new {
                forget_image(dll.base());
                // SAFETY: detached, and no title code holds it yet.
                unsafe { dll.unload() };
            }
            Err(error)
        })
    }
}

/// The Windows loader lock's shape: one thread at a time, re-entered by
/// the thread that holds it.
#[cfg(windows)]
#[derive(Default)]
struct LoaderLock {
    owner: std::sync::Mutex<(Option<std::thread::ThreadId>, usize)>,
    free: std::sync::Condvar,
}

#[cfg(windows)]
impl LoaderLock {
    fn hold<T>(&self, f: impl FnOnce() -> T) -> T {
        let me = std::thread::current().id();
        let lock = || self.owner.lock().unwrap_or_else(|e| e.into_inner());
        {
            let mut owner = lock();
            while owner.0.is_some_and(|o| o != me) {
                owner = self.free.wait(owner).unwrap_or_else(|e| e.into_inner());
            }
            *owner = (Some(me), owner.1 + 1);
        }
        let result = f();
        let mut owner = lock();
        owner.1 -= 1;
        if owner.1 == 0 {
            owner.0 = None;
            self.free.notify_one();
        }
        result
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::sync::{Arc, Mutex};

    use victory_loader::{BindReport, MissingImport};

    #[test]
    fn the_bind_report_names_each_missing_import() {
        let mut report = BindReport {
            hle: 2,
            host: 12,
            title: 1,
            missing: Vec::new(),
        };
        assert_eq!(
            super::bound("title.exe", &report),
            "bound          title.exe: 2 hle, 12 host, 1 title, 0 missing\n"
        );
        report.missing.push(MissingImport {
            dll: "nobody.dll".into(),
            symbol: "Nothing".into(),
        });
        assert_eq!(
            super::bound("a.dll", &report),
            "bound          a.dll: 2 hle, 12 host, 1 title, 1 missing\n  \
             missing      nobody.dll!Nothing\n"
        );
    }

    #[test]
    fn loader_lock_is_reentrant_and_exclusive() {
        let lock = Arc::new(super::LoaderLock::default());
        assert_eq!(lock.hold(|| lock.hold(|| 7)), 7);
        let order = Arc::new(Mutex::new(Vec::new()));
        let other = lock.hold(|| {
            let (theirs, their_order) = (lock.clone(), order.clone());
            let other = std::thread::spawn(move || {
                theirs.hold(|| their_order.lock().unwrap().push(2));
            });
            std::thread::sleep(std::time::Duration::from_millis(50));
            order.lock().unwrap().push(1);
            other
        });
        other.join().unwrap();
        assert_eq!(*order.lock().unwrap(), [1, 2]);
    }
}

#[cfg(not(windows))]
pub fn run(
    _: &Path,
    _: Unimplemented,
    _: bool,
    _: bool,
    _: Console,
    _: &mut dyn Write,
) -> Result<Option<u32>> {
    anyhow::bail!("running titles requires Windows")
}

#[cfg(all(test, not(windows)))]
mod tests {
    use std::path::Path;

    use crate::testing::report;
    use crate::{Console, Unimplemented};

    /// The loader is the Windows loader: on another host `run` says so,
    /// with or without `--execute`, and reports nothing.
    #[test]
    fn run_needs_windows() {
        for execute in [false, true] {
            let (text, result) = report(|out| {
                let exe = Path::new("title.exe");
                let (trap, console) = (Unimplemented::Trap, Console::SeriesX);
                super::run(exe, trap, false, execute, console, out).map(drop)
            });
            let error = result.expect_err("no loader here").to_string();
            assert_eq!(error, "running titles requires Windows");
            assert_eq!(text, "");
        }
    }
}
