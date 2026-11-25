//! The loader's own TLS callback in the built `victory.exe`.
//!
//! Windows calls an executable's TLS callbacks in the order of its
//! callback array when a thread starts, and the loader's callback
//! (`on_thread` in crates/victory-loader/src/threads.rs) is what tells the
//! title's images about that thread. It is linked into the section
//! `.CRT$XLAV` so that it sorts ahead of every other callback of the
//! executable; whether it did is a property of the linked file, which is
//! read here with victory-pe. The file names its functions in its COFF
//! symbol table, which the MinGW-style link keeps.
#![cfg(windows)]

use victory_pe::ImageInfo;

const SCN_MEM_EXECUTE: u32 = 0x2000_0000;

/// How the loader's callback is named in a Rust symbol, in both manglings:
/// the path `victory_loader::threads::on_thread`, each part with its length.
const OURS: &str = "14victory_loader7threads9on_thread";

fn u16_at(file: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(*file.get(at..)?.first_chunk()?))
}

fn u32_at(file: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(*file.get(at..)?.first_chunk()?))
}

/// The symbols of the file's COFF symbol table that lie in a section, as
/// (RVA, name); `None` if a part of the table is not in the file. A file
/// without a symbol table has none.
fn symbols(file: &[u8], info: &ImageInfo) -> Option<Vec<(u32, String)>> {
    const SYMBOL: usize = 18;
    // The file header follows the signature the DOS header points at.
    let header = u32_at(file, 0x3C)? as usize + 4;
    let table = u32_at(file, header + 8)? as usize;
    let count = u32_at(file, header + 12)? as usize;
    let strings = table.checked_add(count.checked_mul(SYMBOL)?)?;
    let mut found = Vec::new();
    let mut index = 0;
    while index < count {
        let symbol = file
            .get(table + index * SYMBOL..)?
            .first_chunk::<SYMBOL>()?;
        // A name of up to eight bytes, or four zero bytes and the offset
        // of a longer one in the string table behind the symbols.
        let name = match symbol[..8] {
            [0, 0, 0, 0, ..] => {
                let at = strings.checked_add(u32_at(symbol, 4)? as usize)?;
                let long = file.get(at..)?;
                &long[..long.iter().position(|b| *b == 0)?]
            }
            _ => symbol[..8].split(|b| *b == 0).next()?,
        };
        let value = u32_at(symbol, 8)?;
        // Sections count from 1; 0 and below are not places in the image.
        let section = u16_at(symbol, 12)?.cast_signed();
        if let Some(section) = usize::try_from(section).ok().and_then(|s| s.checked_sub(1)) {
            let rva = info.sections.get(section)?.rva.checked_add(value)?;
            found.push((rva, String::from_utf8_lossy(name).into_owned()));
        }
        // The auxiliary records behind a symbol are not symbols.
        index += 1 + usize::from(symbol[17]);
    }
    Some(found)
}

#[test]
fn our_tls_callback_is_first_in_victory_exe() {
    let path = env!("CARGO_BIN_EXE_victory");
    let file = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let info = ImageInfo::parse(&file).unwrap();
    let tls = info.tls.as_ref().expect("victory.exe has a TLS directory");
    let callbacks = &tls.callback_rvas;
    assert!(!callbacks.is_empty(), "{tls:?}");
    // Each is code of the image.
    for rva in callbacks {
        let executable = info.sections.iter().any(|s| {
            s.characteristics & SCN_MEM_EXECUTE != 0
                && (s.rva..s.rva.saturating_add(s.virtual_size)).contains(rva)
        });
        assert!(executable, "callback {rva:#x} is not in a code section");
    }

    let symbols = symbols(&file, &info).expect("the symbol table lies in the file");
    if symbols.is_empty() {
        // An MSVC link keeps the names in a PDB beside the file.
        victory_gpu::test_support::skipped(
            "victory.exe has no COFF symbol table to name its TLS callbacks by \
             (our_tls_callback_is_first_in_victory_exe)",
        );
        return;
    }
    let names = |rva: u32| -> Vec<&str> {
        let at = symbols.iter().filter(|(at, _)| *at == rva);
        at.map(|(_, name)| name.as_str()).collect()
    };
    let named: Vec<(u32, Vec<&str>)> = callbacks.iter().map(|&rva| (rva, names(rva))).collect();
    let ours: Vec<usize> = named
        .iter()
        .enumerate()
        .filter(|(_, (_, names))| names.iter().any(|n| n.contains(OURS)))
        .map(|(position, _)| position)
        .collect();
    // In the array once, and ahead of the others: of the C runtime, of
    // Rust's standard library, of whatever is linked in later.
    assert_eq!(ours, [0], "the callbacks, in call order: {named:#x?}");
}
