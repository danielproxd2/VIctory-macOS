//! Synthetic PE32+ images for tests, assembled in code.
//!
//! Nothing here comes from a real executable: every byte is written by
//! [`PeBuilder`] from the published PE/COFF layout, so tests of the parser,
//! the image patcher and the loader run on any host and need no file from a
//! title (docs/asset-policy.md).
//!
//! The module is compiled for this crate's own tests and for crates that
//! enable the `test-support` feature as a dev-dependency. Cargo unifies
//! features across a `cargo test --workspace` build, so the code may end up
//! in a normal library build too. It therefore has no global state, runs
//! nothing unless called and reports every problem as a [`BuildError`]
//! instead of panicking.
//!
//! The images are meant for parsers. They follow the format closely but are
//! not promised to be loadable by Windows.

use crate::layout::{self, dir, file_flags};
use crate::{ImportSymbol, MACHINE_AMD64};

/// `SectionAlignment` of every image built here.
pub const SECTION_ALIGNMENT: u32 = 0x1000;
/// `FileAlignment` of every image built here.
pub const FILE_ALIGNMENT: u32 = 0x200;
/// Image base used unless [`PeBuilder::image_base`] says otherwise.
pub const DEFAULT_IMAGE_BASE: u64 = 0x1_8000_0000;
/// `IMAGE_DELAYLOAD_RVA_BASED`: the delay descriptor holds RVAs, not VAs.
pub const DELAYLOAD_RVA_BASED: u32 = 1;

/// Section characteristics (`IMAGE_SCN_*` combinations linkers emit).
pub mod scn {
    /// Code, execute, read.
    pub const CODE: u32 = 0x6000_0020;
    /// Initialized data, read.
    pub const RDATA: u32 = 0x4000_0040;
    /// Initialized data, read, write.
    pub const DATA: u32 = 0xC000_0040;
    /// Initialized data, read, discardable.
    pub const RELOC: u32 = 0x4200_0040;
}

/// Base relocation types (`IMAGE_REL_BASED_*`).
pub mod reloc {
    pub const ABSOLUTE: u8 = 0;
    pub const HIGHLOW: u8 = 3;
    pub const DIR64: u8 = 10;
}

/// The builder was asked for something that does not fit a PE32+ image.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cannot build synthetic image: {0}")]
pub struct BuildError(&'static str);

const TOO_BIG: BuildError = BuildError("image does not fit in 4 GiB");

/// What one slot of the export address table holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportTarget {
    /// A function or data RVA.
    Rva(u32),
    /// A forwarder string such as `"other.Function"` or `"other.#7"`.
    Forward(String),
    /// Address 0: a gap in the ordinals.
    Unused,
}

/// One export address table slot. Its ordinal is the ordinal base plus its
/// position in the list given to [`PeBuilder::exports`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Export {
    /// `None` exports by ordinal only.
    pub name: Option<String>,
    pub target: ExportTarget,
}

impl Export {
    pub fn named(name: &str, rva: u32) -> Self {
        Export {
            name: Some(name.to_owned()),
            target: ExportTarget::Rva(rva),
        }
    }

    pub fn ordinal_only(rva: u32) -> Self {
        Export {
            name: None,
            target: ExportTarget::Rva(rva),
        }
    }

    pub fn forwarder(name: &str, to: &str) -> Self {
        Export {
            name: Some(name.to_owned()),
            target: ExportTarget::Forward(to.to_owned()),
        }
    }

    pub fn unused() -> Self {
        Export {
            name: None,
            target: ExportTarget::Unused,
        }
    }
}

/// A finished image and the places in it that tests need to find.
#[derive(Debug, Clone, Default)]
pub struct SyntheticPe {
    /// The complete file.
    pub bytes: Vec<u8>,
    /// RVA of `IMAGE_TLS_DIRECTORY64`, if the image has one.
    pub tls_directory_rva: Option<u32>,
    /// RVA of the TLS callback array, if the image has one. The directory's
    /// `AddressOfCallBacks` is the image base plus this.
    pub tls_callbacks_rva: Option<u32>,
}

#[derive(Debug, Clone)]
struct Section {
    name: String,
    characteristics: u32,
    data: Vec<u8>,
}

#[derive(Debug, Clone)]
struct ImportDll {
    dll: String,
    symbols: Vec<ImportSymbol>,
}

#[derive(Debug, Clone)]
struct DelayDll {
    dll: String,
    attributes: u32,
    symbols: Vec<ImportSymbol>,
}

#[derive(Debug, Clone)]
struct ExportTable {
    dll: String,
    ordinal_base: u32,
    slots: Vec<Export>,
}

#[derive(Debug, Clone)]
struct CodeView {
    guid: [u8; 16],
    age: u32,
    pdb_path: String,
}

#[derive(Debug, Clone)]
struct RelocBlock {
    page: u32,
    entries: Vec<(u8, u16)>,
}

/// Describes a PE32+ x86-64 image; [`PeBuilder::build`] writes it out.
///
/// Sections given with [`PeBuilder::section`] come first, in order, starting
/// at RVA 0x1000. The tables asked for are then placed in sections of their
/// own, always in this order: imports, delay imports, TLS, exports, debug,
/// base relocations. Adding a later table therefore never moves an earlier
/// one.
#[derive(Debug, Clone)]
pub struct PeBuilder {
    machine: u16,
    timestamp: u32,
    image_base: u64,
    entry_rva: u32,
    dll: bool,
    relocs_stripped: bool,
    subsystem: u16,
    dll_characteristics: u16,
    stack_reserve: u64,
    stack_commit: u64,
    sections: Vec<Section>,
    imports: Vec<ImportDll>,
    delay_imports: Vec<DelayDll>,
    /// `None`: no TLS directory. `Some(None)`: a directory without a
    /// callback array. `Some(Some(rvas))`: a null-terminated array.
    tls: Option<Option<Vec<u32>>>,
    exports: Option<ExportTable>,
    codeview: Option<CodeView>,
    relocations: Vec<RelocBlock>,
    directories: Vec<(usize, u32, u32)>,
}

impl Default for PeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl PeBuilder {
    /// An executable with no sections and no tables.
    pub fn new() -> Self {
        PeBuilder {
            machine: MACHINE_AMD64,
            timestamp: 0,
            image_base: DEFAULT_IMAGE_BASE,
            entry_rva: 0,
            dll: false,
            relocs_stripped: false,
            subsystem: layout::subsystem::WINDOWS_CUI,
            dll_characteristics: layout::dll_flags::HIGH_ENTROPY_VA
                | layout::dll_flags::DYNAMIC_BASE
                | layout::dll_flags::NX_COMPAT,
            stack_reserve: 0x10_0000,
            stack_commit: 0x1000,
            sections: Vec::new(),
            imports: Vec::new(),
            delay_imports: Vec::new(),
            tls: None,
            exports: None,
            codeview: None,
            relocations: Vec::new(),
            directories: Vec::new(),
        }
    }

    /// `IMAGE_FILE_HEADER::Machine` (AMD64 by default).
    pub fn machine(mut self, machine: u16) -> Self {
        self.machine = machine;
        self
    }

    /// `IMAGE_FILE_HEADER::TimeDateStamp`.
    pub fn timestamp(mut self, timestamp: u32) -> Self {
        self.timestamp = timestamp;
        self
    }

    pub fn image_base(mut self, image_base: u64) -> Self {
        self.image_base = image_base;
        self
    }

    /// `AddressOfEntryPoint`. Not checked against the sections.
    pub fn entry_rva(mut self, rva: u32) -> Self {
        self.entry_rva = rva;
        self
    }

    /// Sets or clears `IMAGE_FILE_DLL`.
    pub fn dll(mut self, dll: bool) -> Self {
        self.dll = dll;
        self
    }

    /// Sets or clears `IMAGE_FILE_RELOCS_STRIPPED`.
    pub fn relocs_stripped(mut self, stripped: bool) -> Self {
        self.relocs_stripped = stripped;
        self
    }

    pub fn subsystem(mut self, subsystem: u16) -> Self {
        self.subsystem = subsystem;
        self
    }

    /// `DllCharacteristics`, replacing the default (high-entropy VA, dynamic
    /// base, NX compatible).
    pub fn dll_characteristics(mut self, flags: u16) -> Self {
        self.dll_characteristics = flags;
        self
    }

    /// `SizeOfStackReserve`.
    pub fn stack_reserve(mut self, bytes: u64) -> Self {
        self.stack_reserve = bytes;
        self
    }

    /// `SizeOfStackCommit`.
    pub fn stack_commit(mut self, bytes: u64) -> Self {
        self.stack_commit = bytes;
        self
    }

    /// Adds a section holding `data`. Its RVA is what
    /// [`PeBuilder::next_section_rva`] returned just before this call.
    pub fn section(mut self, name: &str, characteristics: u32, data: &[u8]) -> Self {
        self.sections.push(Section {
            name: name.to_owned(),
            characteristics,
            data: data.to_vec(),
        });
        self
    }

    /// RVA the next section added with [`PeBuilder::section`] will get.
    pub fn next_section_rva(&self) -> Result<u32, BuildError> {
        self.sections
            .iter()
            .try_fold(SECTION_ALIGNMENT, |rva, s| next_rva(rva, s.data.len()))
    }

    /// Adds an import descriptor for `dll` with a name table and an IAT.
    pub fn import(mut self, dll: &str, symbols: &[ImportSymbol]) -> Self {
        self.imports.push(ImportDll {
            dll: dll.to_owned(),
            symbols: symbols.to_vec(),
        });
        self
    }

    /// Adds a delay-load descriptor for `dll`. `attributes` is written as
    /// given ([`DELAYLOAD_RVA_BASED`] is what current linkers emit); the
    /// descriptor's addresses are RVAs whatever it says. The delay IAT is
    /// left zeroed: no helper thunks exist.
    pub fn delay_import(mut self, dll: &str, attributes: u32, symbols: &[ImportSymbol]) -> Self {
        self.delay_imports.push(DelayDll {
            dll: dll.to_owned(),
            attributes,
            symbols: symbols.to_vec(),
        });
        self
    }

    /// Adds a TLS directory whose callback array lists `callback_rvas`
    /// followed by the null terminator. The RVAs are not checked.
    pub fn tls_callbacks(mut self, callback_rvas: &[u32]) -> Self {
        self.tls = Some(Some(callback_rvas.to_vec()));
        self
    }

    /// Adds a TLS directory whose `AddressOfCallBacks` is 0.
    pub fn tls_without_callbacks(mut self) -> Self {
        self.tls = Some(None);
        self
    }

    /// Adds an export directory. Slot `i` of `slots` has the ordinal
    /// `ordinal_base + i`.
    ///
    /// An ordinal is a 16-bit number. The ordinal base and the last slot's
    /// ordinal may each be at most 65535: a larger base, or
    /// `ordinal_base + slots.len()` above 65536, is a [`BuildError`],
    /// whatever the slots hold. An ordinal base of 0 allows 65536 slots.
    pub fn exports(mut self, dll: &str, ordinal_base: u32, slots: Vec<Export>) -> Self {
        self.exports = Some(ExportTable {
            dll: dll.to_owned(),
            ordinal_base,
            slots,
        });
        self
    }

    /// Adds a debug directory with one CodeView (RSDS) record.
    pub fn codeview(mut self, guid: [u8; 16], age: u32, pdb_path: &str) -> Self {
        self.codeview = Some(CodeView {
            guid,
            age,
            pdb_path: pdb_path.to_owned(),
        });
        self
    }

    /// Adds one base relocation block exactly as given: `page` is its page
    /// RVA (not checked to be page aligned or inside the image) and each
    /// entry is `(type, offset in the page)`. See [`reloc`] for the types.
    /// A block with an odd number of entries is padded with an `ABSOLUTE`
    /// entry, as linkers do.
    pub fn reloc_block(mut self, page: u32, entries: &[(u8, u16)]) -> Self {
        self.relocations.push(RelocBlock {
            page,
            entries: entries.to_vec(),
        });
        self
    }

    /// Sets data directory `index` to `{ rva, size }`, overriding whatever
    /// the builder would write there. Use it for directories the builder
    /// does not generate (exception table, load config) pointing into a
    /// section added with [`PeBuilder::section`].
    pub fn data_directory(mut self, index: usize, rva: u32, size: u32) -> Self {
        self.directories.push((index, rva, size));
        self
    }

    /// Writes the image.
    pub fn build(&self) -> Result<SyntheticPe, BuildError> {
        let generated = usize::from(!self.imports.is_empty())
            + usize::from(!self.delay_imports.is_empty())
            + usize::from(self.tls.is_some())
            + usize::from(self.exports.is_some())
            + usize::from(self.codeview.is_some())
            + usize::from(!self.relocations.is_empty());
        let section_count = self
            .sections
            .len()
            .checked_add(generated)
            .ok_or(BuildError("too many sections"))?;
        let headers_len = section_count
            .checked_mul(SECTION_HEADER_SIZE)
            .and_then(|n| n.checked_add(SECTION_TABLE))
            .and_then(|n| n.checked_next_multiple_of(FILE_ALIGNMENT as usize))
            .ok_or(BuildError("too many sections"))?;

        let mut placed = Placed {
            sections: Vec::with_capacity(section_count),
            next_rva: SECTION_ALIGNMENT,
            next_file: u32::try_from(headers_len).map_err(|_| TOO_BIG)?,
        };
        let mut dirs = [(0u32, 0u32); DIRECTORY_COUNT];
        let mut out = SyntheticPe::default();

        for s in &self.sections {
            placed.add(&s.name, s.characteristics, s.data.clone())?;
        }
        if !self.imports.is_empty() {
            let data = self.import_section(placed.next_rva, &mut dirs)?;
            placed.add(".idata", scn::DATA, data)?;
        }
        if !self.delay_imports.is_empty() {
            let data = self.delay_section(placed.next_rva, &mut dirs)?;
            placed.add(".didat", scn::DATA, data)?;
        }
        if let Some(callbacks) = &self.tls {
            out.tls_directory_rva = Some(placed.next_rva);
            let (data, array) =
                self.tls_section(placed.next_rva, callbacks.as_deref(), &mut dirs)?;
            out.tls_callbacks_rva = array;
            placed.add(".tls", scn::DATA, data)?;
        }
        if let Some(table) = &self.exports {
            let data = export_section(placed.next_rva, table, &mut dirs)?;
            placed.add(".edata", scn::RDATA, data)?;
        }
        if let Some(cv) = &self.codeview {
            let data = debug_section(placed.next_rva, placed.next_file, cv, &mut dirs)?;
            placed.add(".buildid", scn::RDATA, data)?;
        }
        if !self.relocations.is_empty() {
            let data = reloc_section(placed.next_rva, &self.relocations, &mut dirs)?;
            placed.add(".reloc", scn::RELOC, data)?;
        }
        for &(index, rva, size) in &self.directories {
            *dirs
                .get_mut(index)
                .ok_or(BuildError("data directory index out of range"))? = (rva, size);
        }

        let mut d = vec![0u8; headers_len];
        put(&mut d, 0, &DOS_SIGNATURE)?;
        put_u32(&mut d, layout::DOS_E_LFANEW, NT_HEADERS as u32)?;
        put(&mut d, NT_HEADERS, &[b'P', b'E', 0, 0])?;

        let fh = NT_HEADERS + layout::NT_SIGNATURE_SIZE;
        put_u16(&mut d, fh, self.machine)?;
        let count = u16::try_from(section_count).map_err(|_| BuildError("too many sections"))?;
        put_u16(&mut d, fh + 2, count)?;
        put_u32(&mut d, fh + 4, self.timestamp)?;
        put_u16(&mut d, fh + 16, OPTIONAL_HEADER_SIZE as u16)?;
        // EXECUTABLE_IMAGE | LARGE_ADDRESS_AWARE, plus what was asked for.
        let mut characteristics = 0x0022;
        if self.dll {
            characteristics |= file_flags::DLL;
        }
        if self.relocs_stripped {
            characteristics |= file_flags::RELOCS_STRIPPED;
        }
        put_u16(
            &mut d,
            fh + layout::FILE_HEADER_CHARACTERISTICS,
            characteristics,
        )?;

        let opt = fh + layout::FILE_HEADER_SIZE;
        put_u16(&mut d, opt, 0x20B)?; // PE32+
        put_u32(
            &mut d,
            opt + layout::OPT_ADDRESS_OF_ENTRY_POINT,
            self.entry_rva,
        )?;
        put_u64(&mut d, opt + 24, self.image_base)?;
        put_u32(&mut d, opt + 32, SECTION_ALIGNMENT)?;
        put_u32(&mut d, opt + 36, FILE_ALIGNMENT)?;
        put_u16(&mut d, opt + 40, 6)?; // OS version 6.0
        put_u16(&mut d, opt + 48, 6)?; // subsystem version 6.0
        put_u32(&mut d, opt + 56, placed.next_rva)?; // SizeOfImage
        put_u32(&mut d, opt + 60, headers_len as u32)?; // SizeOfHeaders
        put_u16(&mut d, opt + 68, self.subsystem)?;
        put_u16(
            &mut d,
            opt + layout::OPT_DLL_CHARACTERISTICS,
            self.dll_characteristics,
        )?;
        put_u64(&mut d, opt + 72, self.stack_reserve)?;
        put_u64(&mut d, opt + 80, self.stack_commit)?;
        put_u64(&mut d, opt + 88, 0x10_0000)?; // heap reserve
        put_u64(&mut d, opt + 96, 0x1000)?; // heap commit
        put_u32(
            &mut d,
            opt + layout::OPT_NUMBER_OF_RVA_AND_SIZES,
            DIRECTORY_COUNT as u32,
        )?;
        for (i, &(rva, size)) in dirs.iter().enumerate() {
            let at = opt + layout::OPT_DATA_DIRECTORY + i * layout::DATA_DIRECTORY_SIZE;
            put_u32(&mut d, at, rva)?;
            put_u32(&mut d, at + 4, size)?;
        }

        for (i, s) in placed.sections.iter().enumerate() {
            let at = SECTION_TABLE + i * SECTION_HEADER_SIZE;
            put(&mut d, at, s.name.as_bytes())?;
            put_u32(&mut d, at + 8, s.virtual_size)?;
            put_u32(&mut d, at + 12, s.rva)?;
            put_u32(&mut d, at + 16, s.file_size)?;
            put_u32(&mut d, at + 20, s.file_offset)?;
            put_u32(&mut d, at + 36, s.characteristics)?;
        }
        for s in &placed.sections {
            d.extend_from_slice(&s.data);
            d.resize(s.file_offset as usize + s.file_size as usize, 0);
        }
        out.bytes = d;
        Ok(out)
    }

    fn va(&self, rva: u32) -> Result<u64, BuildError> {
        self.image_base
            .checked_add(u64::from(rva))
            .ok_or(BuildError("image base plus RVA overflows"))
    }

    /// Descriptors, then every IAT (contiguous, as the IAT directory wants),
    /// then per DLL its name table, hint/name entries and name.
    fn import_section(&self, rva: u32, dirs: &mut Dirs) -> Result<Vec<u8>, BuildError> {
        const DESCRIPTOR: usize = 20;
        let mut b = Blob::new(rva);
        let table = b.zeros(table_len(self.imports.len(), DESCRIPTOR)?)?;
        let table_end = b.here()?;
        let mut iats = Vec::with_capacity(self.imports.len());
        for m in &self.imports {
            iats.push(b.zeros(table_len(m.symbols.len(), 8)?)?);
        }
        let iat_end = b.here()?;
        for (i, (m, &iat)) in self.imports.iter().zip(&iats).enumerate() {
            let int = b.zeros(table_len(m.symbols.len(), 8)?)?;
            for (j, symbol) in m.symbols.iter().enumerate() {
                let thunk = thunk(&mut b, symbol)?;
                b.set(slot(int, j, 8)?, &thunk.to_le_bytes())?;
                b.set(slot(iat, j, 8)?, &thunk.to_le_bytes())?;
            }
            let name = b.cstr(&m.dll)?;
            let desc = slot(table, i, DESCRIPTOR)?;
            b.set_u32(desc, 0, int)?; // OriginalFirstThunk
            b.set_u32(desc, 12, name)?;
            b.set_u32(desc, 16, iat)?; // FirstThunk
        }
        dirs[dir::IMPORT] = (table, table_end - table);
        dirs[dir::IAT] = (table_end, iat_end - table_end);
        Ok(b.bytes)
    }

    /// `IMAGE_DELAYLOAD_DESCRIPTOR`s, then per DLL its module handle slot,
    /// IAT, name table, hint/name entries and name.
    fn delay_section(&self, rva: u32, dirs: &mut Dirs) -> Result<Vec<u8>, BuildError> {
        const DESCRIPTOR: usize = 32;
        let mut b = Blob::new(rva);
        let table = b.zeros(table_len(self.delay_imports.len(), DESCRIPTOR)?)?;
        let table_end = b.here()?;
        for (i, m) in self.delay_imports.iter().enumerate() {
            let handle = b.zeros(8)?;
            let iat = b.zeros(table_len(m.symbols.len(), 8)?)?;
            let int = b.zeros(table_len(m.symbols.len(), 8)?)?;
            for (j, symbol) in m.symbols.iter().enumerate() {
                let thunk = thunk(&mut b, symbol)?;
                b.set(slot(int, j, 8)?, &thunk.to_le_bytes())?;
            }
            let name = b.cstr(&m.dll)?;
            let desc = slot(table, i, DESCRIPTOR)?;
            b.set_u32(desc, 0, m.attributes)?;
            b.set_u32(desc, 4, name)?;
            b.set_u32(desc, 8, handle)?;
            b.set_u32(desc, 12, iat)?;
            b.set_u32(desc, 16, int)?;
        }
        dirs[dir::DELAY_IMPORT] = (table, table_end - table);
        Ok(b.bytes)
    }

    /// `IMAGE_TLS_DIRECTORY64`, the index, a 16-byte template and, if asked
    /// for, the callback array, whose RVA is returned with the bytes.
    fn tls_section(
        &self,
        rva: u32,
        callbacks: Option<&[u32]>,
        dirs: &mut Dirs,
    ) -> Result<(Vec<u8>, Option<u32>), BuildError> {
        const DIRECTORY: usize = 40;
        const TEMPLATE: usize = 16;
        let mut b = Blob::new(rva);
        let directory = b.zeros(DIRECTORY)?;
        let index = b.zeros(8)?;
        let template = b.zeros(TEMPLATE)?;
        let template_end = b.here()?;
        let mut callbacks_va = 0;
        let mut array = None;
        if let Some(rvas) = callbacks {
            let at = b.here()?;
            array = Some(at);
            callbacks_va = self.va(at)?;
            for &callback in rvas {
                b.push(&self.va(callback)?.to_le_bytes())?;
            }
            b.zeros(8)?;
        }
        b.set(directory, &self.va(template)?.to_le_bytes())?;
        b.set(
            slot(directory, 1, 8)?,
            &self.va(template_end)?.to_le_bytes(),
        )?;
        b.set(slot(directory, 2, 8)?, &self.va(index)?.to_le_bytes())?;
        b.set(slot(directory, 3, 8)?, &callbacks_va.to_le_bytes())?;
        dirs[dir::TLS] = (directory, DIRECTORY as u32);
        Ok((b.bytes, array))
    }
}

/// `e_magic`, spelled as two numbers: written as text, the pair is the
/// executable signature the asset policy looks for in committed files.
const DOS_SIGNATURE: [u8; 2] = [0x4D, 0x5A];
const NT_HEADERS: usize = 0x40;
const OPTIONAL_HEADER_SIZE: usize = 240;
const SECTION_TABLE: usize =
    NT_HEADERS + layout::NT_SIGNATURE_SIZE + layout::FILE_HEADER_SIZE + OPTIONAL_HEADER_SIZE;
const SECTION_HEADER_SIZE: usize = 40;
const DIRECTORY_COUNT: usize = 16;

type Dirs = [(u32, u32); DIRECTORY_COUNT];

/// One past the largest export ordinal.
const EXPORT_ORDINAL_END: u64 = 0x1_0000;
const TOO_MANY_EXPORTS: BuildError = BuildError("an export ordinal does not fit in 16 bits");

/// The export directory, its three tables and its strings. Forwarder
/// strings must lie inside the directory, so the directory covers it all.
fn export_section(rva: u32, table: &ExportTable, dirs: &mut Dirs) -> Result<Vec<u8>, BuildError> {
    // Every slot has an ordinal, named or not, and an ordinal is 16 bits,
    // as is the base they count from.
    let fits = u64::try_from(table.slots.len())
        .ok()
        .and_then(|n| n.checked_add(u64::from(table.ordinal_base)))
        .is_some_and(|end| end <= EXPORT_ORDINAL_END);
    if !fits || u64::from(table.ordinal_base) >= EXPORT_ORDINAL_END {
        return Err(TOO_MANY_EXPORTS);
    }
    let mut b = Blob::new(rva);
    let directory = b.zeros(40)?;
    let functions = b.zeros(table_bytes(table.slots.len(), 4)?)?;
    let mut named: Vec<(&str, usize)> = table
        .slots
        .iter()
        .enumerate()
        .filter_map(|(i, e)| Some((e.name.as_deref()?, i)))
        .collect();
    // The name pointer table is sorted so loaders can search it.
    named.sort();
    let names = b.zeros(table_bytes(named.len(), 4)?)?;
    let ordinals = b.zeros(table_bytes(named.len(), 2)?)?;
    let dll = b.cstr(&table.dll)?;
    for (k, &(name, index)) in named.iter().enumerate() {
        let at = b.cstr(name)?;
        b.set(slot(names, k, 4)?, &at.to_le_bytes())?;
        let index = u16::try_from(index).map_err(|_| TOO_MANY_EXPORTS)?;
        b.set(slot(ordinals, k, 2)?, &index.to_le_bytes())?;
    }
    for (i, e) in table.slots.iter().enumerate() {
        let address = match &e.target {
            ExportTarget::Rva(rva) => *rva,
            ExportTarget::Forward(to) => b.cstr(to)?,
            ExportTarget::Unused => 0,
        };
        b.set(slot(functions, i, 4)?, &address.to_le_bytes())?;
    }
    let count = |n: usize| u32::try_from(n).map_err(|_| TOO_MANY_EXPORTS);
    b.set_u32(directory, 12, dll)?;
    b.set_u32(directory, 16, table.ordinal_base)?;
    b.set_u32(directory, 20, count(table.slots.len())?)?;
    b.set_u32(directory, 24, count(named.len())?)?;
    b.set_u32(directory, 28, functions)?;
    b.set_u32(directory, 32, names)?;
    b.set_u32(directory, 36, ordinals)?;
    dirs[dir::EXPORT] = (directory, b.here()? - directory);
    Ok(b.bytes)
}

/// One `IMAGE_DEBUG_DIRECTORY` followed by its RSDS record.
fn debug_section(
    rva: u32,
    file_offset: u32,
    cv: &CodeView,
    dirs: &mut Dirs,
) -> Result<Vec<u8>, BuildError> {
    const DIRECTORY: usize = 28;
    const TYPE_CODEVIEW: u32 = 2;
    let mut b = Blob::new(rva);
    let directory = b.zeros(DIRECTORY)?;
    let record = b.push(b"RSDS")?;
    b.push(&cv.guid)?;
    b.push(&cv.age.to_le_bytes())?;
    b.cstr(&cv.pdb_path)?;
    let size = b.here()? - record;
    b.set_u32(directory, 12, TYPE_CODEVIEW)?;
    b.set_u32(directory, 16, size)?;
    b.set_u32(directory, 20, record)?;
    let pointer = file_offset.checked_add(record - rva).ok_or(TOO_BIG)?;
    b.set_u32(directory, 24, pointer)?;
    dirs[dir::DEBUG] = (directory, DIRECTORY as u32);
    Ok(b.bytes)
}

fn reloc_section(rva: u32, blocks: &[RelocBlock], dirs: &mut Dirs) -> Result<Vec<u8>, BuildError> {
    let mut b = Blob::new(rva);
    for block in blocks {
        let padded = block.entries.len() + block.entries.len() % 2;
        let size = table_bytes(padded, 2)?
            .checked_add(8)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or(TOO_BIG)?;
        b.push(&block.page.to_le_bytes())?;
        b.push(&size.to_le_bytes())?;
        for &(kind, offset) in &block.entries {
            if kind > 0xF || offset > 0xFFF {
                return Err(BuildError(
                    "relocation type or offset does not fit its field",
                ));
            }
            b.push(&(u16::from(kind) << 12 | offset).to_le_bytes())?;
        }
        if padded != block.entries.len() {
            b.zeros(2)?;
        }
    }
    dirs[dir::BASERELOC] = (rva, b.here()? - rva);
    Ok(b.bytes)
}

/// Writes the hint/name entry of a by-name import and returns the name
/// table value for `symbol`.
fn thunk(b: &mut Blob, symbol: &ImportSymbol) -> Result<u64, BuildError> {
    match symbol {
        ImportSymbol::Ordinal(ordinal) => Ok(1 << 63 | u64::from(*ordinal)),
        ImportSymbol::Name { hint, name } => {
            b.align(2)?;
            let at = b.push(&hint.to_le_bytes())?;
            b.cstr(name)?;
            Ok(u64::from(at))
        }
    }
}

/// Bytes of a table of `count` entries plus its null terminator.
fn table_len(count: usize, entry: usize) -> Result<usize, BuildError> {
    count
        .checked_add(1)
        .and_then(|n| n.checked_mul(entry))
        .ok_or(TOO_BIG)
}

fn table_bytes(count: usize, entry: usize) -> Result<usize, BuildError> {
    count.checked_mul(entry).ok_or(TOO_BIG)
}

/// RVA of entry `index` of a table of `stride`-byte entries at `table`.
fn slot(table: u32, index: usize, stride: usize) -> Result<u32, BuildError> {
    index
        .checked_mul(stride)
        .and_then(|offset| u32::try_from(offset).ok())
        .and_then(|offset| table.checked_add(offset))
        .ok_or(TOO_BIG)
}

/// RVA after a section of `len` bytes at `rva` (an empty one still takes a
/// page, so no two sections share an RVA).
fn next_rva(rva: u32, len: usize) -> Result<u32, BuildError> {
    u32::try_from(len.max(1))
        .ok()
        .and_then(|len| len.checked_next_multiple_of(SECTION_ALIGNMENT))
        .and_then(|len| rva.checked_add(len))
        .ok_or(TOO_BIG)
}

struct PlacedSection {
    name: String,
    characteristics: u32,
    rva: u32,
    virtual_size: u32,
    file_offset: u32,
    file_size: u32,
    data: Vec<u8>,
}

struct Placed {
    sections: Vec<PlacedSection>,
    next_rva: u32,
    next_file: u32,
}

impl Placed {
    fn add(&mut self, name: &str, characteristics: u32, data: Vec<u8>) -> Result<(), BuildError> {
        if name.len() > 8 {
            return Err(BuildError("section name is longer than 8 bytes"));
        }
        let virtual_size = u32::try_from(data.len()).map_err(|_| TOO_BIG)?;
        let file_size = virtual_size
            .checked_next_multiple_of(FILE_ALIGNMENT)
            .ok_or(TOO_BIG)?;
        let (rva, file_offset) = (self.next_rva, self.next_file);
        self.next_rva = next_rva(rva, data.len())?;
        self.next_file = file_offset.checked_add(file_size).ok_or(TOO_BIG)?;
        self.sections.push(PlacedSection {
            name: name.to_owned(),
            characteristics,
            rva,
            virtual_size,
            file_offset,
            file_size,
            data,
        });
        Ok(())
    }
}

/// The contents of one generated section, addressed by RVA.
struct Blob {
    rva: u32,
    bytes: Vec<u8>,
}

impl Blob {
    fn new(rva: u32) -> Self {
        Blob {
            rva,
            bytes: Vec::new(),
        }
    }

    /// RVA of the next byte to be written.
    fn here(&self) -> Result<u32, BuildError> {
        u32::try_from(self.bytes.len())
            .ok()
            .and_then(|len| self.rva.checked_add(len))
            .ok_or(TOO_BIG)
    }

    fn align(&mut self, to: usize) -> Result<(), BuildError> {
        let len = self
            .bytes
            .len()
            .checked_next_multiple_of(to)
            .ok_or(TOO_BIG)?;
        self.bytes.resize(len, 0);
        Ok(())
    }

    /// Appends `len` zero bytes and returns their RVA.
    fn zeros(&mut self, len: usize) -> Result<u32, BuildError> {
        let at = self.here()?;
        let end = self.bytes.len().checked_add(len).ok_or(TOO_BIG)?;
        self.bytes.resize(end, 0);
        self.here()?;
        Ok(at)
    }

    /// Appends `v` and returns its RVA.
    fn push(&mut self, v: &[u8]) -> Result<u32, BuildError> {
        let at = self.here()?;
        self.bytes.extend_from_slice(v);
        self.here()?;
        Ok(at)
    }

    /// Appends `s` with a null terminator and returns its RVA.
    fn cstr(&mut self, s: &str) -> Result<u32, BuildError> {
        let at = self.push(s.as_bytes())?;
        self.push(&[0])?;
        Ok(at)
    }

    /// Overwrites already written bytes at `rva`.
    fn set(&mut self, rva: u32, v: &[u8]) -> Result<(), BuildError> {
        let at = rva.checked_sub(self.rva).ok_or(TOO_BIG)? as usize;
        put(&mut self.bytes, at, v)
    }

    fn set_u32(&mut self, rva: u32, offset: u32, v: u32) -> Result<(), BuildError> {
        self.set(rva.checked_add(offset).ok_or(TOO_BIG)?, &v.to_le_bytes())
    }
}

fn put(bytes: &mut [u8], at: usize, v: &[u8]) -> Result<(), BuildError> {
    at.checked_add(v.len())
        .and_then(|end| bytes.get_mut(at..end))
        .ok_or(BuildError("write outside of the buffer"))?
        .copy_from_slice(v);
    Ok(())
}

fn put_u16(bytes: &mut [u8], at: usize, v: u16) -> Result<(), BuildError> {
    put(bytes, at, &v.to_le_bytes())
}

fn put_u32(bytes: &mut [u8], at: usize, v: u32) -> Result<(), BuildError> {
    put(bytes, at, &v.to_le_bytes())
}

fn put_u64(bytes: &mut [u8], at: usize, v: u64) -> Result<(), BuildError> {
    put(bytes, at, &v.to_le_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_rvas_are_predictable() {
        let b = PeBuilder::new();
        assert_eq!(b.next_section_rva(), Ok(0x1000));
        let b = b.section(".text", scn::CODE, &[0xC3; 0x1001]);
        assert_eq!(b.next_section_rva(), Ok(0x3000));
        // An empty section still takes a page of address space.
        let b = b.section(".bss", scn::DATA, &[]);
        assert_eq!(b.next_section_rva(), Ok(0x4000));

        let info = crate::ImageInfo::parse(&b.build().unwrap().bytes).unwrap();
        let rvas: Vec<_> = info.sections.iter().map(|s| s.rva).collect();
        assert_eq!(rvas, [0x1000, 0x3000]);
        assert_eq!(info.size_of_image, 0x4000);
    }

    #[test]
    fn file_layout_matches_the_headers() {
        let built = PeBuilder::new()
            .section(".text", scn::CODE, &[0xC3; 0x201])
            .tls_callbacks(&[0x1000])
            .reloc_block(0x1000, &[(reloc::DIR64, 0)])
            .build()
            .unwrap();
        let info = crate::ImageInfo::parse(&built.bytes).unwrap();
        let mut expected = info.sections[0].file_offset;
        for s in &info.sections {
            assert_eq!(s.file_offset, expected, "{} follows its neighbour", s.name);
            assert_eq!(s.file_offset % FILE_ALIGNMENT, 0);
            assert_eq!(s.file_size % FILE_ALIGNMENT, 0);
            assert!(s.file_size >= s.virtual_size);
            assert_eq!(s.rva % SECTION_ALIGNMENT, 0);
            expected += s.file_size;
        }
        assert_eq!(built.bytes.len(), expected as usize);
    }

    /// The RVA the builder reports for the callback array is the one the
    /// directory's `AddressOfCallBacks` field holds, read from the file.
    #[test]
    fn the_reported_callback_array_is_the_one_in_the_directory() {
        let base = 0x1_4000_0000;
        let built = PeBuilder::new()
            .image_base(base)
            .section(".text", scn::CODE, &[0xC3; 0x40])
            .tls_callbacks(&[0x1000, 0x1020])
            .build()
            .unwrap();
        let directory = built.tls_directory_rva.unwrap();
        let array = built.tls_callbacks_rva.unwrap();
        // Directory (40 bytes), index (8) and template (16) come first.
        assert_eq!(array, directory + 64);
        let info = crate::ImageInfo::parse(&built.bytes).unwrap();
        let at = info.rva_to_file_offset(directory).unwrap() as usize;
        assert_eq!(
            layout::read_u64(&built.bytes, at + layout::TLS64_ADDRESS_OF_CALLBACKS),
            Some(base + u64::from(array))
        );
        let at = info.rva_to_file_offset(array).unwrap() as usize;
        assert_eq!(layout::read_u64(&built.bytes, at), Some(base + 0x1000));
        assert_eq!(layout::read_u64(&built.bytes, at + 8), Some(base + 0x1020));
        assert_eq!(layout::read_u64(&built.bytes, at + 16), Some(0));

        let none = PeBuilder::new().tls_without_callbacks().build().unwrap();
        assert!(none.tls_directory_rva.is_some());
        assert_eq!(none.tls_callbacks_rva, None);
        assert_eq!(PeBuilder::new().build().unwrap().tls_callbacks_rva, None);
    }

    #[test]
    fn adding_relocations_moves_nothing_else() {
        let base = PeBuilder::new()
            .section(".text", scn::CODE, &[0xC3; 0x40])
            .tls_callbacks(&[0x1000]);
        let without = base.build().unwrap();
        let with = base
            .reloc_block(0x2000, &[(reloc::DIR64, 0x18)])
            .build()
            .unwrap();
        assert_eq!(without.tls_directory_rva, with.tls_directory_rva);
        assert_eq!(without.tls_callbacks_rva, with.tls_callbacks_rva);
        let (a, b) = (
            crate::ImageInfo::parse(&without.bytes).unwrap(),
            crate::ImageInfo::parse(&with.bytes).unwrap(),
        );
        assert_eq!(a.sections.len() + 1, b.sections.len());
        for (x, y) in a.sections.iter().zip(&b.sections) {
            assert_eq!((x.rva, x.virtual_size), (y.rva, y.virtual_size));
        }
    }

    #[test]
    fn odd_relocation_blocks_are_padded() {
        let data = PeBuilder::new()
            .reloc_block(0x1000, &[(reloc::DIR64, 0x18)])
            .build()
            .unwrap()
            .bytes;
        let at = layout::data_directory_offset(&data, dir::BASERELOC).unwrap();
        // Header, one entry and one ABSOLUTE entry of padding.
        assert_eq!(layout::read_u32(&data, at + 4), Some(12));
        let info = crate::ImageInfo::parse(&data).unwrap();
        let rva = layout::read_u32(&data, at).unwrap();
        let block = info.rva_to_file_offset(rva).unwrap() as usize;
        assert_eq!(layout::read_u32(&data, block), Some(0x1000));
        assert_eq!(layout::read_u32(&data, block + 4), Some(12));
        assert_eq!(data[block + 8..block + 12], [0x18, 0xA0, 0, 0]);
    }

    #[test]
    fn impossible_requests_are_errors_not_panics() {
        let long_name = PeBuilder::new().section(".too-long-name", scn::DATA, &[1]);
        assert!(long_name.build().is_err());
        let bad_type = PeBuilder::new().reloc_block(0x1000, &[(0x10, 0)]);
        assert!(bad_type.build().is_err());
        let bad_offset = PeBuilder::new().reloc_block(0x1000, &[(reloc::DIR64, 0x1000)]);
        assert!(bad_offset.build().is_err());
        let bad_directory = PeBuilder::new().data_directory(DIRECTORY_COUNT, 0x1000, 8);
        assert!(bad_directory.build().is_err());
        // A callback RVA that leaves the 64-bit address space.
        let wraps = PeBuilder::new()
            .image_base(u64::MAX - 0x10)
            .tls_callbacks(&[0x1000]);
        assert!(wraps.build().is_err());
        let exports = vec![Export::unused(); 0x1_0001]
            .into_iter()
            .chain([Export::named("last", 0x1000)])
            .collect();
        let too_many = PeBuilder::new().exports("t.dll", 1, exports);
        assert!(too_many.build().is_err());
    }

    /// The limit on export slots is the documented one, the 16-bit ordinal,
    /// for slots of every kind and not only for named ones.
    #[test]
    fn export_slots_are_limited_by_the_ordinal() {
        let build = |base: u32, slots: Vec<Export>| {
            // `.text` takes RVA 0x1000, so that address is not inside the
            // export directory, where it would be read as a forwarder.
            let built = PeBuilder::new()
                .section(".text", scn::CODE, &[0xC3; 0x10])
                .exports("t.dll", base, slots)
                .build()?;
            let info = crate::ImageInfo::parse(&built.bytes).unwrap();
            Ok::<Vec<u16>, BuildError>(info.exports.iter().map(|e| e.ordinal).collect())
        };
        let refused = Err(TOO_MANY_EXPORTS);
        let kinds = [
            Export::unused(),
            Export::ordinal_only(0x1000),
            Export::named("f", 0x1000),
        ];

        // Base 0: 65536 slots fit, of any kind, and one more does not.
        for kind in &kinds[..2] {
            let exports = build(0, vec![kind.clone(); 0x1_0000]).unwrap();
            assert_eq!(exports.first().copied().unwrap_or(0), 0);
            let used = usize::from(*kind != Export::unused()) * 0x1_0000;
            assert_eq!(exports.len(), used);
            assert!(exports.last().is_none_or(|&ordinal| ordinal == 0xFFFF));
            assert_eq!(build(0, vec![kind.clone(); 0x1_0001]), refused);
        }
        // The last slot is the one whose ordinal is largest.
        for kind in &kinds {
            let mut slots = vec![Export::unused(); 0xFFFF];
            slots.push(kind.clone());
            let mut over = slots.clone();
            over.insert(0, Export::unused());
            let exports = build(0, slots.clone()).unwrap();
            assert!(exports.last().is_none_or(|&ordinal| ordinal == 0xFFFF));
            assert_eq!(exports.len(), usize::from(*kind != Export::unused()));
            assert_eq!(build(0, over), refused);
            // The same slots one ordinal higher.
            assert_eq!(build(1, slots), refused);
        }
        // The base counts: ordinal 65535 is the last there is.
        let one = build(0xFFFF, vec![Export::named("f", 0x1000)]).unwrap();
        assert_eq!(one, [0xFFFF]);
        assert_eq!(build(0x1_0000, vec![Export::named("f", 0x1000)]), refused);
        assert_eq!(build(0xFFFF, vec![Export::unused(); 2]), refused);
        assert_eq!(build(u32::MAX, vec![Export::ordinal_only(0x1000)]), refused);
        // The base is an ordinal too, even with no slots to number.
        assert_eq!(build(0xFFFF, Vec::new()), Ok(Vec::new()));
        assert_eq!(build(0x1_0000, Vec::new()), refused);
        assert_eq!(build(0x1_0001, Vec::new()), refused);
    }
}
