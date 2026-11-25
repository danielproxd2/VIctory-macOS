//! Which imports do titles need, and which do we cover?

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use victory_hle::{HleResolver, HostPolicy};
use victory_loader::Provenance;
use victory_pe::{ImageInfo, ImportSymbol};

#[derive(Default)]
struct DllStats {
    functions: BTreeSet<ImportSymbol>,
    hle: usize,
    host: usize,
    missing: usize,
    images: BTreeSet<PathBuf>,
}

pub fn run(
    paths: Vec<PathBuf>,
    strict: bool,
    list_missing: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let paths = crate::default_paths(paths)?;
    let resolver = HleResolver::new(if strict {
        HostPolicy::none()
    } else {
        HostPolicy::default()
    });

    let mut images = 0usize;
    let mut failed = Vec::new();
    let mut dlls: BTreeMap<String, DllStats> = BTreeMap::new();
    let mut missing: BTreeMap<(String, String), BTreeSet<PathBuf>> = BTreeMap::new();

    for root in &paths {
        for entry in walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
        {
            let path = entry.path();
            if !entry.file_type().is_file() || !looks_like_pe(path) {
                continue;
            }
            let info = match std::fs::read(path)
                .map_err(anyhow::Error::from)
                .and_then(|d| ImageInfo::parse(&d).map_err(anyhow::Error::from))
            {
                Ok(info) => info,
                Err(e) => {
                    failed.push((path.to_path_buf(), e.to_string()));
                    continue;
                }
            };
            images += 1;
            for module in info.imports.iter().chain(&info.delay_imports) {
                let key = module.dll.to_ascii_lowercase();
                let stats = dlls.entry(key.clone()).or_default();
                stats.images.insert(path.to_path_buf());
                for e in &module.entries {
                    let class = resolver.classify(&module.dll, &e.symbol);
                    if class.is_none() {
                        missing
                            .entry((key.clone(), e.symbol.to_string()))
                            .or_default()
                            .insert(path.to_path_buf());
                    }
                    // Count each dll!function once across all images.
                    if stats.functions.insert(e.symbol.clone()) {
                        match class {
                            Some(Provenance::Hle) => stats.hle += 1,
                            Some(Provenance::Host) => stats.host += 1,
                            Some(Provenance::Title) => unreachable!("census maps no title DLLs"),
                            None => stats.missing += 1,
                        }
                    }
                }
            }
        }
    }

    writeln!(out, "{images} images scanned, {} unreadable", failed.len())?;
    for (p, e) in &failed {
        writeln!(out, "  skipped {}: {e}", p.display())?;
    }
    writeln!(
        out,
        "\n{:<56} {:>6} {:>6} {:>6} {:>8} {:>7}",
        "dll", "funcs", "hle", "host", "missing", "images"
    )?;
    let (mut f, mut h, mut o, mut m) = (0, 0, 0, 0);
    for (dll, s) in &dlls {
        writeln!(
            out,
            "{dll:<56} {:>6} {:>6} {:>6} {:>8} {:>7}",
            s.functions.len(),
            s.hle,
            s.host,
            s.missing,
            s.images.len()
        )?;
        f += s.functions.len();
        h += s.hle;
        o += s.host;
        m += s.missing;
    }
    writeln!(out, "{:<56} {f:>6} {h:>6} {o:>6} {m:>8}", "TOTAL")?;

    if list_missing {
        let mut rows: Vec<_> = missing.into_iter().collect();
        rows.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
        writeln!(out, "\nmissing (by number of images using it)")?;
        for ((dll, sym), users) in rows {
            writeln!(out, "  {:>4}  {dll}!{sym}", users.len())?;
        }
    }
    Ok(())
}

/// Cheap pre-filter: an `MZ` header, regardless of extension (console
/// packages do not always use .exe/.dll).
fn looks_like_pe(path: &Path) -> bool {
    let mut magic = [0u8; 2];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok()
        && &magic == b"MZ"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{MISSING_DLL, MISSING_FUNCTION, line, report_of, title_image, write};

    /// The numbers of a DLL's row: functions, hle, host, missing, images.
    #[track_caller]
    fn row(text: &str, dll: &str) -> [usize; 5] {
        let numbers: Vec<usize> = line(text, &format!("{dll} "))
            .split_whitespace()
            .skip(1)
            .map(|n| n.parse().expect("a count"))
            .collect();
        numbers.try_into().expect("five counts")
    }

    #[test]
    fn census_reports_covered_and_missing_imports() {
        let dir = tempfile::tempdir().unwrap();
        // The same image twice (extensions do not matter), a file that
        // only starts like an image, and one that is none.
        write(dir.path(), "title.dll", &title_image());
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        write(&dir.path().join("sub"), "copy.bin", &title_image());
        let broken = write(dir.path(), "broken.exe", b"MZ and nothing else");
        write(dir.path(), "notes.txt", b"not an image");

        let text = report_of(|out| run(vec![dir.path().into()], false, true, out));
        assert_eq!(text.lines().next(), Some("2 images scanned, 1 unreadable"));
        line(&text, &format!("  skipped {}: ", broken.display()));
        // Ours: implemented by the runtime, on every host. DLL names are
        // compared without case; each function counts once however many
        // images import it.
        assert_eq!(row(&text, "xgameruntime.dll"), [1, 1, 0, 0, 2]);
        // Nobody's: a named function and an ordinal.
        assert_eq!(row(&text, MISSING_DLL), [2, 0, 0, 2, 2]);
        // Delay-load imports count like the others.
        assert_eq!(row(&text, "victory-test-delayed.dll"), [1, 0, 0, 1, 2]);

        // --missing lists what nobody covers, with the images using it.
        let (_, missing) = text
            .split_once("\nmissing (by number of images using it)\n")
            .expect("the list of missing imports");
        let listed: Vec<&str> = missing.lines().collect();
        for expected in [
            format!("     2  {MISSING_DLL}!{MISSING_FUNCTION}"),
            format!("     2  {MISSING_DLL}!#7"),
            "     2  victory-test-delayed.dll!VIctoryTestDelayed".to_owned(),
        ] {
            assert!(listed.contains(&expected.as_str()), "{expected}\n{text}");
        }
        assert!(!missing.contains("XGameRuntimeInitialize"), "{text}");

        // Without --missing only the table is printed.
        let text = report_of(|out| run(vec![dir.path().into()], false, false, out));
        assert!(!text.contains("missing (by number"), "{text}");
        assert!(!text.contains(MISSING_FUNCTION), "{text}");

        // --strict takes host pass-through away, not our own runtime.
        let text = report_of(|out| run(vec![dir.path().into()], true, false, out));
        assert_eq!(row(&text, "xgameruntime.dll"), [1, 1, 0, 0, 2]);
        assert_eq!(row(&text, MISSING_DLL), [2, 0, 0, 2, 2]);
    }
}
