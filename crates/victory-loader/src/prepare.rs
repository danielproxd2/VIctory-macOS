//! Turns a title's image into something the Windows loader maps but does
//! not run. Pure byte manipulation, so it is testable on any host.

use victory_pe::layout::{self, dir, dll_flags, file_flags};
use victory_pe::{ImageInfo, PeError};

#[derive(Debug, thiserror::Error)]
pub enum PrepareError {
    #[error(transparent)]
    Pe(#[from] PeError),
    #[error("cannot patch image: {0}")]
    Patch(&'static str),
}

/// A patched copy of an image plus everything we need from the original.
#[derive(Debug, Clone)]
pub struct PreparedImage {
    /// Parsed from the *original* bytes: imports, TLS callbacks and entry
    /// point as the title expects them.
    pub info: ImageInfo,
    /// The patched bytes to hand to the Windows loader.
    pub bytes: Vec<u8>,
    /// BLAKE3 of the original bytes: identifies the exact build.
    pub original_hash: blake3::Hash,
    /// See [`PreparedImage::named`]; lower case, empty if never said.
    name: String,
}

impl PreparedImage {
    /// Says which file of the title this image is (`title_a.dll`; ASCII
    /// case does not count). A title can ship one DLL under two names, and
    /// they are two modules, each with its own state: the name is part of
    /// what [`Self::file_name`] is made from, so each gets a copy of its
    /// own and Windows maps each on its own.
    pub fn named(mut self, name: &str) -> Self {
        self.name = name.to_ascii_lowercase();
        self
    }

    /// File name for the patched copy, unique per original build and, for
    /// an image that was [`named`](Self::named), per name.
    pub fn file_name(&self) -> String {
        let hash = if self.name.is_empty() {
            self.original_hash
        } else {
            let mut both = blake3::Hasher::new();
            both.update(self.original_hash.as_bytes());
            both.update(self.name.as_bytes());
            both.finalize()
        };
        format!("victory-image-{}.dll", &hash.to_hex()[..16])
    }
}

/// Produces the patched copy. The changes are:
///
/// * `IMAGE_FILE_DLL` set, entry point cleared: the OS maps it like a DLL
///   and calls nothing;
/// * import directory points to its empty terminator and the bound-import
///   directory is cleared: we bind the original slots using `info.imports`;
/// * TLS callback pointer cleared: the OS still allocates static TLS for
///   the image, but callbacks only run when we call them (after binding);
///   its base relocation is made a no-op so the 0 survives a rebase;
/// * `FORCE_INTEGRITY` cleared, checksum zeroed: the copy is unsigned.
///
/// An image whose callback pointer cannot be kept at 0 through a rebase is
/// refused: its relocation table is unreadable, or does not relocate the
/// pointer the one way we can undo.
pub fn prepare(original: &[u8]) -> Result<PreparedImage, PrepareError> {
    let info = ImageInfo::parse(original)?;
    let mut bytes = original.to_vec();

    let fh = layout::file_header_offset(&bytes).ok_or(PrepareError::Patch("no file header"))?;
    let opt =
        layout::optional_header_offset(&bytes).ok_or(PrepareError::Patch("no optional header"))?;

    let characteristics = info.characteristics | file_flags::DLL;
    write_u16(
        &mut bytes,
        fh + layout::FILE_HEADER_CHARACTERISTICS,
        characteristics,
    )?;
    write_u32(&mut bytes, opt + layout::OPT_ADDRESS_OF_ENTRY_POINT, 0)?;
    write_u32(&mut bytes, opt + layout::OPT_CHECKSUM, 0)?;
    let dll_characteristics = info.dll_characteristics & !dll_flags::FORCE_INTEGRITY;
    write_u16(
        &mut bytes,
        opt + layout::OPT_DLL_CHARACTERISTICS,
        dll_characteristics,
    )?;

    // Windows 10 assigns static TLS to the prepared DLL when its empty
    // import table is present. The parser has already walked one descriptor
    // per module up to the null terminator, regardless of the recorded Size.
    if let Some(at) = layout::data_directory_offset(&bytes, dir::IMPORT) {
        let rva = layout::read_u32(&bytes, at).ok_or(PrepareError::Patch("no import RVA"))?;
        if rva != 0 {
            let terminator = u32::try_from(info.imports.len())
                .ok()
                .and_then(|count| count.checked_mul(IMPORT_DESCRIPTOR_SIZE))
                .and_then(|offset| rva.checked_add(offset))
                .ok_or(PrepareError::Patch("import terminator RVA overflow"))?;
            write_u32(&mut bytes, at, terminator)?;
            write_u32(&mut bytes, at + 4, IMPORT_DESCRIPTOR_SIZE)?;
        }
    }
    if let Some(at) = layout::data_directory_offset(&bytes, dir::BOUND_IMPORT) {
        write(&mut bytes, at, &[0; layout::DATA_DIRECTORY_SIZE])?;
    }

    // With callbacks there is a pointer to clear. With base relocations
    // there may be an entry on the field even when it is 0 already, which
    // the loader would turn into a pointer just the same.
    if let Some(tls) = &info.tls
        && (tls.callbacks_va != 0 || info.has_relocations)
    {
        // Header values are untrusted: a directory in a section placed at
        // the top of the 32-bit address space overflows here.
        let field_rva = u32::try_from(layout::TLS64_ADDRESS_OF_CALLBACKS)
            .ok()
            .and_then(|offset| tls.directory_rva.checked_add(offset))
            .ok_or(PrepareError::Patch(CALLBACKS_FIELD_OVERFLOW))?;
        if tls.callbacks_va != 0 {
            let at = info
                .rva_to_file_offset(tls.directory_rva)
                .ok_or(PrepareError::Patch("TLS directory is not file-backed"))?;
            write_u64(
                &mut bytes,
                at as usize + layout::TLS64_ADDRESS_OF_CALLBACKS,
                0,
            )?;
        }
        // The field's base relocation would turn the 0 into `delta` whenever
        // the loader maps the copy away from its preferred base (Wine always
        // does for DYNAMIC_BASE images), and the loader then reads callbacks
        // from there. Make that entry a no-op. An image that the loader
        // will not relocate needs nothing.
        if info.has_relocations {
            let mut neutralised = false;
            for relocation in base_relocations(&bytes, &info)? {
                let Some(target) = relocation.target else {
                    continue;
                };
                if relocation.kind == REL_BASED_DIR64 && target == field_rva {
                    write_u16(&mut bytes, relocation.entry, 0)?; // IMAGE_REL_BASED_ABSOLUTE
                    neutralised = true;
                } else if relocation.kind != REL_BASED_ABSOLUTE && target.abs_diff(field_rva) < 8 {
                    // Another type, or a DIR64 that only overlaps the
                    // field: it would change part of the 0 and we do not
                    // know how to undo it.
                    return Err(PrepareError::Patch(CALLBACKS_FIELD_ODD_RELOCATION));
                }
            }
            // A pointer in a relocatable image always has its DIR64 entry.
            // Without one the table is not what the image was linked with,
            // and nothing we wrote above can be trusted to stay.
            if tls.callbacks_va != 0 && !neutralised {
                return Err(PrepareError::Patch(CALLBACKS_FIELD_NOT_RELOCATED));
            }
        }
    }

    Ok(PreparedImage {
        info,
        bytes,
        original_hash: blake3::hash(original),
        name: String::new(),
    })
}

/// Size of an IMAGE_IMPORT_DESCRIPTOR in the PE file.
const IMPORT_DESCRIPTOR_SIZE: u32 = 20;

/// `IMAGE_REL_BASED_ABSOLUTE`: padding, relocates nothing.
const REL_BASED_ABSOLUTE: u8 = 0;
/// `IMAGE_REL_BASED_DIR64`: adds the load delta to a 64-bit field.
const REL_BASED_DIR64: u8 = 10;

const CALLBACKS_FIELD_OVERFLOW: &str = "TLS callbacks field RVA overflow";
const CALLBACKS_FIELD_NOT_RELOCATED: &str =
    "the TLS callbacks field has no DIR64 base relocation in a relocatable image";
const CALLBACKS_FIELD_ODD_RELOCATION: &str =
    "a base relocation other than DIR64 at its start changes the TLS callbacks field";
const RELOCATIONS_OUTSIDE_SECTIONS: &str = "base relocation directory is outside the sections";
const RELOCATION_BLOCK_MALFORMED: &str = "malformed base relocation block";
const RELOCATION_BLOCK_ZERO_NOT_LAST: &str =
    "a base relocation block of size 0 is followed by bytes that are not zero";

/// One entry of the base relocation table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BaseRelocation {
    /// File offset of the 16-bit entry.
    entry: usize,
    /// `IMAGE_REL_BASED_*`.
    kind: u8,
    /// RVA it patches. `None` if page plus offset leaves the 32-bit address
    /// space: such an entry names no place in any image.
    target: Option<u32>,
}

/// Every entry of the base relocation table, in file order. The table comes
/// from the title, so a directory that is not wholly inside one section's
/// file data, or a block whose size does not fit the directory, is an error
/// and not a shorter list: an entry we did not see is one we cannot disarm.
fn base_relocations(bytes: &[u8], info: &ImageInfo) -> Result<Vec<BaseRelocation>, PrepareError> {
    let outside = PrepareError::Patch(RELOCATIONS_OUTSIDE_SECTIONS);
    let mut out = Vec::new();
    let Some(d) = layout::data_directory_offset(bytes, dir::BASERELOC) else {
        return Ok(out);
    };
    let (Some(rva), Some(size)) = (
        layout::read_u32(bytes, d),
        d.checked_add(4).and_then(|at| layout::read_u32(bytes, at)),
    ) else {
        return Ok(out);
    };
    if rva == 0 || size == 0 {
        return Ok(out);
    }
    // First and last byte must map to the same run of file data.
    let Some(start) = info.rva_to_file_offset(rva) else {
        return Err(outside);
    };
    let last = rva
        .checked_add(size - 1)
        .and_then(|rva| info.rva_to_file_offset(rva));
    if last != start.checked_add(size - 1) {
        return Err(outside);
    }
    let table = (start as usize)
        .checked_add(size as usize)
        .and_then(|end| bytes.get(start as usize..end))
        .ok_or(outside)?;

    let mut at = 0usize;
    // Fewer than 8 bytes cannot hold a block header: alignment padding.
    while let (Some(page), Some(block)) = (
        layout::read_u32(table, at),
        at.checked_add(4).and_then(|at| layout::read_u32(table, at)),
    ) {
        // A block of size 0 ends the table for Wine's loader. Whether the
        // Windows loader stops there too is not established, so only what
        // cannot matter is accepted behind it: zero bytes up to the end of
        // the directory, which arm nothing however a loader reads them (a
        // header of zeros is again a block of size 0, and fewer than 8
        // bytes hold no header). Anything else behind the block may be
        // entries one loader applies and another never sees, and those
        // could not be disarmed. (Neither half of this rule has been
        // checked against what a real linker writes.)
        if block == 0 {
            // Both fields were read, so the header's 8 bytes are there.
            if table[at + 8..].iter().any(|&b| b != 0) {
                return Err(PrepareError::Patch(RELOCATION_BLOCK_ZERO_NOT_LAST));
            }
            break;
        }
        let entries = at
            .checked_add(block as usize)
            .filter(|_| block >= 8 && block % 2 == 0)
            .and_then(|end| table.get(at + 8..end))
            .ok_or(PrepareError::Patch(RELOCATION_BLOCK_MALFORMED))?;
        for (i, e) in entries.as_chunks::<2>().0.iter().enumerate() {
            let e = u16::from_le_bytes(*e);
            out.push(BaseRelocation {
                entry: start as usize + at + 8 + i * 2,
                kind: (e >> 12) as u8,
                target: page.checked_add(u32::from(e & 0xFFF)),
            });
        }
        at += block as usize;
    }
    Ok(out)
}

fn write_u16(bytes: &mut [u8], at: usize, v: u16) -> Result<(), PrepareError> {
    write(bytes, at, &v.to_le_bytes())
}
fn write_u32(bytes: &mut [u8], at: usize, v: u32) -> Result<(), PrepareError> {
    write(bytes, at, &v.to_le_bytes())
}
fn write_u64(bytes: &mut [u8], at: usize, v: u64) -> Result<(), PrepareError> {
    write(bytes, at, &v.to_le_bytes())
}
fn write(bytes: &mut [u8], at: usize, v: &[u8]) -> Result<(), PrepareError> {
    bytes
        .get_mut(at..at + v.len())
        .ok_or(PrepareError::Patch("header field outside of file"))?
        .copy_from_slice(v);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use victory_pe::ImportSymbol;
    use victory_pe::testing::{DEFAULT_IMAGE_BASE, PeBuilder, SyntheticPe, reloc, scn};

    fn by_name(name: &str) -> ImportSymbol {
        ImportSymbol::Name {
            hint: 0,
            name: name.to_owned(),
        }
    }

    /// A signed-looking executable with code, imports from two DLLs and an
    /// entry point; `.text` is at RVA 0x1000.
    fn title() -> PeBuilder {
        PeBuilder::new()
            .entry_rva(0x1010)
            .dll_characteristics(
                dll_flags::DYNAMIC_BASE | dll_flags::NX_COMPAT | dll_flags::FORCE_INTEGRITY,
            )
            .section(".text", scn::CODE, &[0xC3; 0x40])
            .import("alpha.dll", &[by_name("First"), ImportSymbol::Ordinal(7)])
            .import("beta.dll", &[by_name("Second")])
    }

    /// [`title`] with two TLS callbacks and, if `page` is given, one base
    /// relocation block with that page RVA holding a single DIR64 entry at
    /// offset 0x18: the TLS directory's callbacks field when the page is
    /// the directory's own. The image is at the builder's default base.
    fn tls_title(page: Option<u32>) -> SyntheticPe {
        let mut builder = title().tls_callbacks(&[0x1000, 0x1020]);
        if let Some(page) = page {
            builder = builder.reloc_block(
                page,
                &[(reloc::DIR64, layout::TLS64_ADDRESS_OF_CALLBACKS as u16)],
            );
        }
        builder.build().unwrap()
    }

    /// `AddressOfCallBacks` of an image from [`tls_title`]: the builder's
    /// image base plus the RVA it gave the callback array.
    fn callbacks_va(built: &SyntheticPe) -> u64 {
        DEFAULT_IMAGE_BASE + u64::from(built.tls_callbacks_rva.unwrap())
    }

    /// Page RVA of the TLS directory of [`tls_title`]. The builder places
    /// base relocations last, so adding a block does not move the directory.
    fn tls_directory_page() -> u32 {
        let directory_rva = tls_title(None).tls_directory_rva.unwrap();
        assert_eq!(directory_rva & 0xFFF, 0, "the directory starts its page");
        directory_rva
    }

    /// Every DIR64 base relocation with a target: (file offset of its
    /// entry, RVA it patches).
    fn dir64_relocations(bytes: &[u8], info: &ImageInfo) -> Vec<(usize, u32)> {
        base_relocations(bytes, info)
            .unwrap()
            .into_iter()
            .filter(|r| r.kind == REL_BASED_DIR64)
            .filter_map(|r| Some((r.entry, r.target?)))
            .collect()
    }

    fn patch_error(result: Result<PreparedImage, PrepareError>) -> &'static str {
        match result {
            Err(PrepareError::Patch(why)) => why,
            other => panic!("expected PrepareError::Patch, got {other:?}"),
        }
    }

    /// File offset of the first entry of the first base relocation block.
    fn first_relocation_entry(bytes: &[u8]) -> usize {
        let directory = layout::data_directory_offset(bytes, dir::BASERELOC).unwrap();
        let rva = layout::read_u32(bytes, directory).unwrap();
        let info = ImageInfo::parse(bytes).unwrap();
        info.rva_to_file_offset(rva).unwrap() as usize + 8
    }

    #[cfg(windows)]
    #[test]
    fn patches_own_test_binary() {
        let original = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let prepared = prepare(&original).unwrap();
        assert!(!prepared.info.is_dll(), "info describes the original");
        assert!(prepared.info.import_count() > 0);
        // A Rust test binary for Windows has static TLS with callbacks (the
        // standard library's thread-local destructors) and relocations; said
        // first, so that the check below cannot be skipped in silence.
        let tls = prepared.info.tls.as_ref();
        let relocated_callbacks = tls.map(|t| (t.callbacks_va != 0, prepared.info.has_relocations));
        assert_eq!(relocated_callbacks, Some((true, true)));
        if let Some(tls) = tls {
            let field = tls.directory_rva + layout::TLS64_ADDRESS_OF_CALLBACKS as u32;
            let on_field = dir64_relocations(&original, &prepared.info)
                .iter()
                .filter(|&&(_, target)| target == field)
                .count();
            assert_eq!(on_field, 1, "one entry relocates the callbacks field");
        }

        let patched = ImageInfo::parse(&prepared.bytes).unwrap();
        assert!(patched.is_dll());
        assert_eq!(patched.entry_rva, 0);
        assert!(patched.imports.is_empty());
        assert_eq!(patched.dll_characteristics & dll_flags::FORCE_INTEGRITY, 0);
        if let Some(tls) = &patched.tls {
            assert_eq!(tls.callbacks_va, 0);
            let field = tls.directory_rva + layout::TLS64_ADDRESS_OF_CALLBACKS as u32;
            assert!(
                dir64_relocations(&prepared.bytes, &patched)
                    .iter()
                    .all(|&(_, target)| target != field),
                "the cleared callback pointer must not be relocated"
            );
        }
        // Nothing else moved: same sections, same size.
        assert_eq!(patched.sections.len(), prepared.info.sections.len());
        assert_eq!(prepared.bytes.len(), original.len());
    }

    #[test]
    fn patches_synthetic_image() {
        let mut original = title().build().unwrap().bytes;
        let opt = layout::optional_header_offset(&original).unwrap();
        write_u32(&mut original, opt + layout::OPT_CHECKSUM, 0x1234_5678).unwrap();
        // Stale bound-import data must not survive either.
        let bound = layout::data_directory_offset(&original, dir::BOUND_IMPORT).unwrap();
        write_u32(&mut original, bound, 0x1000).unwrap();
        write_u32(&mut original, bound + 4, 8).unwrap();

        let prepared = prepare(&original).unwrap();
        assert!(!prepared.info.is_dll(), "info describes the original");
        assert_eq!(prepared.info.entry_rva, 0x1010);
        assert_eq!(prepared.info.import_count(), 3);
        assert_eq!(prepared.original_hash, blake3::hash(&original));

        let patched = ImageInfo::parse(&prepared.bytes).unwrap();
        assert!(patched.is_dll());
        assert_eq!(patched.entry_rva, 0);
        assert!(patched.imports.is_empty());
        assert_eq!(patched.dll_characteristics & dll_flags::FORCE_INTEGRITY, 0);
        assert_eq!(
            patched.dll_characteristics,
            dll_flags::DYNAMIC_BASE | dll_flags::NX_COMPAT,
            "the other flags are kept"
        );
        assert_eq!(
            layout::read_u32(&prepared.bytes, opt + layout::OPT_CHECKSUM),
            Some(0)
        );
        assert_eq!(layout::read_u64(&prepared.bytes, bound), Some(0));
        // Nothing else moved: same sections, same size.
        assert_eq!(patched.sections.len(), prepared.info.sections.len());
        assert_eq!(patched.size_of_image, prepared.info.size_of_image);
        assert_eq!(prepared.bytes.len(), original.len());
    }

    #[test]
    fn an_image_without_imports_or_tls_is_still_prepared() {
        let original = PeBuilder::new()
            .entry_rva(0x1000)
            .section(".text", scn::CODE, &[0xC3; 0x10])
            .build()
            .unwrap()
            .bytes;
        let prepared = prepare(&original).unwrap();
        let patched = ImageInfo::parse(&prepared.bytes).unwrap();
        assert!(patched.is_dll());
        assert_eq!(patched.entry_rva, 0);
        let directory = layout::data_directory_offset(&original, dir::IMPORT).unwrap();
        assert_eq!(layout::read_u64(&prepared.bytes, directory), Some(0));
    }

    #[test]
    fn synthetic_tls_callbacks_are_neutralised() {
        let built = tls_title(Some(tls_directory_page()));
        let (original, directory_rva) = (&built.bytes, built.tls_directory_rva.unwrap());

        let prepared = prepare(original).unwrap();
        let tls = prepared.info.tls.as_ref().expect("info keeps the TLS data");
        assert_eq!(tls.callback_rvas, [0x1000, 0x1020]);
        assert_eq!(tls.callbacks_va, callbacks_va(&built));

        let patched = ImageInfo::parse(&prepared.bytes).unwrap();
        let patched_tls = patched.tls.as_ref().expect("the directory stays");
        assert_eq!(patched_tls.directory_rva, directory_rva);
        assert_eq!(patched_tls.callbacks_va, 0);
        assert!(patched_tls.callback_rvas.is_empty());
        let field = directory_rva + layout::TLS64_ADDRESS_OF_CALLBACKS as u32;
        assert!(
            dir64_relocations(original, &prepared.info)
                .iter()
                .any(|&(_, target)| target == field),
            "the original relocates the callbacks field"
        );
        assert!(
            dir64_relocations(&prepared.bytes, &patched)
                .iter()
                .all(|&(_, target)| target != field),
            "the cleared callback pointer must not be relocated"
        );
        // The callback array itself is left in place; only the pointer to
        // it is gone.
        let array = u32::try_from(tls.callbacks_va - prepared.info.image_base).unwrap();
        let at = patched.rva_to_file_offset(array).unwrap() as usize;
        assert_eq!(
            layout::read_u64(&prepared.bytes, at),
            Some(prepared.info.image_base + 0x1000)
        );
        assert_eq!(prepared.bytes.len(), original.len());
    }

    #[test]
    fn keeps_iat_slots_when_import_size_excludes_the_terminator() {
        let mut original = title().build().unwrap().bytes;
        let info = ImageInfo::parse(&original).unwrap();
        let directory = layout::data_directory_offset(&original, dir::IMPORT).unwrap();
        // This linker layout counts the descriptors for real modules only.
        let descriptor_bytes = u32::try_from(info.imports.len()).unwrap() * IMPORT_DESCRIPTOR_SIZE;
        write_u32(&mut original, directory + 4, descriptor_bytes).unwrap();
        let prepared = prepare(&original).unwrap();
        let empty = layout::read_u32(&prepared.bytes, directory).unwrap();
        assert_eq!(
            empty,
            layout::read_u32(&original, directory).unwrap() + descriptor_bytes
        );
        assert_eq!(
            layout::read_u32(&prepared.bytes, directory + 4),
            Some(IMPORT_DESCRIPTOR_SIZE)
        );
        let at = info.rva_to_file_offset(empty).unwrap() as usize;
        assert_eq!(
            &prepared.bytes[at..at + IMPORT_DESCRIPTOR_SIZE as usize],
            &[0; 20]
        );
        let iat = layout::data_directory_offset(&original, dir::IAT).unwrap();
        assert_ne!(layout::read_u64(&original, iat), Some(0));
        assert_eq!(
            &prepared.bytes[iat..iat + layout::DATA_DIRECTORY_SIZE],
            &original[iat..iat + layout::DATA_DIRECTORY_SIZE]
        );
        assert_eq!(prepared.info.import_count(), info.import_count());
        for module in &prepared.info.imports {
            for entry in &module.entries {
                let at = info.rva_to_file_offset(entry.iat_slot_rva).unwrap() as usize;
                assert_ne!(layout::read_u64(&original, at), Some(0));
                assert_eq!(&prepared.bytes[at..at + 8], &original[at..at + 8]);
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_assigns_static_tls_to_a_prepared_dll() {
        // Model a title DLL using our own linked image. Loading a DLL leaves
        // its assigned index intact; the executable-specific redirect resets it.
        let mut original = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let flags =
            layout::file_header_offset(&original).unwrap() + layout::FILE_HEADER_CHARACTERISTICS;
        let characteristics =
            ImageInfo::parse(&original).unwrap().characteristics | file_flags::DLL;
        write_u16(&mut original, flags, characteristics).unwrap();
        let prepared = prepare(&original).unwrap();
        let info = &prepared.info;
        let tls = info.tls.as_ref().expect("the test binary uses static TLS");
        let directory = info.rva_to_file_offset(tls.directory_rva).unwrap() as usize;
        let index_va = layout::read_u64(&original, directory + 16).unwrap();
        let index_rva = index_va.checked_sub(info.image_base).unwrap();
        assert!(index_rva + 4 <= u64::from(info.size_of_image));

        let folder = tempfile::tempdir().unwrap();
        let image = crate::load(&prepared, folder.path()).unwrap();
        // SAFETY: the mapped test image contains the checked DWORD above.
        let assigned =
            unsafe { ((image.base() + index_rva as usize) as *const u32).read_unaligned() };
        // SAFETY: the prepared DLL has never been attached or given to a caller.
        unsafe { image.unload() };
        assert_ne!(
            assigned, 0,
            "the process executable already owns TLS slot zero"
        );
    }

    #[test]
    fn a_relocation_page_near_4_gib_does_not_overflow() {
        // Header values are untrusted: page + offset used to overflow u32.
        // The block's other entry is the real relocation of the callbacks
        // field, on the directory's own page.
        let field = layout::TLS64_ADDRESS_OF_CALLBACKS as u16;
        let built = title()
            .tls_callbacks(&[0x1000, 0x1020])
            .reloc_block(0xFFFF_FFFF, &[(reloc::DIR64, field), (reloc::DIR64, 0)])
            .reloc_block(tls_directory_page(), &[(reloc::DIR64, field)])
            .build()
            .unwrap();
        let original = &built.bytes;
        let entry = first_relocation_entry(original);
        let dir64 = (u16::from(reloc::DIR64) << 12 | 0x18).to_le_bytes();
        assert_eq!(original[entry..entry + 2], dir64);
        // The answer is in the returned values, so it is the same answer
        // in a build without overflow checks: an entry past 4 GiB has no
        // target. Wrapping arithmetic would report RVA 0x17 for it.
        let info = ImageInfo::parse(original).unwrap();
        let relocations = base_relocations(original, &info).unwrap();
        let targets: Vec<_> = relocations.iter().map(|r| (r.kind, r.target)).collect();
        let field_rva = tls_directory_page() + 0x18;
        assert_eq!(
            targets,
            [
                (reloc::DIR64, None),
                (reloc::DIR64, Some(0xFFFF_FFFF)),
                (reloc::DIR64, Some(field_rva)),
                (reloc::ABSOLUTE, Some(tls_directory_page())),
            ]
        );
        assert_eq!(relocations[0].entry, entry);

        let prepared = prepare(original).unwrap();
        // The original's TLS data is reported untouched: the callback array
        // is where the builder put it, 64 bytes into the `.tls` section
        // that follows `.text` and `.idata`.
        let tls = prepared.info.tls.as_ref().unwrap();
        assert_eq!(tls.callbacks_va, callbacks_va(&built));
        assert_eq!(tls.callbacks_va, 0x1_8000_3040);
        assert_eq!(tls.callback_rvas, [0x1000, 0x1020]);
        // The entry is left alone: it names no field of the image. The
        // field's own entry, in the second block, is turned into a no-op.
        assert_eq!(prepared.bytes[entry..entry + 2], dir64);
        let own = relocations[2].entry;
        assert_eq!(original[own..own + 2], dir64);
        assert_eq!(prepared.bytes[own..own + 2], [0, 0]);
        let table = info.sections.last().unwrap();
        let table = table.file_offset as usize..(table.file_offset + table.file_size) as usize;
        assert!(table.contains(&entry) && table.contains(&own));
        for i in table {
            let same = original[i] == prepared.bytes[i];
            assert_eq!(same, !(own..own + 2).contains(&i), "table byte {i:#x}");
        }
        // On the directory's own page the first entry is the callbacks
        // field's relocation, and is turned into a no-op.
        let original = tls_title(Some(tls_directory_page())).bytes;
        assert_eq!(first_relocation_entry(&original), entry);
        let prepared = prepare(&original).unwrap();
        assert_eq!(prepared.bytes[entry..entry + 2], [0, 0]);
    }

    #[test]
    fn an_unrelocated_callbacks_field_is_an_error() {
        // The only DIR64 entry is on another page: after a rebase nothing
        // about this image's TLS directory can be relied on.
        let built = tls_title(Some(0xFFFF_FFFF));
        assert!(ImageInfo::parse(&built.bytes).unwrap().has_relocations);
        assert_eq!(
            patch_error(prepare(&built.bytes)),
            CALLBACKS_FIELD_NOT_RELOCATED
        );
        let other_page = tls_title(Some(tls_directory_page() + 0x1000));
        assert_eq!(
            patch_error(prepare(&other_page.bytes)),
            CALLBACKS_FIELD_NOT_RELOCATED
        );
        // An image the loader does not relocate needs no entry: without a
        // table, and with the table marked as stripped.
        assert!(prepare(&tls_title(None).bytes).is_ok());
        let stripped = title()
            .tls_callbacks(&[0x1000])
            .reloc_block(0x1000, &[(reloc::DIR64, 0)])
            .relocs_stripped(true)
            .build()
            .unwrap();
        assert!(prepare(&stripped.bytes).is_ok());
        // Nor does an image whose callbacks field is 0 to begin with.
        let no_callbacks = title()
            .tls_without_callbacks()
            .reloc_block(0xFFFF_FFFF, &[(reloc::HIGHLOW, 0x18)])
            .build()
            .unwrap();
        assert!(prepare(&no_callbacks.bytes).is_ok());
    }

    /// Found by `mutated_images_prepare_or_fail_cleanly`: a callbacks field
    /// that is 0 in the file but has a base relocation is a pointer once
    /// the loader has rebased the image.
    #[test]
    fn a_relocated_null_callbacks_field_is_disarmed() {
        let with = |kind: u8| {
            let built = title().tls_without_callbacks().build().unwrap();
            let page = built.tls_directory_rva.unwrap();
            assert_eq!(page, tls_directory_page());
            let built = title()
                .tls_without_callbacks()
                .reloc_block(page, &[(reloc::DIR64, 0), (kind, 0x18)])
                .build()
                .unwrap();
            let info = ImageInfo::parse(&built.bytes).unwrap();
            assert_eq!(info.tls.unwrap().callbacks_va, 0);
            (prepare(&built.bytes), first_relocation_entry(&built.bytes))
        };
        let (prepared, entry) = with(reloc::DIR64);
        let entries = &prepared.unwrap().bytes[entry..entry + 4];
        // The directory's first field keeps its relocation.
        assert_eq!(entries, [0x00, 0xA0, 0, 0]);
        assert_eq!(
            patch_error(with(reloc::HIGHLOW).0),
            CALLBACKS_FIELD_ODD_RELOCATION
        );
        // An unreadable table is refused here too: the entry may be in it.
        let built = title()
            .tls_without_callbacks()
            .reloc_block(0x1000, &[(reloc::DIR64, 0)])
            .build()
            .unwrap();
        let mut bytes = built.bytes;
        let block_size = first_relocation_entry(&bytes) - 4;
        write_u32(&mut bytes, block_size, 7).unwrap();
        assert_eq!(patch_error(prepare(&bytes)), RELOCATION_BLOCK_MALFORMED);
    }

    #[test]
    fn highlow_relocation_on_callbacks_is_an_error() {
        // A 32-bit relocation on the field would add the low half of the
        // load delta to the 0 we wrote. It used to be skipped, and the
        // image prepared as if the 0 were safe.
        let page = tls_directory_page();
        let with = |entries: &[(u8, u16)]| {
            let built = title().tls_callbacks(&[0x1000, 0x1020]);
            prepare(&built.reloc_block(page, entries).build().unwrap().bytes)
        };
        assert_eq!(
            patch_error(with(&[(reloc::HIGHLOW, 0x18)])),
            CALLBACKS_FIELD_ODD_RELOCATION
        );
        // Also next to the field's own DIR64 entry, on its upper half, and
        // as a DIR64 that only overlaps the field.
        for odd in [
            (reloc::HIGHLOW, 0x18),
            (reloc::HIGHLOW, 0x1C),
            (reloc::DIR64, 0x14),
            (reloc::DIR64, 0x1F),
        ] {
            assert_eq!(
                patch_error(with(&[(reloc::DIR64, 0x18), odd])),
                CALLBACKS_FIELD_ODD_RELOCATION,
                "{odd:?}"
            );
        }
        // Its neighbours are not the field: the 8 bytes before and after.
        let prepared = with(&[
            (reloc::DIR64, 0x10),
            (reloc::DIR64, 0x18),
            (reloc::HIGHLOW, 0x20),
            (reloc::ABSOLUTE, 0x18),
        ])
        .unwrap();
        let entry = first_relocation_entry(&prepared.bytes);
        let entries: Vec<_> = prepared.bytes[entry..entry + 8]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|e| u16::from_le_bytes(*e))
            .collect();
        assert_eq!(entries, [0xA010, 0, 0x3020, 0x0018]);
    }

    #[test]
    fn relocation_directory_outside_sections_is_an_error() {
        let built = tls_title(Some(tls_directory_page()));
        assert!(prepare(&built.bytes).is_ok());
        let directory = layout::data_directory_offset(&built.bytes, dir::BASERELOC).unwrap();
        let rva = layout::read_u32(&built.bytes, directory).unwrap();
        let size = layout::read_u32(&built.bytes, directory + 4).unwrap();
        let info = ImageInfo::parse(&built.bytes).unwrap();
        let reloc_section = info.sections.last().unwrap();
        assert_eq!((reloc_section.rva, reloc_section.virtual_size), (rva, size));

        let moved = |rva: u32, size: u32| {
            let mut bytes = built.bytes.clone();
            write_u32(&mut bytes, directory, rva).unwrap();
            write_u32(&mut bytes, directory + 4, size).unwrap();
            assert!(ImageInfo::parse(&bytes).unwrap().has_relocations);
            prepare(&bytes)
        };
        // Past the image, in the headers, at the 4 GiB limit; starting in
        // the section but running out of it; and starting in one section
        // and ending in the next. All used to be read as "no relocations",
        // and the image was prepared with its callbacks field still
        // relocated.
        for (rva, size) in [
            (info.size_of_image + 0x1000, size),
            (0x80, size),
            (0xFFFF_FFF8, 0x10),
            (rva, size + 8),
            (rva, 0x1000),
            (rva + 4, size),
            (0x1000, 0x2008),
        ] {
            assert_eq!(
                patch_error(moved(rva, size)),
                RELOCATIONS_OUTSIDE_SECTIONS,
                "directory {rva:#x}+{size:#x}"
            );
        }
        assert!(moved(rva, size).is_ok());
    }

    #[test]
    fn a_malformed_relocation_block_is_an_error() {
        let built = tls_title(Some(tls_directory_page()));
        let entry = first_relocation_entry(&built.bytes);
        let block_size = entry - 4;
        assert_eq!(layout::read_u32(&built.bytes, block_size), Some(12));
        // Shorter than its own header, odd, or longer than the directory.
        for size in [1u32, 7, 9, 11, 14, 0x1000, u32::MAX] {
            let mut bytes = built.bytes.clone();
            write_u32(&mut bytes, block_size, size).unwrap();
            assert_eq!(
                patch_error(prepare(&bytes)),
                RELOCATION_BLOCK_MALFORMED,
                "block size {size}"
            );
        }
        // A block of size 0 with an entry behind it: see
        // `a_zero_sized_relocation_block_must_be_the_last`.
        let mut bytes = built.bytes.clone();
        write_u32(&mut bytes, block_size, 0).unwrap();
        assert_eq!(patch_error(prepare(&bytes)), RELOCATION_BLOCK_ZERO_NOT_LAST);
        // A block with a header and no entries is well formed.
        let mut bytes = built.bytes.clone();
        write_u32(&mut bytes, block_size, 8).unwrap();
        let info = ImageInfo::parse(&bytes).unwrap();
        let relocations = base_relocations(&bytes, &info).unwrap();
        // The four bytes left over cannot hold another block header.
        assert_eq!(relocations, []);
    }

    /// A title with a TLS directory whose callbacks field is 0 and two
    /// base relocation blocks: an empty one and one with a DIR64 entry on
    /// the field, in the order asked for. Returns the image with the empty
    /// block's size set to 0, and the file offset of the DIR64 entry.
    fn with_a_zero_sized_block(zero_first: bool) -> (Vec<u8>, usize) {
        let page = tls_directory_page();
        let field = [(reloc::DIR64, layout::TLS64_ADDRESS_OF_CALLBACKS as u16)];
        let builder = title().tls_without_callbacks();
        let builder = if zero_first {
            builder.reloc_block(0, &[]).reloc_block(page, &field)
        } else {
            builder.reloc_block(page, &field).reloc_block(0, &[])
        };
        let mut bytes = builder.build().unwrap().bytes;
        let info = ImageInfo::parse(&bytes).unwrap();
        assert_eq!(info.tls.as_ref().unwrap().directory_rva, page);
        assert_eq!(info.tls.as_ref().unwrap().callbacks_va, 0);
        // An empty block is 8 bytes, the other one 12.
        let first = first_relocation_entry(&bytes) - 8;
        let (zero, entry) = if zero_first {
            (first, first + 8 + 8)
        } else {
            (first + 12, first + 8)
        };
        assert_eq!(layout::read_u32(&bytes, zero + 4), Some(8));
        assert_eq!(bytes[entry..entry + 2], 0xA018u16.to_le_bytes());
        write_u32(&mut bytes, zero + 4, 0).unwrap();
        (bytes, entry)
    }

    /// Wine's loader stops at a block of size 0; that the Windows loader
    /// does too is not established. An entry behind such a block is one a
    /// loader may apply and we did not disarm, so the image is refused.
    #[test]
    fn a_zero_sized_relocation_block_must_be_the_last() {
        // The callbacks field is 0 in the file and its DIR64 entry sits
        // behind the zero block: this used to prepare with the entry armed.
        let (bytes, entry) = with_a_zero_sized_block(true);
        assert_eq!(bytes[entry..entry + 2], 0xA018u16.to_le_bytes());
        assert_eq!(patch_error(prepare(&bytes)), RELOCATION_BLOCK_ZERO_NOT_LAST);
        let info = ImageInfo::parse(&bytes).unwrap();
        assert!(matches!(
            base_relocations(&bytes, &info),
            Err(PrepareError::Patch(RELOCATION_BLOCK_ZERO_NOT_LAST))
        ));
    }

    #[test]
    fn a_zero_sized_relocation_block_at_the_end_is_harmless() {
        // Nothing follows the zero block, so every loader sees the same
        // table, and the entry on the field is disarmed as usual.
        let (bytes, entry) = with_a_zero_sized_block(false);
        let prepared = prepare(&bytes).unwrap();
        assert_eq!(prepared.bytes[entry..entry + 2], [0, 0]);
        let info = ImageInfo::parse(&prepared.bytes).unwrap();
        let relocations = base_relocations(&prepared.bytes, &info).unwrap();
        // The 12-byte block's entry and its padding; nothing from the end.
        assert_eq!(relocations.len(), 2);
        assert!(relocations.iter().all(|r| r.kind == REL_BASED_ABSOLUTE));
    }

    /// What follows a block of size 0 inside the directory may be zero
    /// bytes (padding, which arms nothing) and nothing else. The image has
    /// the DIR64 block on the callbacks field (12 bytes), the zero block
    /// (8) and 12 more bytes; the directory is cut to each length from the
    /// zero block's end onwards, with each one of the bytes behind the
    /// block set in turn.
    #[test]
    fn only_zero_bytes_may_follow_a_zero_sized_relocation_block() {
        const BEFORE: u32 = 12 + 8;
        const BEHIND: usize = 12;
        let page = tls_directory_page();
        let field = [(reloc::DIR64, layout::TLS64_ADDRESS_OF_CALLBACKS as u16)];
        let built = title()
            .tls_without_callbacks()
            .reloc_block(page, &field)
            .reloc_block(0, &[])
            .reloc_block(0, &[(reloc::ABSOLUTE, 0)])
            .build()
            .unwrap();
        let mut bytes = built.bytes;
        let directory = layout::data_directory_offset(&bytes, dir::BASERELOC).unwrap();
        let size = directory + 4;
        assert_eq!(
            layout::read_u32(&bytes, size),
            Some(BEFORE + BEHIND as u32),
            "the three blocks"
        );
        let entry = first_relocation_entry(&bytes);
        let zero = entry - 8 + 12;
        let behind = zero + 8;
        assert_eq!(layout::read_u32(&bytes, zero + 4), Some(8));
        write_u32(&mut bytes, zero + 4, 0).unwrap();
        // The third block as the linker-style builder wrote it is not
        // padding: its size field says 12.
        assert_eq!(patch_error(prepare(&bytes)), RELOCATION_BLOCK_ZERO_NOT_LAST);
        bytes[behind..behind + BEHIND].fill(0);

        for kept in 0..=BEHIND {
            write_u32(&mut bytes, size, BEFORE + kept as u32).unwrap();
            // Zeros only: prepared, with the entry on the field disarmed
            // and nothing read from behind the zero block.
            let prepared = prepare(&bytes).unwrap_or_else(|e| panic!("{kept} zero bytes: {e}"));
            assert_eq!(
                prepared.bytes[entry..entry + 2],
                [0, 0],
                "{kept} zero bytes"
            );
            let info = ImageInfo::parse(&prepared.bytes).unwrap();
            let relocations = base_relocations(&prepared.bytes, &info).unwrap();
            assert_eq!(relocations.len(), 2, "{kept} zero bytes");
            assert!(relocations.iter().all(|r| r.kind == REL_BASED_ABSOLUTE));

            for set in 0..BEHIND {
                for value in [1u8, 0x80] {
                    let mut bytes = bytes.clone();
                    bytes[behind + set] = value;
                    let result = prepare(&bytes);
                    let told = format!("byte {set} of {kept} behind the block is {value:#x}");
                    if set < kept {
                        // In the directory: refused, one byte or many,
                        // whether or not a block header fits there.
                        assert_eq!(
                            patch_error(result),
                            RELOCATION_BLOCK_ZERO_NOT_LAST,
                            "{told}"
                        );
                        let info = ImageInfo::parse(&bytes).unwrap();
                        assert!(
                            matches!(
                                base_relocations(&bytes, &info),
                                Err(PrepareError::Patch(RELOCATION_BLOCK_ZERO_NOT_LAST))
                            ),
                            "{told}"
                        );
                    } else {
                        // Past the directory's end: not part of the table.
                        assert!(result.is_ok(), "{told}");
                    }
                }
            }
        }
    }

    #[test]
    fn tls_directory_near_4_gib_does_not_overflow() {
        // `.text` at 0x1000, the callback array in `.cb` at 0x2000 and the
        // TLS directory in the third section, which is then moved so that
        // the directory's callbacks field would start exactly at 4 GiB.
        let array = [DEFAULT_IMAGE_BASE + 0x1000, 0].map(u64::to_le_bytes);
        let built = PeBuilder::new()
            .section(".text", scn::CODE, &[0xC3; 0x40])
            .section(".cb", scn::RDATA, array.as_flattened())
            .tls_callbacks(&[0x1000])
            .build()
            .unwrap();
        let mut bytes = built.bytes;
        let info = ImageInfo::parse(&bytes).unwrap();
        let old_rva = built.tls_directory_rva.unwrap();
        let at = info.rva_to_file_offset(old_rva).unwrap() as usize;
        let moved_to = u32::MAX - 0x17;
        let section = victory_pe::testing::section_header_offset(&bytes, 2).unwrap();
        assert_eq!(layout::read_u32(&bytes, section + 12), Some(old_rva));
        write_u32(&mut bytes, section + 12, moved_to).unwrap();
        let directory = layout::data_directory_offset(&bytes, dir::TLS).unwrap();
        write_u32(&mut bytes, directory, moved_to).unwrap();
        write_u64(
            &mut bytes,
            at + layout::TLS64_ADDRESS_OF_CALLBACKS,
            DEFAULT_IMAGE_BASE + 0x2000,
        )
        .unwrap();

        // The parser accepts it: the directory and the array are backed by
        // file data and the callback is in `.text`.
        let info = ImageInfo::parse(&bytes).unwrap();
        let tls = info.tls.as_ref().unwrap();
        assert_eq!(tls.directory_rva, moved_to);
        assert_eq!(tls.callback_rvas, [0x1000]);
        // `directory_rva + 24` was unchecked: a panic with overflow checks,
        // and without them RVA 0, with the image prepared as if its
        // callbacks field were there.
        assert_eq!(patch_error(prepare(&bytes)), CALLBACKS_FIELD_OVERFLOW);
        // One byte lower the field fits, and the image is prepared.
        write_u32(&mut bytes, section + 12, moved_to - 8).unwrap();
        write_u32(&mut bytes, directory, moved_to - 8).unwrap();
        let prepared = prepare(&bytes).unwrap();
        assert_eq!(
            layout::read_u64(&prepared.bytes, at + layout::TLS64_ADDRESS_OF_CALLBACKS),
            Some(0)
        );
    }

    #[test]
    fn file_name_is_stable_per_build() {
        let original = title().build().unwrap().bytes;
        let a = prepare(&original).unwrap();
        let b = prepare(&original).unwrap();
        assert_eq!(a.file_name(), b.file_name());
        assert!(a.file_name().starts_with("victory-image-"));
        assert!(a.file_name().ends_with(".dll"));
        // Another build gets another name.
        let other = title().timestamp(1).build().unwrap().bytes;
        assert_ne!(prepare(&other).unwrap().file_name(), a.file_name());
    }

    /// One build shipped under two names is two files, whatever the case
    /// of the names; an image nobody named keeps the name of its build.
    #[test]
    fn file_name_tells_two_names_of_one_build_apart() {
        let original = title().build().unwrap().bytes;
        let named = |name: &str| prepare(&original).unwrap().named(name).file_name();
        let unnamed = prepare(&original).unwrap().file_name();
        assert_ne!(named("one.dll"), named("two.dll"));
        assert_ne!(named("one.dll"), unnamed);
        assert_eq!(named("one.dll"), named("ONE.Dll"));
        assert_eq!(named("one.dll"), named("one.dll"));
        assert_eq!(named(""), unnamed);
        for name in [named("one.dll"), named("two.dll"), named("..\\x:y")] {
            assert!(name.starts_with("victory-image-") && name.ends_with(".dll"));
            let hash = &name["victory-image-".len()..name.len() - ".dll".len()];
            assert_eq!(hash.len(), 16, "{name}");
            assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()), "{name}");
        }
        // The name says nothing of the build: another build under the same
        // name is another file.
        let other = title().timestamp(1).build().unwrap().bytes;
        let other = prepare(&other).unwrap().named("one.dll").file_name();
        assert_ne!(other, named("one.dll"));
    }

    /// Seeded mutations of images with imports, TLS callbacks and base
    /// relocations: prepare() either refuses the image or returns a copy
    /// that the OS loader can map without running or binding anything.
    #[test]
    fn mutated_images_prepare_or_fail_cleanly() {
        use victory_pe::testing::{XorShift64, mutate};

        let page = tls_directory_page();
        let field = layout::TLS64_ADDRESS_OF_CALLBACKS as u16;
        let seeds = [
            tls_title(Some(page)).bytes,
            title()
                .tls_callbacks(&[0x1000])
                .reloc_block(0x1000, &[(reloc::DIR64, 8), (reloc::HIGHLOW, 0x10)])
                .reloc_block(page, &[(reloc::DIR64, 0), (reloc::DIR64, field)])
                .build()
                .unwrap()
                .bytes,
            title().build().unwrap().bytes,
        ];
        for seed in &seeds {
            prepare(seed).unwrap();
        }

        let mut rng = XorShift64::new(0x5EED_0002);
        let (mut prepared_count, mut refused) = (0u32, 0u32);
        for i in 0..10_000 {
            let mut original = seeds[i % seeds.len()].clone();
            mutate(&mut original, 0x400, &mut rng);
            if rng.below(16) == 0 {
                original.truncate(rng.below(original.len()));
            }
            let prepared = match prepare(&original) {
                Ok(prepared) => prepared,
                Err(PrepareError::Pe(_) | PrepareError::Patch(_)) => {
                    refused += 1;
                    continue;
                }
            };
            prepared_count += 1;
            assert_eq!(prepared.bytes.len(), original.len(), "mutation {i}");
            assert_eq!(prepared.original_hash, blake3::hash(&original));
            let patched = match ImageInfo::parse(&prepared.bytes) {
                Ok(patched) => patched,
                Err(e) => panic!("mutation {i}: the patched copy does not parse: {e}"),
            };
            assert!(patched.is_dll(), "mutation {i}");
            assert_eq!(patched.entry_rva, 0, "mutation {i}");
            assert!(patched.imports.is_empty(), "mutation {i}");
            if let Some(tls) = &patched.tls {
                assert_eq!(tls.callbacks_va, 0, "mutation {i}");
                // No relocation is left that would change the field.
                if patched.has_relocations {
                    let field = u64::from(tls.directory_rva) + 0x18;
                    for r in base_relocations(&prepared.bytes, &patched).unwrap() {
                        let on_field = r.target.is_some_and(|t| u64::from(t).abs_diff(field) < 8);
                        assert!(
                            r.kind == REL_BASED_ABSOLUTE || !on_field,
                            "mutation {i}: {r:?}"
                        );
                    }
                }
            }
        }
        assert!(prepared_count > 0, "no mutation was prepared");
        assert!(refused > 0, "no mutation was refused");
    }
}
