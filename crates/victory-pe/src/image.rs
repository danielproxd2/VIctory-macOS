use object::LittleEndian as LE;
use object::Object;
use object::pe::{self, ImageNtHeaders64};
use object::read::pe::{
    ImageNtHeaders, ImageOptionalHeader, Import, ImportTable, ImportThunkList, PeFile64,
};

use crate::layout::{self, dir};
use crate::{MACHINE_AMD64, PeError};

/// Everything we need to know about a PE32+ image, extracted in one pass.
#[derive(Debug, Clone)]
pub struct ImageInfo {
    pub machine: u16,
    /// `IMAGE_FILE_HEADER::Characteristics`.
    pub characteristics: u16,
    pub timestamp: u32,
    pub subsystem: u16,
    /// `IMAGE_OPTIONAL_HEADER64::DllCharacteristics`.
    pub dll_characteristics: u16,
    pub image_base: u64,
    pub size_of_image: u32,
    pub entry_rva: u32,
    pub stack_reserve: u64,
    pub stack_commit: u64,
    pub sections: Vec<SectionInfo>,
    pub imports: Vec<ImportModule>,
    pub delay_imports: Vec<ImportModule>,
    pub exports: Vec<ExportInfo>,
    pub tls: Option<TlsInfo>,
    pub has_relocations: bool,
    pub has_exception_table: bool,
    pub has_load_config: bool,
    pub codeview: Option<CodeViewInfo>,
}

#[derive(Debug, Clone)]
pub struct SectionInfo {
    pub name: String,
    pub rva: u32,
    pub virtual_size: u32,
    pub file_offset: u32,
    pub file_size: u32,
    pub characteristics: u32,
}

/// One imported DLL and the functions taken from it.
#[derive(Debug, Clone)]
pub struct ImportModule {
    /// DLL name exactly as written in the image (case preserved).
    pub dll: String,
    pub entries: Vec<ImportEntry>,
    /// Delay imports only: RVA of the 8-byte slot in which the delay-load
    /// helper caches the module's `HMODULE` (`ModuleHandleRVA`). `None` for
    /// eager imports and for a descriptor without one. The slot is checked
    /// to lie inside the image, not to be aligned.
    pub module_handle_rva: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct ImportEntry {
    pub symbol: ImportSymbol,
    /// RVA of the 8-byte IAT slot the loader must fill in.
    pub iat_slot_rva: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ImportSymbol {
    Name { hint: u16, name: String },
    Ordinal(u16),
}

impl std::fmt::Display for ImportSymbol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportSymbol::Name { name, .. } => f.write_str(name),
            ImportSymbol::Ordinal(o) => write!(f, "#{o}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExportInfo {
    pub name: Option<String>,
    pub ordinal: u16,
    /// `None` for forwarded exports.
    pub rva: Option<u32>,
    /// `"dll.name"` or `"dll.#ordinal"` for forwarded exports.
    pub forward: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TlsInfo {
    /// RVA of `IMAGE_TLS_DIRECTORY64`.
    pub directory_rva: u32,
    /// VA (at the preferred base) of the callback array, 0 if none.
    pub callbacks_va: u64,
    /// Callback RVAs, in call order.
    pub callback_rvas: Vec<u32>,
}

/// CodeView (RSDS) record: identifies the exact build that produced an image.
#[derive(Debug, Clone)]
pub struct CodeViewInfo {
    pub pdb_path: String,
    pub guid: [u8; 16],
    pub age: u32,
}

impl CodeViewInfo {
    /// The key symbol servers use: GUID (Windows byte order) + age.
    pub fn symbol_key(&self) -> String {
        let g = &self.guid;
        let d1 = u32::from_le_bytes([g[0], g[1], g[2], g[3]]);
        let d2 = u16::from_le_bytes([g[4], g[5]]);
        let d3 = u16::from_le_bytes([g[6], g[7]]);
        let mut s = format!("{d1:08X}{d2:04X}{d3:04X}");
        for b in &g[8..] {
            s.push_str(&format!("{b:02X}"));
        }
        s.push_str(&format!("{:X}", self.age));
        s
    }
}

/// Upper bound on TLS callbacks we are willing to walk (defends against
/// garbage). An array with more is refused, not cut short.
const MAX_TLS_CALLBACKS: usize = 1024;

impl ImageInfo {
    /// Parses a complete PE32+ x86-64 image from its file bytes.
    pub fn parse(data: &[u8]) -> Result<Self, PeError> {
        let kind = object::FileKind::parse(data)?;
        if kind != object::FileKind::Pe64 {
            return Err(PeError::Unsupported(format!(
                "expected PE32+, found {kind:?}"
            )));
        }
        let file = PeFile64::parse(data)?;
        let nt = file.nt_headers();
        let fh = nt.file_header();
        let opt = nt.optional_header();

        let machine = fh.machine.get(LE).0;
        if machine != MACHINE_AMD64 {
            return Err(PeError::Unsupported(format!(
                "machine 0x{machine:04x} is not AMD64"
            )));
        }

        let sections: Vec<SectionInfo> = file
            .section_table()
            .iter()
            .map(|s| SectionInfo {
                name: String::from_utf8_lossy(s.raw_name()).into_owned(),
                rva: s.virtual_address.get(LE),
                virtual_size: s.virtual_size.get(LE),
                file_offset: s.pointer_to_raw_data.get(LE),
                file_size: s.size_of_raw_data.get(LE),
                characteristics: s.characteristics.get(LE).0,
            })
            .collect();

        // Validate section raw-data boundaries before accepting
        // the PE image.
        for section in &sections {
            if section.file_size == 0 {
                continue;
            }

            let start = u64::from(section.file_offset);
            let size = u64::from(section.file_size);

            let Some(end) = start.checked_add(size) else {
                return Err(PeError::Malformed(format!(
                    "section '{}' has invalid raw-data bounds",
                    section.name
                )));
            };

            if end > data.len() as u64 {
                return Err(PeError::Malformed(format!(
                    "section '{}' extends beyond file bounds",
                    section.name
                )));
            }
        }

        // 0 means the image has no entry point. Anything else is called.
        let entry_rva = opt.address_of_entry_point();
        if entry_rva != 0 && !is_code(&sections, opt.size_of_image(), entry_rva) {
            return Err(PeError::Malformed(format!(
                "entry point at {entry_rva:#x} is not in an executable section"
            )));
        }

        let dir_present = |index: usize| {
            file.data_directory(index)
                .is_some_and(|d| d.virtual_address.get(LE) != 0 && d.size.get(LE) != 0)
        };

        let imports = match file.import_table()? {
            Some(table) => parse_imports(&table, opt.size_of_image())?,
            None => Vec::new(),
        };
        let delay_imports = parse_delay_imports(&file, opt.size_of_image())?;
        let exports = parse_exports(&file, opt.image_base())?;
        let tls = parse_tls(
            &file,
            data,
            opt.image_base(),
            opt.size_of_image(),
            &sections,
        )?;
        let codeview = file.pdb_info()?.map(|cv| CodeViewInfo {
            pdb_path: String::from_utf8_lossy(cv.path()).into_owned(),
            guid: cv.guid(),
            age: cv.age(),
        });

        Ok(ImageInfo {
            machine,
            characteristics: fh.characteristics.get(LE).0,
            timestamp: fh.time_date_stamp.get(LE),
            subsystem: opt.subsystem().0,
            dll_characteristics: opt.dll_characteristics().0,
            image_base: opt.image_base(),
            size_of_image: opt.size_of_image(),
            entry_rva,
            stack_reserve: opt.size_of_stack_reserve(),
            stack_commit: opt.size_of_stack_commit(),
            sections,
            imports,
            delay_imports,
            exports,
            tls,
            has_relocations: dir_present(dir::BASERELOC)
                && !has_flag(
                    fh.characteristics.get(LE).0,
                    layout::file_flags::RELOCS_STRIPPED,
                ),
            has_exception_table: dir_present(dir::EXCEPTION),
            has_load_config: dir_present(dir::LOAD_CONFIG),
            codeview,
        })
    }

    pub fn is_dll(&self) -> bool {
        has_flag(self.characteristics, layout::file_flags::DLL)
    }

    /// Maps an RVA to an offset in the file, if it is backed by file data.
    pub fn rva_to_file_offset(&self, rva: u32) -> Option<u32> {
        self.sections.iter().find_map(|s| {
            let delta = rva.checked_sub(s.rva)?;
            // Raw data may be padded past the virtual size; that padding is
            // not part of the section in memory.
            let backed = match s.virtual_size {
                0 => s.file_size,
                vs => vs.min(s.file_size),
            };
            // Header values are untrusted: an offset near 4 GiB overflows.
            (delta < backed).then(|| s.file_offset.checked_add(delta))?
        })
    }

    pub fn import_count(&self) -> usize {
        self.imports.iter().map(|m| m.entries.len()).sum()
    }

    pub fn subsystem_name(&self) -> &'static str {
        use layout::subsystem::*;
        match self.subsystem {
            WINDOWS_GUI => "windows-gui",
            WINDOWS_CUI => "windows-cui",
            XBOX => "xbox",
            XBOX_CODE_CATALOG => "xbox-code-catalog",
            _ => "other",
        }
    }
}

fn has_flag(value: u16, flag: u16) -> bool {
    value & flag != 0
}

/// Whether the image may run code at `rva`: the address is inside the image
/// and inside a section that is mapped executable. Addresses from the
/// headers that we later call are checked with this, so a damaged header is
/// an error with a cause and not a jump to a wild address.
fn is_code(sections: &[SectionInfo], size_of_image: u32, rva: u32) -> bool {
    rva < size_of_image
        && sections.iter().any(|s| {
            // As in `rva_to_file_offset`: without a virtual size, the raw
            // size is the section's size.
            let size = match s.virtual_size {
                0 => s.file_size,
                vs => vs,
            };
            s.characteristics & layout::section_flags::MEM_EXECUTE != 0
                && rva.checked_sub(s.rva).is_some_and(|delta| delta < size)
        })
}

fn utf8(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn convert_import(import: Import<'_>) -> ImportSymbol {
    match import {
        Import::Ordinal(o) => ImportSymbol::Ordinal(o),
        Import::Name(hint, name) => ImportSymbol::Name {
            hint,
            name: utf8(name),
        },
    }
}

fn parse_imports(
    table: &ImportTable<'_>,
    size_of_image: u32,
) -> Result<Vec<ImportModule>, PeError> {
    let mut modules = Vec::new();
    let mut descriptors = table.descriptors()?;
    while let Some(desc) = descriptors.next()? {
        let iat_rva = desc.first_thunk.get(LE);
        // Prefer the unbound name table; fall back to the IAT for old linkers.
        let int_rva = match desc.original_first_thunk.get(LE) {
            0 => iat_rva,
            rva => rva,
        };
        modules.push(ImportModule {
            dll: utf8(table.name(desc.name.get(LE))?),
            entries: entries(table.thunks(int_rva)?, iat_rva, size_of_image, |t| {
                table.import::<ImageNtHeaders64>(t)
            })?,
            module_handle_rva: None,
        });
    }
    Ok(modules)
}

fn parse_delay_imports(
    file: &PeFile64<'_>,
    size_of_image: u32,
) -> Result<Vec<ImportModule>, PeError> {
    let Some(table) = file.delay_load_import_table()? else {
        return Ok(Vec::new());
    };
    let mut modules = Vec::new();
    let mut descriptors = table.descriptors()?;
    while let Some(desc) = descriptors.next()? {
        let dll = utf8(table.name(desc.dll_name_rva.get(LE))?);
        // Without this attribute the descriptor's fields are virtual
        // addresses, the layout of old linkers. Read as RVAs they would
        // name other places in the image, or none.
        if desc.attributes.get(LE) & pe::IMAGE_DELAYLOAD_RVA_BASED == 0 {
            return Err(PeError::Unsupported(format!(
                "delay-load descriptor for {dll} holds virtual addresses, not RVAs"
            )));
        }
        let module_handle_rva = match desc.module_handle_rva.get(LE) {
            0 => None,
            rva if rva.checked_add(8).is_some_and(|end| end <= size_of_image) => Some(rva),
            rva => {
                return Err(PeError::Malformed(format!(
                    "delay-load module handle at {rva:#x} is outside the image"
                )));
            }
        };
        modules.push(ImportModule {
            dll,
            entries: entries(
                table.thunks(desc.import_name_table_rva.get(LE))?,
                desc.import_address_table_rva.get(LE),
                size_of_image,
                |t| table.import::<ImageNtHeaders64>(t),
            )?,
            module_handle_rva,
        });
    }
    Ok(modules)
}

/// Pairs each name-table entry with its IAT slot. The IAT RVA is not checked
/// by `object`, so every slot is checked here: it lies inside the image, and
/// on an 8-byte boundary, because binding stores a pointer through it.
fn entries<'data>(
    mut thunks: ImportThunkList<'data>,
    iat_rva: u32,
    size_of_image: u32,
    import: impl Fn(pe::ImageThunkData64) -> object::read::Result<Import<'data>>,
) -> Result<Vec<ImportEntry>, PeError> {
    if iat_rva % 8 != 0 {
        return Err(PeError::Malformed(format!(
            "IAT at {iat_rva:#x} is not 8-byte aligned"
        )));
    }
    let mut entries = Vec::new();
    while let Some(thunk) = thunks.next::<ImageNtHeaders64>()? {
        let iat_slot_rva = u32::try_from(entries.len() * 8)
            .ok()
            .and_then(|offset| iat_rva.checked_add(offset))
            .filter(|&rva| rva.checked_add(8).is_some_and(|end| end <= size_of_image))
            .ok_or_else(|| {
                PeError::Malformed(format!("IAT at {iat_rva:#x} runs outside the image"))
            })?;
        entries.push(ImportEntry {
            symbol: convert_import(import(thunk)?),
            iat_slot_rva,
        });
    }
    Ok(entries)
}

fn parse_exports(file: &PeFile64<'_>, image_base: u64) -> Result<Vec<ExportInfo>, PeError> {
    use object::read::{ExportFlags, ExportTarget as ReadTarget, NameOrOrdinal};

    // The PE-specific table keeps one name per address-table slot, so two
    // names sharing an ordinal lose all but the last one. The generic
    // iterator yields every name, then the ordinal-only entries.
    file.exports()
        .map_err(|e| PeError::Malformed(format!("export table: {e}")))?
        .map(|e| {
            let e = e.map_err(|e| PeError::Malformed(format!("export: {e}")))?;
            let ordinal = match e.flags() {
                ExportFlags::Pe { ordinal } => ordinal.0,
                _ => 0,
            };
            let name = match e.name() {
                NameOrOrdinal::Name(name) => Some(utf8(name)),
                NameOrOrdinal::Ordinal(_) => None,
            };
            let (rva, forward) = match e.target() {
                // The generic iterator adds the image base to an address.
                ReadTarget::Address { address } => {
                    (Some(address.wrapping_sub(image_base) as u32), None)
                }
                ReadTarget::Reexport { library, name } => {
                    let symbol = match name {
                        NameOrOrdinal::Name(name) => utf8(name),
                        NameOrOrdinal::Ordinal(ordinal) => format!("#{ordinal}"),
                    };
                    (None, Some(format!("{}.{}", utf8(library), symbol)))
                }
                _ => (None, None),
            };
            Ok(ExportInfo {
                name,
                ordinal,
                rva,
                forward,
            })
        })
        .collect()
}

fn parse_tls(
    file: &PeFile64<'_>,
    data: &[u8],
    image_base: u64,
    size_of_image: u32,
    image_sections: &[SectionInfo],
) -> Result<Option<TlsInfo>, PeError> {
    let Some(dd) = file.data_directory(pe::IMAGE_DIRECTORY_ENTRY_TLS) else {
        return Ok(None);
    };
    let directory_rva = dd.virtual_address.get(LE);
    if directory_rva == 0 {
        return Ok(None);
    }
    let sections = file.section_table();
    let tls = sections
        .pe_data_at(data, directory_rva)
        .ok_or_else(|| PeError::Malformed("TLS directory outside of sections".into()))?;
    let callbacks_va = layout::read_u64(tls, layout::TLS64_ADDRESS_OF_CALLBACKS)
        .ok_or_else(|| PeError::Malformed("truncated TLS directory".into()))?;

    let mut callback_rvas = Vec::new();
    if callbacks_va != 0 {
        let outside = || PeError::Malformed("TLS callback array outside of sections".into());
        let array_rva = va_to_rva(callbacks_va, image_base)?;
        let section = sections
            .iter()
            .find(|s| s.pe_file_range_at(array_rva).is_some())
            .ok_or_else(outside)?;
        // The array's bytes in the file, up to the end of its section.
        let array = section.pe_data_at(data, array_rva).ok_or_else(outside)?;
        // What the section holds in memory from the array on. Past the file
        // data that is zero fill, which may be the array's terminator.
        let in_memory = section
            .virtual_size
            .get(LE)
            .saturating_sub(array_rva.saturating_sub(section.virtual_address.get(LE)));
        // Every exit of the loop is a terminator or an error: a list is
        // never cut short, because the callbacks after the cut would
        // silently not run.
        for start in (0usize..).step_by(8) {
            let in_section = u32::try_from(start)
                .ok()
                .and_then(|start| start.checked_add(8))
                .is_some_and(|end| end <= in_memory);
            if !in_section {
                return Err(PeError::Malformed(
                    "TLS callback array has no null terminator in its section".into(),
                ));
            }
            let mut slot = [0u8; 8];
            let file = array.get(start..).unwrap_or(&[]);
            let backed = file.len().min(8);
            slot[..backed].copy_from_slice(&file[..backed]);
            let va = u64::from_le_bytes(slot);
            if va == 0 {
                break;
            }
            if callback_rvas.len() == MAX_TLS_CALLBACKS {
                return Err(PeError::Malformed(format!(
                    "more than {MAX_TLS_CALLBACKS} TLS callbacks"
                )));
            }
            let rva = va_to_rva(va, image_base)?;
            if !is_code(image_sections, size_of_image, rva) {
                return Err(PeError::Malformed(format!(
                    "TLS callback at {rva:#x} is not in an executable section"
                )));
            }
            callback_rvas.push(rva);
        }
    }
    Ok(Some(TlsInfo {
        directory_rva,
        callbacks_va,
        callback_rvas,
    }))
}

fn va_to_rva(va: u64, image_base: u64) -> Result<u32, PeError> {
    va.checked_sub(image_base)
        .and_then(|rva| u32::try_from(rva).ok())
        .ok_or_else(|| PeError::Malformed(format!("VA 0x{va:x} is outside the image")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{DELAYLOAD_RVA_BASED, Export, PeBuilder, XorShift64, mutate, reloc, scn};

    fn by_name(hint: u16, name: &str) -> ImportSymbol {
        ImportSymbol::Name {
            hint,
            name: name.to_owned(),
        }
    }

    /// An executable shaped like a linked one: code, an exception table,
    /// imports from two DLLs and a TLS directory. `.text` is at RVA 0x1000
    /// and `.pdata` at 0x2000.
    fn sample() -> PeBuilder {
        PeBuilder::new()
            .entry_rva(0x1010)
            .section(".text", scn::CODE, &[0xC3; 0x40])
            .section(".pdata", scn::RDATA, &[0; 12])
            .data_directory(dir::EXCEPTION, 0x2000, 12)
            .import(
                "alpha.dll",
                &[by_name(3, "First"), ImportSymbol::Ordinal(7)],
            )
            .import("beta.dll", &[by_name(0, "Second")])
            .tls_without_callbacks()
    }

    fn sample_image() -> Vec<u8> {
        sample().build().unwrap().bytes
    }

    /// The test binary itself is a real linked PE32+ image.
    #[cfg(windows)]
    #[test]
    fn parses_own_test_binary() {
        let data = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let info = ImageInfo::parse(&data).unwrap();
        assert_eq!(info.machine, MACHINE_AMD64);
        assert!(!info.is_dll());
        assert!(info.entry_rva != 0);
        assert!(info.has_exception_table, "x64 images always carry .pdata");
        assert!(info.sections.iter().any(|s| s.name.starts_with(".text")));
        let kernel32 = info
            .imports
            .iter()
            .find(|m| m.dll.eq_ignore_ascii_case("kernel32.dll"))
            .expect("Rust std imports kernel32");
        assert!(kernel32.entries.iter().all(|e| e.iat_slot_rva != 0));
        // Rust std uses static TLS, so a TLS directory must be present (its
        // callback array may legitimately be empty).
        assert!(info.tls.is_some());
        let off = info.rva_to_file_offset(info.entry_rva).unwrap() as usize;
        assert!(off < data.len());
    }

    #[test]
    fn parses_synthetic_executable() {
        let guid: [u8; 16] = std::array::from_fn(|i| 0xA0 + i as u8);
        let data = sample()
            .timestamp(0x1234_5678)
            .stack_reserve(0x20_0000)
            .codeview(guid, 2, "sample.pdb")
            .build()
            .unwrap()
            .bytes;
        let info = ImageInfo::parse(&data).unwrap();
        assert_eq!(info.machine, MACHINE_AMD64);
        assert!(!info.is_dll());
        assert_eq!(info.timestamp, 0x1234_5678);
        assert_eq!(info.entry_rva, 0x1010);
        assert_eq!(info.image_base, crate::testing::DEFAULT_IMAGE_BASE);
        assert_eq!(info.stack_reserve, 0x20_0000);
        assert_eq!(info.subsystem_name(), "windows-cui");
        assert!(info.has_exception_table);
        assert!(!info.has_relocations);
        assert!(!info.has_load_config);
        let text = &info.sections[0];
        assert!(text.name.starts_with(".text"));
        assert_eq!((text.rva, text.virtual_size), (0x1000, 0x40));
        assert_eq!(text.characteristics, scn::CODE);
        let last = info.sections.last().unwrap();
        assert!(last.rva + last.virtual_size <= info.size_of_image);
        let cv = info.codeview.expect("an RSDS record was added");
        assert_eq!((cv.guid, cv.age), (guid, 2));
        assert_eq!(cv.pdb_path, "sample.pdb");

        let dll = PeBuilder::new().dll(true).build().unwrap().bytes;
        assert!(ImageInfo::parse(&dll).unwrap().is_dll());
    }

    #[test]
    fn rejects_other_machines() {
        // IMAGE_FILE_MACHINE_ARM64.
        let data = sample().machine(0xAA64).build().unwrap().bytes;
        assert!(matches!(
            ImageInfo::parse(&data),
            Err(PeError::Unsupported(_))
        ));
    }

    #[test]
    fn parses_synthetic_imports_by_name_and_ordinal() {
        let data = sample_image();
        let info = ImageInfo::parse(&data).unwrap();
        assert_eq!(info.import_count(), 3);
        let modules: Vec<_> = info.imports.iter().map(|m| m.dll.as_str()).collect();
        assert_eq!(modules, ["alpha.dll", "beta.dll"]);
        let alpha: Vec<_> = info.imports[0].entries.iter().map(|e| &e.symbol).collect();
        assert_eq!(alpha, [&by_name(3, "First"), &ImportSymbol::Ordinal(7)]);
        assert_eq!(info.imports[1].entries[0].symbol, by_name(0, "Second"));

        // Consecutive 8-byte slots per module, inside the IAT directory,
        // each still holding its name-table value in the file.
        let slots = &info.imports[0].entries;
        assert_eq!(slots[1].iat_slot_rva, slots[0].iat_slot_rva + 8);
        let iat = layout::data_directory_offset(&data, dir::IAT).unwrap();
        let iat_rva = layout::read_u32(&data, iat).unwrap();
        let iat_size = layout::read_u32(&data, iat + 4).unwrap();
        for entry in info.imports.iter().flat_map(|m| &m.entries) {
            assert!(entry.iat_slot_rva >= iat_rva);
            assert!(entry.iat_slot_rva + 8 <= iat_rva + iat_size);
        }
        let at = info.rva_to_file_offset(slots[1].iat_slot_rva).unwrap() as usize;
        assert_eq!(layout::read_u64(&data, at), Some(1 << 63 | 7));
    }

    #[test]
    fn parses_synthetic_delay_imports() {
        let data = sample()
            .delay_import(
                "late.dll",
                DELAYLOAD_RVA_BASED,
                &[by_name(1, "Later"), ImportSymbol::Ordinal(9)],
            )
            .build()
            .unwrap()
            .bytes;
        let info = ImageInfo::parse(&data).unwrap();
        // Delay imports are kept apart from the eager ones.
        assert_eq!(info.import_count(), 3);
        assert_eq!(info.delay_imports.len(), 1);
        let late = &info.delay_imports[0];
        assert_eq!(late.dll, "late.dll");
        let symbols: Vec<_> = late.entries.iter().map(|e| &e.symbol).collect();
        assert_eq!(symbols, [&by_name(1, "Later"), &ImportSymbol::Ordinal(9)]);
        assert_eq!(
            late.entries[1].iat_slot_rva,
            late.entries[0].iat_slot_rva + 8
        );
        // The slots are those of the descriptor's ImportAddressTableRVA
        // (offset 12 of IMAGE_DELAYLOAD_DESCRIPTOR), read from the file,
        // and not of the name table that follows it at offset 16.
        let directory = layout::data_directory_offset(&data, dir::DELAY_IMPORT).unwrap();
        let descriptor_rva = layout::read_u32(&data, directory).unwrap();
        let descriptor = info.rva_to_file_offset(descriptor_rva).unwrap() as usize;
        let iat = layout::read_u32(&data, descriptor + 12).unwrap();
        let names = layout::read_u32(&data, descriptor + 16).unwrap();
        assert_ne!(iat, 0);
        assert_ne!(iat, names, "the two tables are apart in this image");
        let slots: Vec<_> = late.entries.iter().map(|e| e.iat_slot_rva).collect();
        assert_eq!(slots, [iat, iat + 8]);
        // The name table holds the two thunks; the IAT is not a copy of it.
        let names_at = info.rva_to_file_offset(names).unwrap() as usize;
        assert_eq!(
            layout::read_u64(&data, names_at + 8),
            Some(1 << 63 | 9),
            "the ordinal import's entry in the name table"
        );
        // The delay IAT is its own table, not part of the eager IAT.
        let eager: Vec<_> = info
            .imports
            .iter()
            .flat_map(|m| &m.entries)
            .map(|e| e.iat_slot_rva)
            .collect();
        assert!(
            late.entries
                .iter()
                .all(|e| !eager.contains(&e.iat_slot_rva))
        );
    }

    #[test]
    fn parses_synthetic_tls_callbacks() {
        let built = PeBuilder::new()
            .section(".text", scn::CODE, &[0xC3; 0x40])
            .tls_callbacks(&[0x1000, 0x1020])
            .build()
            .unwrap();
        let info = ImageInfo::parse(&built.bytes).unwrap();
        let tls = info.tls.expect("a TLS directory was added");
        assert_eq!(Some(tls.directory_rva), built.tls_directory_rva);
        assert_eq!(tls.callback_rvas, [0x1000, 0x1020]);
        let array = tls.callbacks_va - info.image_base;
        assert!(array < u64::from(info.size_of_image));

        // A directory without a callback array is still a TLS directory.
        let info = ImageInfo::parse(&sample_image()).unwrap();
        let tls = info.tls.expect("a TLS directory was added");
        assert_eq!(tls.callbacks_va, 0);
        assert!(tls.callback_rvas.is_empty());

        let none = PeBuilder::new().build().unwrap().bytes;
        assert!(ImageInfo::parse(&none).unwrap().tls.is_none());
    }

    #[test]
    fn rejects_tls_callbacks_below_the_image_base() {
        let built = code().tls_callbacks(&[0x1000]).build().unwrap();
        let mut data = built.bytes;
        let info = ImageInfo::parse(&data).unwrap();
        let tls = info.tls.as_ref().unwrap();
        let array = u32::try_from(tls.callbacks_va - info.image_base).unwrap();
        let at = info.rva_to_file_offset(array).unwrap() as usize;
        data[at..at + 8].copy_from_slice(&(info.image_base - 1).to_le_bytes());
        assert!(matches!(
            ImageInfo::parse(&data),
            Err(PeError::Malformed(_))
        ));
    }

    /// `.text` at RVA 0x1000 and `.data` at 0x2000, 0x40 bytes each.
    fn code() -> PeBuilder {
        PeBuilder::new()
            .section(".text", scn::CODE, &[0xC3; 0x40])
            .section(".data", scn::DATA, &[1; 0x40])
    }

    #[test]
    fn rejects_tls_callback_outside_image() {
        let ok = code().tls_callbacks(&[0x1000, 0x103F]).build().unwrap();
        let info = ImageInfo::parse(&ok.bytes).unwrap();
        assert_eq!(info.tls.unwrap().callback_rvas, [0x1000, 0x103F]);
        let size_of_image = info.size_of_image;
        // Past the end of the image, at its very end, and in the gap
        // between `.text`'s last byte and the next section.
        for rva in [0x10_0000, size_of_image, 0x1040, 0x1FFF] {
            let data = code().tls_callbacks(&[0x1000, rva]).build().unwrap().bytes;
            let why = malformed(ImageInfo::parse(&data));
            assert_eq!(
                why,
                format!("TLS callback at {rva:#x} is not in an executable section")
            );
        }
        // An executable section whose header reaches past SizeOfImage does
        // not make an address out there part of the image.
        let mut data = code()
            .tls_callbacks(&[0x1000, size_of_image + 8])
            .build()
            .unwrap()
            .bytes;
        let text = crate::testing::section_header_offset(&data, 0).unwrap();
        data[text + 8..text + 12].copy_from_slice(&0x10_0000u32.to_le_bytes());
        let why = malformed(ImageInfo::parse(&data));
        assert!(why.contains("not in an executable section"), "{why}");
    }

    /// [`sample`] with one delay-loaded DLL, and the file offset of its
    /// `IMAGE_DELAYLOAD_DESCRIPTOR`.
    fn delay_sample(attributes: u32) -> (Vec<u8>, usize) {
        let data = sample()
            .delay_import("late.dll", attributes, &[by_name(1, "Later")])
            .build()
            .unwrap()
            .bytes;
        let directory = layout::data_directory_offset(&data, dir::DELAY_IMPORT).unwrap();
        let descriptor_rva = layout::read_u32(&data, directory).unwrap();
        // The delay section is the fourth, after `.text`, `.pdata` and
        // `.idata`, and starts with the descriptor table. Its place is read
        // from the section header, because an image with attributes 0 no
        // longer parses.
        let header = crate::testing::section_header_offset(&data, 3).unwrap();
        assert_eq!(&data[header..header + 6], b".didat");
        assert_eq!(layout::read_u32(&data, header + 12), Some(descriptor_rva));
        let at = layout::read_u32(&data, header + 20).unwrap() as usize;
        (data, at)
    }

    #[test]
    fn rejects_va_based_delay_descriptor() {
        // Attributes 0: the fields would be virtual addresses.
        let (data, at) = delay_sample(0);
        assert_eq!(layout::read_u32(&data, at), Some(0));
        match ImageInfo::parse(&data) {
            Err(PeError::Unsupported(why)) => assert_eq!(
                why,
                "delay-load descriptor for late.dll holds virtual addresses, not RVAs"
            ),
            other => panic!("expected PeError::Unsupported, got {other:?}"),
        }
        // Other attribute bits do not stand in for the RVA one.
        let (mut data, at) = delay_sample(DELAYLOAD_RVA_BASED);
        assert!(ImageInfo::parse(&data).is_ok());
        data[at..at + 4].copy_from_slice(&0xFFFF_FFFEu32.to_le_bytes());
        assert!(matches!(
            ImageInfo::parse(&data),
            Err(PeError::Unsupported(_))
        ));
    }

    #[test]
    fn delay_descriptor_reports_module_handle_slot() {
        let (mut data, at) = delay_sample(DELAYLOAD_RVA_BASED);
        let handle = layout::read_u32(&data, at + 8).unwrap();
        let info = ImageInfo::parse(&data).unwrap();
        let late = &info.delay_imports[0];
        assert_ne!(handle, 0);
        assert_eq!(late.module_handle_rva, Some(handle));
        // The slot is its own: not a slot of the delay IAT.
        assert!(late.entries.iter().all(|e| e.iat_slot_rva != handle));
        assert!(handle + 8 <= late.entries[0].iat_slot_rva);
        // Eager imports have no such slot.
        assert!(info.imports.iter().all(|m| m.module_handle_rva.is_none()));

        // A descriptor without a slot is allowed and says so.
        data[at + 8..at + 12].copy_from_slice(&0u32.to_le_bytes());
        let info = ImageInfo::parse(&data).unwrap();
        assert_eq!(info.delay_imports[0].module_handle_rva, None);
        // A slot that does not fit in the image is an error.
        let last = info.size_of_image - 8;
        data[at + 8..at + 12].copy_from_slice(&last.to_le_bytes());
        let info = ImageInfo::parse(&data).unwrap();
        assert_eq!(info.delay_imports[0].module_handle_rva, Some(last));
        for rva in [last + 1, info.size_of_image, 0xFFFF_FFF9] {
            data[at + 8..at + 12].copy_from_slice(&rva.to_le_bytes());
            let why = malformed(ImageInfo::parse(&data));
            assert_eq!(
                why,
                format!("delay-load module handle at {rva:#x} is outside the image")
            );
        }
    }

    #[test]
    fn rejects_entry_outside_code() {
        let entry = |rva| ImageInfo::parse(&code().entry_rva(rva).build().unwrap().bytes);
        assert_eq!(entry(0x1000).unwrap().entry_rva, 0x1000);
        assert_eq!(entry(0x103F).unwrap().entry_rva, 0x103F);
        // No entry point at all is not an error (a resource-only DLL).
        assert_eq!(entry(0).unwrap().entry_rva, 0);
        let size_of_image = entry(0).unwrap().size_of_image;
        // In `.data`, in the headers, just past `.text`, at the end of the
        // image and far outside it.
        for rva in [0x2010, 0x80, 0x1040, size_of_image, 0xFFFF_FFF0] {
            let why = malformed(entry(rva));
            assert_eq!(
                why,
                format!("entry point at {rva:#x} is not in an executable section")
            );
        }
        // A DLL's entry point is called too.
        let dll = code().dll(true).entry_rva(0x2000).build().unwrap().bytes;
        let why = malformed(ImageInfo::parse(&dll));
        assert_eq!(why, "entry point at 0x2000 is not in an executable section");
    }

    #[test]
    fn rejects_tls_callback_in_data_section() {
        // `.data` is mapped, but not executable.
        let data = code()
            .tls_callbacks(&[0x1000, 0x2010])
            .build()
            .unwrap()
            .bytes;
        let why = malformed(ImageInfo::parse(&data));
        assert_eq!(
            why,
            "TLS callback at 0x2010 is not in an executable section"
        );
        // The headers are not a section at all.
        let data = code().tls_callbacks(&[0x80]).build().unwrap().bytes;
        let why = malformed(ImageInfo::parse(&data));
        assert_eq!(why, "TLS callback at 0x80 is not in an executable section");
    }

    #[test]
    fn rejects_1025_tls_callbacks() {
        let most = vec![0x1000u32; MAX_TLS_CALLBACKS];
        let data = code().tls_callbacks(&most).build().unwrap().bytes;
        let tls = ImageInfo::parse(&data).unwrap().tls.unwrap();
        assert_eq!(tls.callback_rvas, most);
        // One more used to be dropped without a word.
        let one_more = vec![0x1000u32; MAX_TLS_CALLBACKS + 1];
        let data = code().tls_callbacks(&one_more).build().unwrap().bytes;
        let why = malformed(ImageInfo::parse(&data));
        assert_eq!(why, "more than 1024 TLS callbacks");
    }

    #[test]
    fn rejects_unterminated_tls_callbacks() {
        let built = code().tls_callbacks(&[0x1000, 0x1020]).build().unwrap();
        let info = ImageInfo::parse(&built.bytes).unwrap();
        let array = built.tls_callbacks_rva.unwrap();
        let terminator = info.rva_to_file_offset(array).unwrap() as usize + 16;
        assert_eq!(layout::read_u64(&built.bytes, terminator), Some(0));
        // The `.tls` section is the third; the array is the last thing in
        // it, so its terminator is the section's last 8 bytes.
        let header = crate::testing::section_header_offset(&built.bytes, 2).unwrap();
        let virtual_size = layout::read_u32(&built.bytes, header + 8).unwrap();
        assert_eq!(array + 24, info.sections[2].rva + virtual_size);

        // A callback where the terminator was: the array runs into the end
        // of its section. It used to be read as three callbacks.
        let mut data = built.bytes.clone();
        let callback = (info.image_base + 0x1000).to_le_bytes();
        data[terminator..terminator + 8].copy_from_slice(&callback);
        let why = malformed(ImageInfo::parse(&data));
        assert_eq!(
            why,
            "TLS callback array has no null terminator in its section"
        );
        // The same when the section ends in the middle of a slot.
        let mut data = built.bytes.clone();
        data[header + 8..header + 12].copy_from_slice(&(virtual_size - 4).to_le_bytes());
        let why = malformed(ImageInfo::parse(&data));
        assert_eq!(
            why,
            "TLS callback array has no null terminator in its section"
        );

        // A terminator that is not in the file but in the section's zero
        // fill is a terminator: the raw data ends after the second callback.
        let mut data = built.bytes.clone();
        data[terminator..terminator + 8].copy_from_slice(&callback);
        data[header + 16..header + 20].copy_from_slice(&(virtual_size - 8).to_le_bytes());
        let tls = ImageInfo::parse(&data).unwrap().tls.unwrap();
        assert_eq!(tls.callback_rvas, [0x1000, 0x1020]);
        // Raw data that ends inside a slot: the rest of the slot is zero.
        // The one byte of the third slot that is left is the 0x00 its
        // callback address starts with.
        assert_eq!(callback[0], 0);
        data[header + 16..header + 20].copy_from_slice(&(virtual_size - 7).to_le_bytes());
        let tls = ImageInfo::parse(&data).unwrap().tls.unwrap();
        assert_eq!(tls.callback_rvas, [0x1000, 0x1020]);
    }

    #[test]
    fn parses_synthetic_exports_with_forwarder() {
        let data = PeBuilder::new()
            .dll(true)
            .section(".text", scn::CODE, &[0xC3; 0x40])
            .exports(
                "t.dll",
                5,
                vec![
                    Export::named("gamma", 0x1010),
                    Export::ordinal_only(0x1020),
                    Export::forwarder("alpha", "other.Target"),
                    Export::forwarder("beta", "other.#12"),
                ],
            )
            .build()
            .unwrap()
            .bytes;
        let info = ImageInfo::parse(&data).unwrap();
        let mut exports: Vec<_> = info
            .exports
            .iter()
            .map(|e| (e.ordinal, e.name.as_deref(), e.rva, e.forward.as_deref()))
            .collect();
        exports.sort();
        assert_eq!(
            exports,
            [
                (5, Some("gamma"), Some(0x1010), None),
                (6, None, Some(0x1020), None),
                (7, Some("alpha"), None, Some("other.Target")),
                (8, Some("beta"), None, Some("other.#12")),
            ]
        );
    }

    #[test]
    fn rejects_section_extending_beyond_file() {
        // Create a synthetic PE image.
        let mut data = synthetic_image(&vec![0u8; 0x200]);

        // Remove the export directory to isolate section validation.
        let opt = 0x58;
        data[opt + 112..opt + 120].fill(0);

        // Verify the baseline PE is accepted.
        assert!(
            ImageInfo::parse(&data).is_ok(),
            "baseline synthetic image must be valid"
        );

        // The section header starts after the 240-byte optional header.
        let sec = opt + 240;

        // Declare 4096 bytes of raw section data.
        // The actual file contains only 1024 bytes.
        data[sec + 16..sec + 20].copy_from_slice(&0x1000u32.to_le_bytes());

        // Investigate whether the parser rejects this malformed image.
        assert!(
            ImageInfo::parse(&data).is_err(),
            "section extends beyond physical file bounds"
        );
    }

    #[test]
    fn accepts_section_ending_at_file_boundary() {
        let mut data = synthetic_image(&vec![0u8; 0x200]);

        // Disable the export directory to isolate section validation.
        let opt = 0x58;
        data[opt + 112..opt + 120].fill(0);

        // Raw section: offset 0x200 + size 0x200 = file length 0x400.
        let info = ImageInfo::parse(&data);

        assert!(
            info.is_ok(),
            "section ending exactly at EOF should be accepted"
        );
    }

    #[test]
    fn accepts_section_with_zero_raw_size() {
        let mut data = synthetic_image(&vec![0u8; 0x200]);

        // Disable the export directory.
        let opt = 0x58;
        data[opt + 112..opt + 120].fill(0);

        // Section header begins after the optional header.
        let sec = opt + 240;

        // A section may occupy virtual memory without having raw data.
        data[sec + 16..sec + 20].copy_from_slice(&0u32.to_le_bytes());

        let info = ImageInfo::parse(&data);

        assert!(
            info.is_ok(),
            "section with zero raw size should be accepted"
        );
    }

    #[test]
    fn unused_export_slots_are_not_exports() {
        // Ordinal base 1; three address slots: "alpha", an unused hole (0),
        // and "gamma". The Windows loader treats address 0 as "not exported".
        let data = PeBuilder::new()
            .dll(true)
            .exports(
                "t.dll",
                1,
                vec![
                    Export::named("alpha", 0x1200),
                    Export::unused(),
                    Export::named("gamma", 0x1204),
                ],
            )
            .build()
            .unwrap()
            .bytes;
        let info = ImageInfo::parse(&data).unwrap();
        let exports: Vec<_> = info
            .exports
            .iter()
            .map(|e| (e.name.as_deref(), e.ordinal, e.rva))
            .collect();
        assert_eq!(
            exports,
            [
                (Some("alpha"), 1, Some(0x1200)),
                (Some("gamma"), 3, Some(0x1204))
            ]
        );
    }

    #[test]
    fn reports_base_relocations_unless_stripped() {
        let with = PeBuilder::new().reloc_block(0x1000, &[(crate::testing::reloc::DIR64, 8)]);
        let info = ImageInfo::parse(&with.clone().build().unwrap().bytes).unwrap();
        assert!(info.has_relocations);
        let stripped = with.relocs_stripped(true).build().unwrap().bytes;
        assert!(!ImageInfo::parse(&stripped).unwrap().has_relocations);
    }

    #[test]
    fn symbol_key_known_answer() {
        let cv = CodeViewInfo {
            pdb_path: String::new(),
            guid: std::array::from_fn(|i| i as u8),
            age: 1,
        };
        // The first three GUID fields are little-endian integers, the last
        // eight bytes are printed in order; the age follows in hex.
        assert_eq!(cv.symbol_key(), "030201000504070608090A0B0C0D0E0F1");
        let cv = CodeViewInfo { age: 0x2A, ..cv };
        assert!(cv.symbol_key().ends_with("0E0F2A"));
    }

    #[test]
    fn rva_to_file_offset_maps_entry_point() {
        let data = sample_image();
        let info = ImageInfo::parse(&data).unwrap();
        let off = info.rva_to_file_offset(info.entry_rva).unwrap() as usize;
        assert_eq!(off, info.sections[0].file_offset as usize + 0x10);
        assert_eq!(data[off], 0xC3);
        // Past the section's data, and in the headers: not file-backed.
        assert_eq!(info.rva_to_file_offset(0x1040), None);
        assert_eq!(info.rva_to_file_offset(0x10), None);
    }

    #[test]
    fn rva_to_file_offset_rejects_offsets_past_4_gib() {
        let mut info = ImageInfo::parse(&sample_image()).unwrap();
        let rva = info.sections[0].rva + 0x20;
        assert!(info.rva_to_file_offset(rva).is_some());
        // Used to overflow (a panic in debug builds, a wrong offset in
        // release ones).
        info.sections[0].file_offset = u32::MAX - 0x10;
        assert_eq!(info.rva_to_file_offset(rva), None);
    }

    #[test]
    fn rejects_garbage_without_panicking() {
        assert!(ImageInfo::parse(b"").is_err());
        assert!(ImageInfo::parse(b"MZ not really").is_err());
        // Headers that describe sections the file no longer has.
        let data = sample_image();
        let headers = info_headers_len(&data);
        assert!(ImageInfo::parse(&data[..headers]).is_err());
        assert!(ImageInfo::parse(&data[..0x100]).is_err());
    }

    /// File offset of the first section's data: where the headers end.
    fn info_headers_len(data: &[u8]) -> usize {
        ImageInfo::parse(data).unwrap().sections[0].file_offset as usize
    }

    /// File offset of the first import descriptor of [`sample_image`].
    fn first_import_descriptor(data: &[u8]) -> usize {
        let dd = layout::data_directory_offset(data, dir::IMPORT).unwrap();
        let import_rva = layout::read_u32(data, dd).unwrap();
        let info = ImageInfo::parse(data).unwrap();
        info.rva_to_file_offset(import_rva).unwrap() as usize
    }

    fn malformed(result: Result<ImageInfo, PeError>) -> String {
        match result {
            Err(PeError::Malformed(why)) => why,
            other => panic!("expected PeError::Malformed, got {other:?}"),
        }
    }

    #[test]
    fn rejects_unaligned_iat_slot() {
        let mut data = sample_image();
        let desc = first_import_descriptor(&data);
        let iat = layout::read_u32(&data, desc + 16).unwrap();
        assert_eq!(iat % 8, 0, "the builder aligns the table");
        // FirstThunk four bytes on: still inside the image and its section,
        // but binding would store a pointer through an unaligned address.
        for off_by in [4u32, 1, 7] {
            data[desc + 16..desc + 20].copy_from_slice(&(iat + off_by).to_le_bytes());
            let why = malformed(ImageInfo::parse(&data));
            assert!(why.contains("not 8-byte aligned"), "{why}");
        }
        // The delay IAT is bound the same way.
        let mut data = sample()
            .delay_import("late.dll", DELAYLOAD_RVA_BASED, &[by_name(1, "Later")])
            .build()
            .unwrap()
            .bytes;
        let directory = layout::data_directory_offset(&data, dir::DELAY_IMPORT).unwrap();
        let descriptor_rva = layout::read_u32(&data, directory).unwrap();
        let info = ImageInfo::parse(&data).unwrap();
        let desc = info.rva_to_file_offset(descriptor_rva).unwrap() as usize;
        let iat = info.delay_imports[0].entries[0].iat_slot_rva;
        data[desc + 12..desc + 16].copy_from_slice(&(iat + 4).to_le_bytes());
        let why = malformed(ImageInfo::parse(&data));
        assert!(why.contains("not 8-byte aligned"), "{why}");
    }

    #[test]
    fn rejects_iat_outside_image() {
        let mut data = sample_image();
        let dd = layout::data_directory_offset(&data, dir::IMPORT).unwrap();
        let import_rva = layout::read_u32(&data, dd).unwrap();
        let desc = ImageInfo::parse(&data)
            .unwrap()
            .rva_to_file_offset(import_rva)
            .unwrap() as usize;
        // First descriptor's FirstThunk, at the 4 GiB limit: used to overflow.
        data[desc + 16..desc + 20].copy_from_slice(&0xFFFF_FFF8u32.to_le_bytes());
        assert!(matches!(
            ImageInfo::parse(&data),
            Err(PeError::Malformed(_))
        ));
        // Inside 4 GiB but past the end of the image.
        let size_of_image = ImageInfo::parse(&sample_image()).unwrap().size_of_image;
        data[desc + 16..desc + 20].copy_from_slice(&(size_of_image - 8).to_le_bytes());
        assert!(matches!(
            ImageInfo::parse(&data),
            Err(PeError::Malformed(_))
        ));
    }

    /// What every image the parser accepts must satisfy, because the loader
    /// acts on these values without looking again. Written out from the
    /// section list, not with the parser's own helpers.
    fn assert_safe_to_load(info: &ImageInfo, case: &str) {
        let size_of_image = u64::from(info.size_of_image);
        let executable = |rva: u32| {
            u64::from(rva) < size_of_image
                && info.sections.iter().any(|s| {
                    let size = if s.virtual_size == 0 {
                        s.file_size
                    } else {
                        s.virtual_size
                    };
                    s.characteristics & 0x2000_0000 != 0
                        && rva >= s.rva
                        && u64::from(rva) < u64::from(s.rva) + u64::from(size)
                })
        };
        for module in info.imports.iter().chain(&info.delay_imports) {
            for entry in &module.entries {
                let slot = u64::from(entry.iat_slot_rva);
                assert!(slot + 8 <= size_of_image, "{case}: IAT slot {slot:#x}");
                assert_eq!(slot % 8, 0, "{case}: IAT slot {slot:#x}");
            }
            if let Some(handle) = module.module_handle_rva {
                assert!(u64::from(handle) + 8 <= size_of_image, "{case}: handle");
            }
        }
        assert!(info.imports.iter().all(|m| m.module_handle_rva.is_none()));
        assert!(
            info.entry_rva == 0 || executable(info.entry_rva),
            "{case}: entry point {:#x}",
            info.entry_rva
        );
        if let Some(tls) = &info.tls {
            assert!(tls.callback_rvas.len() <= MAX_TLS_CALLBACKS, "{case}");
            assert!(tls.callbacks_va != 0 || tls.callback_rvas.is_empty());
            for &rva in &tls.callback_rvas {
                assert!(executable(rva), "{case}: TLS callback {rva:#x}");
            }
        }
        // Total over the whole RVA range: no panic, and an offset only for
        // an RVA inside a section.
        for rva in [0, 0x1000, info.entry_rva, info.size_of_image, u32::MAX] {
            if let Some(offset) = info.rva_to_file_offset(rva) {
                assert!(
                    info.sections
                        .iter()
                        .any(|s| rva >= s.rva && offset >= s.file_offset),
                    "{case}: RVA {rva:#x}"
                );
            }
        }
    }

    /// Images with every table the builder can write, for the mutation
    /// tests: an executable and a DLL.
    fn mutation_seeds() -> Vec<Vec<u8>> {
        let executable = sample()
            .delay_import(
                "late.dll",
                DELAYLOAD_RVA_BASED,
                &[by_name(1, "Later"), ImportSymbol::Ordinal(9)],
            )
            .tls_callbacks(&[0x1000, 0x1020])
            .codeview([7; 16], 1, "seed.pdb")
            .reloc_block(0x5000, &[(reloc::DIR64, 0x18), (reloc::HIGHLOW, 0x20)])
            .reloc_block(0x1000, &[(reloc::DIR64, 8)]);
        let dll = code()
            .dll(true)
            .entry_rva(0x1008)
            .import("alpha.dll", &[by_name(0, "First")])
            .tls_callbacks(&[0x1010])
            .exports(
                "seed.dll",
                3,
                vec![
                    Export::named("gamma", 0x1010),
                    Export::unused(),
                    Export::forwarder("alpha", "other.Target"),
                ],
            );
        vec![
            executable.build().unwrap().bytes,
            dll.build().unwrap().bytes,
            sample_image(),
        ]
    }

    /// The stand-in for a fuzzer: seeded, so a failure is the same failure
    /// everywhere. Overflow checks make the debug run the stricter one, but
    /// the invariants are about returned values and hold in release too.
    #[test]
    fn mutated_headers_never_panic() {
        let seeds = mutation_seeds();
        for (i, seed) in seeds.iter().enumerate() {
            let info = ImageInfo::parse(seed).unwrap();
            assert_safe_to_load(&info, &format!("seed {i}"));
        }
        // The first seed's TLS directory is where its relocation says.
        let first = ImageInfo::parse(&seeds[0]).unwrap();
        assert_eq!(first.tls.as_ref().unwrap().directory_rva, 0x5000);
        assert!(first.has_relocations && !first.delay_imports.is_empty());

        let mut rng = XorShift64::new(0x5EED_0001);
        let (mut accepted, mut refused) = (0u32, 0u32);
        for i in 0..10_000 {
            let mut data = seeds[i % seeds.len()].clone();
            mutate(&mut data, 0x400, &mut rng);
            // Now and then the file is cut short as well.
            if rng.below(16) == 0 {
                data.truncate(rng.below(data.len()));
            }
            match ImageInfo::parse(&data) {
                Ok(info) => {
                    accepted += 1;
                    assert_safe_to_load(&info, &format!("mutation {i}"));
                }
                Err(PeError::Malformed(why) | PeError::Unsupported(why)) => {
                    refused += 1;
                    assert!(!why.is_empty(), "mutation {i}");
                }
            }
        }
        // Both outcomes occur, so the loop tests the parser and not only
        // the first header check.
        assert!(accepted > 0, "no mutation was accepted");
        assert!(refused > 0, "no mutation was refused");
    }

    /// A minimal PE32+ image: one section at RVA 0x1000 holding `body`, with
    /// the export directory (RVA 0x1000, 0x80 bytes) pointing at it.
    fn synthetic_image(body: &[u8]) -> Vec<u8> {
        let mut d = vec![0u8; 0x200];
        d[0..2].copy_from_slice(b"MZ");
        d[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        d[0x40..0x44].copy_from_slice(b"PE\0\0");
        d[0x44..0x46].copy_from_slice(&MACHINE_AMD64.to_le_bytes());
        d[0x46..0x48].copy_from_slice(&1u16.to_le_bytes()); // sections
        d[0x54..0x56].copy_from_slice(&240u16.to_le_bytes()); // optional header size
        d[0x56..0x58].copy_from_slice(&0x2022u16.to_le_bytes()); // DLL, large address aware
        let opt = 0x58;
        d[opt..opt + 2].copy_from_slice(&0x20Bu16.to_le_bytes());
        d[opt + 24..opt + 32].copy_from_slice(&0x1_8000_0000u64.to_le_bytes());
        d[opt + 32..opt + 36].copy_from_slice(&0x1000u32.to_le_bytes());
        d[opt + 36..opt + 40].copy_from_slice(&0x200u32.to_le_bytes());
        d[opt + 56..opt + 60].copy_from_slice(&0x2000u32.to_le_bytes());
        d[opt + 60..opt + 64].copy_from_slice(&0x200u32.to_le_bytes());
        d[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes());
        d[opt + 112..opt + 116].copy_from_slice(&0x1000u32.to_le_bytes());
        d[opt + 116..opt + 120].copy_from_slice(&0x80u32.to_le_bytes());
        let sec = opt + 240;
        d[sec..sec + 6].copy_from_slice(b".edata");
        d[sec + 8..sec + 12].copy_from_slice(&0x200u32.to_le_bytes()); // virtual size
        d[sec + 12..sec + 16].copy_from_slice(&0x1000u32.to_le_bytes());
        d[sec + 16..sec + 20].copy_from_slice(&0x200u32.to_le_bytes()); // raw size
        d[sec + 20..sec + 24].copy_from_slice(&0x200u32.to_le_bytes()); // raw offset
        d.extend_from_slice(body);
        d.resize(0x400, 0);
        d
    }

    #[test]
    fn every_name_of_a_shared_ordinal_survives() {
        // Two names for address slot 0, one for slot 1. The PE-specific
        // export table keeps only one name per slot, so "alpha" would be
        // lost; the generic iterator keeps both.
        let mut body = vec![0u8; 0x100];
        let put32 =
            |b: &mut Vec<u8>, at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        put32(&mut body, 12, 0x1040); // name of the DLL
        put32(&mut body, 16, 1); // ordinal base
        put32(&mut body, 20, 2); // number of functions
        put32(&mut body, 24, 3); // number of names
        put32(&mut body, 28, 0x1028); // address table
        put32(&mut body, 32, 0x1034); // name pointer table
        put32(&mut body, 36, 0x1044); // name ordinal table
        put32(&mut body, 0x28, 0x1200);
        put32(&mut body, 0x2C, 0x1204);
        put32(&mut body, 0x34, 0x1050);
        put32(&mut body, 0x38, 0x1056);
        put32(&mut body, 0x3C, 0x105C);
        body[0x40..0x46].copy_from_slice(b"t.dll\0");
        body[0x44..0x46].copy_from_slice(&0u16.to_le_bytes());
        body[0x46..0x48].copy_from_slice(&0u16.to_le_bytes());
        body[0x48..0x4A].copy_from_slice(&1u16.to_le_bytes());
        body[0x50..0x56].copy_from_slice(b"alpha\0");
        body[0x56..0x5B].copy_from_slice(b"beta\0");
        body[0x5C..0x62].copy_from_slice(b"gamma\0");

        let info = ImageInfo::parse(&synthetic_image(&body)).unwrap();
        let exports: Vec<_> = info
            .exports
            .iter()
            .map(|e| (e.name.as_deref(), e.ordinal, e.rva))
            .collect();
        assert_eq!(
            exports,
            [
                (Some("alpha"), 1, Some(0x1200)),
                (Some("beta"), 1, Some(0x1200)),
                (Some("gamma"), 2, Some(0x1204)),
            ]
        );
    }
}
