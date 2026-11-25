use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use victory_pe::implib::ImportLibrary;
use victory_pe::{ImageInfo, layout};

pub fn run(file: &Path, list_imports: bool, list_exports: bool, out: &mut dyn Write) -> Result<()> {
    let data = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let info = ImageInfo::parse(&data).with_context(|| format!("parsing {}", file.display()))?;

    writeln!(out, "file           {}", file.display())?;
    writeln!(out, "file size      {} bytes", data.len())?;
    writeln!(out, "blake3         {}", blake3::hash(&data).to_hex())?;
    if let Some(cv) = &info.codeview {
        writeln!(
            out,
            "pdb            {}  (key {})",
            cv.pdb_path,
            cv.symbol_key()
        )?;
    }
    writeln!(
        out,
        "kind           {}",
        if info.is_dll() { "dll" } else { "exe" }
    )?;
    writeln!(
        out,
        "subsystem      {} ({})",
        info.subsystem,
        info.subsystem_name()
    )?;
    writeln!(
        out,
        "image base     {:#x}  size {:#x}",
        info.image_base, info.size_of_image
    )?;
    writeln!(out, "entry rva      {:#x}", info.entry_rva)?;
    writeln!(
        out,
        "stack          reserve {:#x} commit {:#x}",
        info.stack_reserve, info.stack_commit
    )?;
    writeln!(
        out,
        "dll flags      {:#06x}{}",
        info.dll_characteristics,
        dll_flags(info.dll_characteristics)
    )?;
    writeln!(
        out,
        "relocations    {}   exception table {}   load config {}",
        yes(info.has_relocations),
        yes(info.has_exception_table),
        yes(info.has_load_config)
    )?;
    if let Some(tls) = &info.tls {
        writeln!(
            out,
            "tls            {} callback(s)",
            tls.callback_rvas.len()
        )?;
    }

    writeln!(out, "\nsections")?;
    for s in &info.sections {
        writeln!(
            out,
            "  {:<8} rva {:#010x} vsize {:#010x} raw {:#010x} flags {:#010x}",
            s.name.trim_end_matches('\0'),
            s.rva,
            s.virtual_size,
            s.file_size,
            s.characteristics
        )?;
    }

    writeln!(
        out,
        "\nimports        {} functions from {} DLLs",
        info.import_count(),
        info.imports.len()
    )?;
    for m in &info.imports {
        writeln!(out, "  {:<48} {:>5}", m.dll, m.entries.len())?;
        if list_imports {
            for e in &m.entries {
                writeln!(out, "      {}", e.symbol)?;
            }
        }
    }
    if !info.delay_imports.is_empty() {
        writeln!(out, "\ndelay-load imports")?;
        for m in &info.delay_imports {
            writeln!(out, "  {:<48} {:>5}", m.dll, m.entries.len())?;
            if list_imports {
                for e in &m.entries {
                    writeln!(out, "      {}", e.symbol)?;
                }
            }
        }
    }

    writeln!(out, "\nexports        {}", info.exports.len())?;
    if list_exports {
        for e in &info.exports {
            let name = e.name.as_deref().unwrap_or("(by ordinal)");
            match (&e.rva, &e.forward) {
                (_, Some(fwd)) => writeln!(out, "  {:>5}  {name} -> {fwd}", e.ordinal)?,
                (Some(rva), _) => writeln!(out, "  {:>5}  {name}  {rva:#x}", e.ordinal)?,
                _ => writeln!(out, "  {:>5}  {name}", e.ordinal)?,
            }
        }
    }
    Ok(())
}

pub fn lib_exports(lib: &Path, list: bool, out: &mut dyn Write) -> Result<()> {
    let data = std::fs::read(lib).with_context(|| format!("reading {}", lib.display()))?;
    let parsed =
        ImportLibrary::parse(&data).with_context(|| format!("parsing {}", lib.display()))?;
    let mut by_dll: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for i in &parsed.imports {
        by_dll
            .entry(i.dll.to_ascii_lowercase())
            .or_default()
            .push(i.export.to_string());
    }
    writeln!(
        out,
        "{} exports from {} DLLs ({} other members)",
        parsed.imports.len(),
        by_dll.len(),
        parsed.other_members
    )?;
    for (dll, names) in &by_dll {
        writeln!(out, "{dll:<60} {:>5}", names.len())?;
        if list {
            for n in names {
                writeln!(out, "    {n}")?;
            }
        }
    }
    Ok(())
}

fn yes(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

fn dll_flags(v: u16) -> String {
    use layout::dll_flags::*;
    let names = [
        (HIGH_ENTROPY_VA, "high-entropy-va"),
        (DYNAMIC_BASE, "dynamic-base"),
        (FORCE_INTEGRITY, "force-integrity"),
        (NX_COMPAT, "nx"),
        (APPCONTAINER, "appcontainer"),
        (GUARD_CF, "cfg"),
    ];
    let set: Vec<_> = names
        .iter()
        .filter(|(f, _)| v & f != 0)
        .map(|(_, n)| *n)
        .collect();
    if set.is_empty() {
        String::new()
    } else {
        format!("  ({})", set.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{
        MISSING_DLL, MISSING_FUNCTION, line, report, report_of, title_image, write,
    };

    #[test]
    fn inspect_reports_a_synthetic_image() {
        let dir = tempfile::tempdir().unwrap();
        let image = title_image();
        let file = write(dir.path(), "title.dll", &image);

        let text = report_of(|out| run(&file, false, false, out));
        // The path and the size both start with "file".
        assert_eq!(
            line(&text, "file  "),
            format!("file           {}", file.display())
        );
        assert_eq!(
            line(&text, "file size "),
            format!("file size      {} bytes", image.len())
        );
        assert_eq!(
            line(&text, "blake3 "),
            format!("blake3         {}", blake3::hash(&image).to_hex())
        );
        assert_eq!(line(&text, "kind "), "kind           dll");
        assert_eq!(line(&text, "subsystem "), "subsystem      3 (windows-cui)");
        assert_eq!(line(&text, "entry rva "), "entry rva      0x1000");
        assert!(line(&text, "dll flags ").ends_with("(high-entropy-va, dynamic-base, nx)"));
        assert!(line(&text, "  .text ").contains("rva 0x00001000"), "{text}");
        // Each imported DLL with the number of its functions; the
        // functions themselves only with --imports.
        assert_eq!(
            line(&text, "  XGameRuntime.dll").split_whitespace().last(),
            Some("1")
        );
        assert_eq!(
            line(&text, &format!("  {MISSING_DLL}"))
                .split_whitespace()
                .last(),
            Some("2")
        );
        assert!(text.contains("\ndelay-load imports\n"), "{text}");
        assert_eq!(
            line(&text, "  victory-test-delayed.dll")
                .split_whitespace()
                .last(),
            Some("1")
        );
        assert_eq!(line(&text, "exports "), "exports        2");
        for listed_on_request in [
            "XGameRuntimeInitialize",
            MISSING_FUNCTION,
            "VIctoryTestDelayed",
            "title_entry",
        ] {
            assert!(!text.contains(listed_on_request), "{text}");
        }

        let text = report_of(|out| run(&file, true, false, out));
        for imported in [
            "      XGameRuntimeInitialize",
            &format!("      {MISSING_FUNCTION}"),
            "      #7",
            "      VIctoryTestDelayed",
        ] {
            assert_eq!(line(&text, imported), imported);
        }
        assert!(!text.contains("title_entry"), "{text}");

        let text = report_of(|out| run(&file, false, true, out));
        assert_eq!(line(&text, "      1  "), "      1  title_entry  0x1000");
        assert_eq!(
            line(&text, "      2  "),
            "      2  title_forwarded -> other.function"
        );
        assert!(!text.contains(MISSING_FUNCTION), "{text}");
    }

    #[test]
    fn inspect_fails_on_what_is_not_an_image_and_names_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = write(dir.path(), "notes.txt", b"MZ, but nothing after it");
        let (text, result) = report(|out| run(&file, true, true, out));
        let error = format!("{:#}", result.expect_err("not an image"));
        assert!(
            error.starts_with(&format!("parsing {}: ", file.display())),
            "{error}"
        );
        assert_eq!(text, "");

        let absent = dir.path().join("absent.exe");
        let (text, result) = report(|out| run(&absent, false, false, out));
        let error = format!("{:#}", result.expect_err("no such file"));
        assert!(
            error.starts_with(&format!("reading {}: ", absent.display())),
            "{error}"
        );
        assert_eq!(text, "");
    }

    /// A COFF archive holding short-format import objects for `imports`
    /// (symbol, DLL) and one ordinary object. Written from the published
    /// layouts (the archive member header and `IMPORT_OBJECT_HEADER`); the
    /// signature is kept in two pieces, because it is one of the file
    /// signatures the asset policy refuses.
    fn import_library(imports: &[(&str, &str)]) -> Vec<u8> {
        let mut lib = [&b"!<"[..], &b"arch>\n"[..]].concat();
        let mut member = |name: &str, data: &[u8]| {
            // name[16] date[12] uid[6] gid[6] mode[8] size[10], then the
            // two terminator bytes; members start at even offsets.
            let header = format!(
                "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                format!("{name}/"),
                0,
                0,
                0,
                0,
                data.len()
            );
            lib.extend_from_slice(header.as_bytes());
            lib.extend_from_slice(data);
            if data.len() % 2 == 1 {
                lib.push(b'\n');
            }
        };
        for (symbol, dll) in imports {
            let strings = [symbol.as_bytes(), &[0], dll.as_bytes(), &[0]].concat();
            let mut object = Vec::new();
            // Sig1 (IMAGE_FILE_MACHINE_UNKNOWN), Sig2, Version, Machine.
            for v in [0u16, 0xFFFF, 0, victory_pe::MACHINE_AMD64] {
                object.extend_from_slice(&v.to_le_bytes());
            }
            object.extend_from_slice(&0u32.to_le_bytes()); // TimeDateStamp
            object.extend_from_slice(&(strings.len() as u32).to_le_bytes());
            object.extend_from_slice(&0u16.to_le_bytes()); // Hint
            // Type IMPORT_OBJECT_CODE (0), name type IMPORT_OBJECT_NAME (1).
            object.extend_from_slice(&(1u16 << 2).to_le_bytes());
            object.extend_from_slice(&strings);
            member(dll, &object);
        }
        // An AMD64 object file with no sections.
        let mut plain = vec![0u8; 20];
        plain[..2].copy_from_slice(&victory_pe::MACHINE_AMD64.to_le_bytes());
        member("plain.obj", &plain);
        lib
    }

    #[test]
    fn lib_exports_counts_per_dll_and_lists_on_request() {
        let dir = tempfile::tempdir().unwrap();
        let lib = import_library(&[
            ("First", "One.dll"),
            ("Second", "one.dll"),
            ("Third", "two.dll"),
        ]);
        let file = write(dir.path(), "synthetic.lib", &lib);

        let text = report_of(|out| lib_exports(&file, false, out));
        assert_eq!(
            text.lines().next(),
            Some("3 exports from 2 DLLs (1 other members)")
        );
        // DLL names compare without case.
        assert_eq!(line(&text, "one.dll").split_whitespace().last(), Some("2"));
        assert_eq!(line(&text, "two.dll").split_whitespace().last(), Some("1"));
        assert!(!text.contains("First"), "{text}");

        let text = report_of(|out| lib_exports(&file, true, out));
        for export in ["    First", "    Second", "    Third"] {
            assert_eq!(line(&text, export), export);
        }

        let not_a_lib = write(dir.path(), "notes.txt", b"not an archive");
        let (text, result) = report(|out| lib_exports(&not_a_lib, true, out));
        let error = format!("{:#}", result.expect_err("not an archive"));
        assert!(
            error.starts_with(&format!("parsing {}: ", not_a_lib.display())),
            "{error}"
        );
        assert_eq!(text, "");
    }
}
