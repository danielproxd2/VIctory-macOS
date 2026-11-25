//! Survey shader containers: bitcode, console additions and native code;
//! and list shader packs, naming each stage's container where one of the
//! given files holds it.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use victory_game::{sgd5, shaderpack};
use victory_shader::gfx10::{self, Instruction};
use victory_shader::{Container, DxilProgram, FourCC, HwStage, ShaderError, native_programs};

/// Files above this size are not read: a file is searched whole, in
/// memory, and engine archives run to many gigabytes. Such a file is
/// counted and named in the report ([`Skipped`]).
pub(crate) const MAX_FILE: u64 = 2 << 30;

/// What a walk read, and what it could not search and why.
#[derive(Default)]
pub(crate) struct Skipped {
    searched: usize,
    unreadable: Vec<(PathBuf, String)>,
    too_large: Vec<PathBuf>,
}

impl Skipped {
    /// One line of counts, then one line per file or folder left out.
    pub(crate) fn report(&self, max_file: u64, out: &mut dyn Write) -> Result<()> {
        writeln!(
            out,
            "files                  {} searched, {} unreadable, {} over {}",
            self.searched,
            self.unreadable.len(),
            self.too_large.len(),
            size_text(max_file)
        )?;
        for (path, why) in &self.unreadable {
            writeln!(out, "  unreadable {}: {why}", path.display())?;
        }
        for path in &self.too_large {
            writeln!(out, "  too large {}", path.display())?;
        }
        Ok(())
    }
}

/// `bytes` in GiB where that is exact.
fn size_text(bytes: u64) -> String {
    const GIB: u64 = 1 << 30;
    if bytes != 0 && bytes.is_multiple_of(GIB) {
        format!("{} GiB", bytes / GIB)
    } else {
        format!("{bytes} bytes")
    }
}

/// The contents of `path`, or `None` if it holds more than `max` bytes.
/// Never reads more than `max + 1` bytes, whatever the file's length says
/// (a file may grow while it is read).
fn read_bounded(path: &Path, max: u64) -> std::io::Result<Option<Vec<u8>>> {
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    if len > max {
        return Ok(None);
    }
    let mut data = Vec::new();
    // No memory for it is an error to report, not an abort.
    data.try_reserve_exact(usize::try_from(len).map_err(std::io::Error::other)?)
        .map_err(std::io::Error::other)?;
    file.take(max.saturating_add(1)).read_to_end(&mut data)?;
    Ok((data.len() as u64 <= max).then_some(data))
}

/// Calls `each` with every file under `roots` that holds at most
/// `max_file` bytes, and its contents. What cannot be listed or read (a
/// root that does not exist, a folder or file without permission) and
/// what is too large goes into `skipped`, to be reported.
pub(crate) fn walk(
    roots: &[PathBuf],
    max_file: u64,
    skipped: &mut Skipped,
    mut each: impl FnMut(&Path, &[u8]) -> Result<()>,
) -> Result<()> {
    for root in roots {
        for entry in walkdir::WalkDir::new(root) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    let path = e.path().unwrap_or(root).to_path_buf();
                    let why = e
                        .io_error()
                        .map_or_else(|| e.to_string(), ToString::to_string);
                    skipped.unreadable.push((path, why));
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            match read_bounded(path, max_file) {
                Ok(Some(data)) => {
                    skipped.searched += 1;
                    each(path, &data)?;
                }
                Ok(None) => skipped.too_large.push(path.to_path_buf()),
                Err(e) => skipped.unreadable.push((path.to_path_buf(), e.to_string())),
            }
        }
    }
    Ok(())
}

/// A shader container in a file: as is, or packed in an SGD5 record.
pub(crate) enum Source<'a> {
    Raw { offset: usize, data: &'a [u8] },
    Packed(sgd5::Record<'a>),
}

impl Source<'_> {
    /// The container bytes; `None` if a packed record is corrupt.
    pub(crate) fn bytes(&self) -> Option<Cow<'_, [u8]>> {
        match self {
            Source::Raw { data, .. } => Some(Cow::Borrowed(data)),
            Source::Packed(r) => r.unpack().map(Cow::Owned),
        }
    }

    pub(crate) fn label(&self) -> String {
        match self {
            Source::Raw { offset, .. } => format!("@{offset:#x}"),
            Source::Packed(r) => format!("@{:#x} {}", r.offset, r.name.unwrap_or("?")),
        }
    }
}

/// The packed records of SGD5 data, and every container stored as is (the
/// executable keeps some records uncompressed), found by its magic.
pub(crate) fn sources(data: &[u8]) -> Vec<Source<'_>> {
    let mut out: Vec<Source<'_>> = Vec::new();
    if sgd5::is_sgd5(data) {
        out.extend(sgd5::records(data).into_iter().map(Source::Packed));
    }
    let raw = memchr::memmem::find_iter(data, b"DXBC")
        .filter(|&offset| Container::parse(&data[offset..]).is_ok())
        .map(|offset| Source::Raw {
            offset,
            data: &data[offset..],
        });
    out.extend(raw);
    out
}

#[derive(Default)]
struct Totals {
    containers: usize,
    with_dxil_part: usize,
    with_bitcode: usize,
    standard_layout: usize,
    xdat: usize,
    xbop: usize,
    kinds: BTreeMap<String, usize>,
    with_native: usize,
    native_stages: BTreeMap<HwStage, usize>,
    native_errors: BTreeMap<String, usize>,
    decoded: usize,
    instructions: usize,
    unnamed: BTreeMap<String, usize>,
    decode_errors: BTreeMap<&'static str, usize>,
    files_with_shaders: usize,
    corrupt_records: usize,
    /// A native program's hash instructions -> its container's BLAKE3
    /// (first 8 bytes, hex, as the GPU side logs it) and name.
    programs: HashMap<[u8; 24], String>,
    /// The same, by the hash a shader database stores for the container.
    engine: HashMap<u64, String>,
}

pub fn run(
    paths: Vec<PathBuf>,
    verbose: bool,
    disasm: bool,
    effects: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let paths = crate::default_paths(paths)?;
    scan(&paths, verbose, disasm, effects, MAX_FILE, out)
}

/// [`run`] on `paths` as they are, reading no file above `max_file` bytes.
fn scan(
    paths: &[PathBuf],
    verbose: bool,
    disasm: bool,
    effects: bool,
    max_file: u64,
    out: &mut dyn Write,
) -> Result<()> {
    let mut t = Totals::default();
    let mut skipped = Skipped::default();
    let mut packs = Vec::new();
    walk(paths, max_file, &mut skipped, |path, data| {
        if effects {
            return list_effects(path, data, out);
        }
        if data.starts_with(shaderpack::MAGIC) {
            packs.push((path.to_owned(), data.to_vec()));
            return Ok(());
        }
        let mut found_here = 0;
        let engine: HashMap<usize, u64> = sgd5::databases(data)
            .iter()
            .flat_map(|db| db.shaders.iter().flatten())
            .map(|s| (s.record.offset, s.hash))
            .collect();
        for source in sources(data) {
            let Some(bytes) = source.bytes() else {
                t.corrupt_records += 1;
                if verbose {
                    writeln!(out, "{}{}: corrupt record", path.display(), source.label())?;
                }
                continue;
            };
            let Ok(container) = Container::parse(&bytes) else {
                continue;
            };
            found_here += 1;
            t.containers += 1;
            let dxil = container.part(FourCC::DXIL);
            t.with_dxil_part += usize::from(dxil.is_some());
            let program = dxil.and_then(|p| DxilProgram::parse(p.data).ok());
            let mut listing = Vec::new();
            // Independent of the bitcode: titles may strip it.
            let code = dxil.or(container.part(FourCC::ACCC));
            let native = match code.map(|p| native_programs(p.data)) {
                Some(Ok(programs)) if !programs.is_empty() => {
                    t.with_native += 1;
                    let mut list = Vec::new();
                    // `parse` checked the size field against the data.
                    let size = u32::from_le_bytes(bytes[0x18..0x1c].try_into()?) as usize;
                    let name = match &source {
                        Source::Raw { offset, .. } => {
                            let file = path.file_name().unwrap_or_default();
                            format!("{}@{offset:#x}", file.to_string_lossy())
                        }
                        Source::Packed(r) => r.name.unwrap_or("?").to_owned(),
                    };
                    let id = format!("{}:{name}", &blake3::hash(&bytes[..size]).to_hex()[..16]);
                    if let Source::Packed(r) = &source
                        && let Some(&hash) = engine.get(&r.offset)
                    {
                        t.engine.entry(hash).or_insert_with(|| id.clone());
                    }
                    for n in &programs {
                        if let Some(key) = shaderpack::program_id(&n.code) {
                            t.programs.entry(key).or_insert_with(|| id.clone());
                        }
                        *t.native_stages.entry(n.stage).or_default() += 1;
                        list.push(format!("{:?} {} B", n.stage, n.code.len()));
                        if disasm {
                            listing.push(format!("    {:?} stage I/O: {:?}", n.stage, n.io));
                            listing.extend(n.layout.iter().map(|p| format!("    {p:?}")));
                            listing.extend(n.used.iter().map(|u| format!("    {u:?}")));
                        }
                        match gfx10::decode(&n.code) {
                            Ok(insts) => {
                                t.decoded += 1;
                                t.instructions += insts.len();
                                for i in insts.iter().filter(|i| i.name().is_none()) {
                                    let key = format!("{:?} {:#x}", i.encoding, i.opcode);
                                    *t.unnamed.entry(key).or_default() += 1;
                                }
                                if disasm {
                                    listing.extend(
                                        insts.iter().map(|i| format_instruction(i, n.wave64)),
                                    );
                                }
                            }
                            Err(e) => {
                                let reason = match e {
                                    ShaderError::Isa { reason, .. } => reason,
                                    _ => "other",
                                };
                                *t.decode_errors.entry(reason).or_default() += 1;
                                list.push(format!("({e})"));
                            }
                        }
                    }
                    format!(", native {}", list.join(" "))
                }
                Some(Err(e)) => {
                    *t.native_errors.entry(e.to_string()).or_default() += 1;
                    format!(", native: {e}")
                }
                _ => String::new(),
            };
            let line = match &program {
                Some(p) => {
                    t.with_bitcode += 1;
                    let x = p.xbox_extras();
                    t.standard_layout += usize::from(p.is_standard_layout());
                    t.xdat += usize::from(x.xdat);
                    t.xbop += usize::from(x.xbop);
                    *t.kinds.entry(format!("{:?}", p.kind)).or_default() += 1;
                    format!(
                        "{:?} sm{}.{} bitcode {} B, xdat {}, xbop {}",
                        p.kind,
                        p.shader_model.0,
                        p.shader_model.1,
                        p.bitcode.len(),
                        x.xdat,
                        x.xbop
                    )
                }
                None => "no parsable DXIL program".to_owned(),
            } + &native;
            if verbose {
                writeln!(out, "{}{}: {line}", path.display(), source.label())?;
                for l in &listing {
                    writeln!(out, "{l}")?;
                }
            }
        }
        t.files_with_shaders += usize::from(found_here > 0);
        Ok(())
    })?;
    skipped.report(max_file, out)?;
    writeln!(
        out,
        "containers found       {} (in {} files)",
        t.containers, t.files_with_shaders
    )?;
    writeln!(out, "  with DXIL part       {}", t.with_dxil_part)?;
    writeln!(out, "  with LLVM bitcode    {}", t.with_bitcode)?;
    writeln!(out, "  PC-standard layout   {}", t.standard_layout)?;
    writeln!(out, "  XDAT prefix          {}", t.xdat)?;
    writeln!(out, "  xbop trailer         {}", t.xbop)?;
    for (k, n) in &t.kinds {
        writeln!(out, "  {k:<20} {n}")?;
    }
    writeln!(out, "  with native code     {}", t.with_native)?;
    for (stage, n) in &t.native_stages {
        writeln!(out, "    {:<18} {n}", format!("{stage:?}"))?;
    }
    for (e, n) in &t.native_errors {
        writeln!(out, "  native unreadable    {n}: {e}")?;
    }
    if t.with_native > 0 {
        writeln!(
            out,
            "  decoded programs     {} ({} instructions)",
            t.decoded, t.instructions
        )?;
        for (e, n) in &t.decode_errors {
            writeln!(out, "  decode failed        {n}: {e}")?;
        }
        let mut unnamed: Vec<_> = t.unnamed.iter().collect();
        unnamed.sort_by_key(|&(_, n)| std::cmp::Reverse(*n));
        for (op, n) in unnamed.iter().take(10) {
            writeln!(out, "  opcode not in table  {op} x{n}")?;
        }
    }
    if t.corrupt_records > 0 {
        writeln!(out, "  corrupt records      {}", t.corrupt_records)?;
    }
    if t.containers == 0 && packs.is_empty() {
        writeln!(
            out,
            "(none found: shaders may sit in compressed or encrypted archives)"
        )?;
    }
    for (path, data) in &packs {
        list_pack(path, data, &t, verbose, out)?;
    }
    Ok(())
}

/// A shader pack's pipelines; with `verbose` one line each, naming every
/// stage's container (BLAKE3 prefix and name) found in the other files.
fn list_pack(
    path: &Path,
    data: &[u8],
    t: &Totals,
    verbose: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let pack = match shaderpack::Pack::parse(data) {
        Ok(pack) => pack,
        Err(e) => {
            writeln!(out, "{}: {e}", path.display())?;
            return Ok(());
        }
    };
    let name = |code: &[u8]| {
        let id = shaderpack::program_id(code).and_then(|id| t.programs.get(&id));
        id.map_or("?", String::as_str)
    };
    let hex = |h: u64| format!("{h:016x}");
    let (mut graphics, mut compute, mut stages, mut named, mut keyed) = (0, 0, 0, 0, 0);
    for g in pack.graphics() {
        graphics += 1;
        keyed += usize::from(t.engine.contains_key(&g.shader_hash));
        let mut line = format!(
            "graphics key={} shader={} pipeline={} depth-stencil={} rasterizer={} blend={}",
            hex(g.key),
            hex(g.shader_hash),
            hex(g.pipeline_hash),
            hex(g.depth_stencil_hash),
            hex(g.rasterizer_hash),
            hex(g.blend_hash)
        );
        if let (Some(f), Some((depth, count, topology)), Some((mask, samples, quality))) =
            (g.target_formats(), g.depth_targets_topology(), g.samples())
        {
            let f: Vec<_> = f.iter().map(u8::to_string).collect();
            line += &format!(
                " targets={count}:{} depth={depth} topology={topology} samples={samples}.{quality} mask={mask:#x}",
                f.join(",")
            );
        }
        if let Some(inputs) = g.input_layout() {
            let e: Vec<_> = (inputs.iter())
                .map(|e| {
                    let (s, i, f, slot, o) = (e.semantic, e.index, e.format, e.slot, e.offset);
                    format!("{s}{i}:{f}:{slot}:{o}:{}:{}", e.class, e.step_rate)
                })
                .collect();
            line += &format!(" inputs={}", e.join(","));
        }
        for (stage, s) in shaderpack::STAGES.iter().zip(&g.stages) {
            let Some(s) = s else { continue };
            stages += 1;
            let n = name(s.program);
            named += usize::from(n != "?");
            line += &format!(" {stage}={n}");
        }
        if verbose {
            writeln!(out, "{line}")?;
        }
    }
    let mut compute_keyed = 0;
    for c in pack.compute() {
        compute += 1;
        compute_keyed += usize::from(t.engine.contains_key(&c.shader_hash));
        let n = name(c.stage.program);
        named += usize::from(n != "?");
        stages += 1;
        if verbose {
            writeln!(out, "compute shader={} CS={n}", hex(c.shader_hash))?;
        }
    }
    writeln!(
        out,
        "{}: shader pack ({}, compiler {}, database {:016x}): {graphics} graphics and {compute} compute pipelines",
        path.display(),
        pack.driver(),
        pack.compiler(),
        pack.fingerprint()
    )?;
    writeln!(
        out,
        "  stages with their container found   {named} of {stages}"
    )?;
    writeln!(
        out,
        "  graphics shader hash in a database  {keyed} of {graphics}"
    )?;
    writeln!(
        out,
        "  compute shader hash in a database   {compute_keyed} of {compute}"
    )?;
    Ok(())
}

/// Effect -> technique -> pass -> shaders (name, container hash, the
/// engine's hash) and the state each pass sets, for every database in `data`.
fn list_effects(path: &Path, data: &[u8], out: &mut dyn Write) -> Result<()> {
    for db in sgd5::databases(data) {
        let passes: usize = (db.effects.iter())
            .flat_map(|e| &e.techniques)
            .map(|t| t.passes.len())
            .sum();
        writeln!(
            out,
            "{}@{:#x}: {} effects, {passes} passes",
            path.display(),
            db.start,
            db.effects.len()
        )?;
        let mut containers = HashMap::new();
        for e in &db.effects {
            writeln!(out, "effect {}", e.name)?;
            for t in &e.techniques {
                writeln!(out, "  technique {}", t.name)?;
                for (i, p) in t.passes.iter().enumerate() {
                    let mut parts = Vec::new();
                    for (slot, index) in p.shaders.iter().enumerate() {
                        let Some(index) = *index else { continue };
                        let s = &db.shaders[slot][index];
                        let container = containers.entry((slot, index)).or_insert_with(|| {
                            s.record.unpack().map_or("corrupt".to_owned(), |c| {
                                blake3::hash(&c).to_hex()[..16].to_owned()
                            })
                        });
                        let name = s.record.name.unwrap_or("?");
                        let stage = sgd5::STAGES[slot];
                        parts.push(format!("{stage} {name} {container} {:016x}", s.hash));
                    }
                    let states = [
                        ("rasterizer", p.rasterizer),
                        ("depth-stencil", p.depth_stencil),
                        ("blend", p.blend),
                    ];
                    for (what, bytes) in states {
                        if let Some(b) = bytes {
                            let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
                            parts.push(format!("{what} {hex}"));
                        }
                    }
                    if let Some(r) = p.stencil_ref {
                        parts.push(format!("stencil ref {r:#x}"));
                    }
                    writeln!(out, "    pass {i}: {}", parts.join(", "))?;
                }
            }
        }
    }
    Ok(())
}

fn format_instruction(i: &Instruction, wave64: bool) -> String {
    let words: Vec<_> = i.words.iter().map(|w| format!("{w:08X}")).collect();
    let text = match i.name() {
        Some(_) => i.text(wave64),
        None => format!("<{:?} {:#x}>", i.encoding, i.opcode),
    };
    format!("    {:06x}: {:<44} {text}", i.offset, words.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{
        line, pixel_container, report_of, sgd5_file, sgd5_record, vertex_container, write,
    };

    #[test]
    fn shader_info_reports_a_synthetic_container_and_a_packed_record() {
        let dir = tempfile::tempdir().unwrap();
        // A container as it is, between other bytes (as in an archive).
        let mut raw = vec![0x5A; 0x30];
        raw.extend(vertex_container());
        raw.extend([0xA5; 0x11]);
        let raw = write(dir.path(), "archive.bin", &raw);
        // One in a packed record of an SGD5 file.
        let record = sgd5_record("PS_Synthetic_0011aabb_Wrapped", 0x32, &pixel_container());
        let packed = write(dir.path(), "shaders.sgd5", &sgd5_file(&[record]));
        write(dir.path(), "notes.txt", b"no shader in here");

        let text = report_of(|out| run(vec![dir.path().into()], true, false, false, out));
        assert_eq!(
            line(&text, "files "),
            "files                  3 searched, 0 unreadable, 0 over 2 GiB"
        );
        assert_eq!(
            line(&text, "containers found"),
            "containers found       2 (in 2 files)"
        );
        assert_eq!(line(&text, "  with DXIL part"), "  with DXIL part       2");
        assert_eq!(
            line(&text, "  with native code"),
            "  with native code     2"
        );
        assert_eq!(line(&text, "    Vs").split_whitespace().last(), Some("1"));
        assert_eq!(line(&text, "    Ps").split_whitespace().last(), Some("1"));
        assert!(
            line(&text, "  decoded programs").starts_with("  decoded programs     2 ("),
            "{text}"
        );
        assert!(!text.contains("corrupt"), "{text}");
        // --verbose: one line per container, saying where it is.
        let at_offset = format!("{}@0x30: ", raw.display());
        assert!(line(&text, &at_offset).contains("native Vs "), "{text}");
        let in_record = line(&text, &format!("{}@", packed.display()));
        assert!(
            in_record.contains(" PS_Synthetic_0011aabb_Wrapped: "),
            "{text}"
        );
        assert!(in_record.contains("native Ps "), "{text}");

        // Without --verbose only the summary is printed.
        let quiet = report_of(|out| run(vec![dir.path().into()], false, false, false, out));
        assert!(!quiet.contains("@0x30"), "{quiet}");
        assert_eq!(
            line(&quiet, "containers found"),
            "containers found       2 (in 2 files)"
        );
        // --disasm lists the decoded instructions under each container.
        let listing = report_of(|out| run(vec![raw.clone()], true, true, false, out));
        assert!(listing.contains("s_endpgm"), "{listing}");
    }

    /// A file that starts as a shader pack and ends inside its header is
    /// named with what is wrong with it, after the summary, and is not
    /// searched for containers; `--effects` lists databases only, and says
    /// nothing of a file that holds none.
    #[test]
    fn a_shader_pack_that_does_not_parse_is_named_with_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        let mut cut = shaderpack::MAGIC.to_vec();
        // A container after the magic: found if the file were searched.
        cut.extend(vertex_container());
        cut.truncate(0x50);
        let pack = write(dir.path(), "cut.pack", &cut);
        let why = shaderpack::Pack::parse(&cut).map(drop).unwrap_err();

        let text = report_of(|out| run(vec![dir.path().into()], true, false, false, out));
        assert_eq!(
            line(&text, "containers found"),
            "containers found       0 (in 0 files)"
        );
        assert_eq!(
            text.lines().last(),
            Some(format!("{}: {why}", pack.display()).as_str()),
            "{text}"
        );
        assert!(!text.contains("(none found"), "{text}");

        let effects = report_of(|out| run(vec![dir.path().into()], true, false, true, out));
        assert!(!effects.contains("effect "), "{effects}");
        assert!(!effects.contains(&pack.display().to_string()), "{effects}");
    }

    #[test]
    fn a_corrupt_packed_record_is_counted() {
        let dir = tempfile::tempdir().unwrap();
        let mut record = sgd5_record("VS_Broken_Wrapped", 0x32, &vertex_container());
        // The sizes still agree; the deflate stream no longer unpacks.
        let stream = record.len() - 8;
        record[stream..].fill(0xFF);
        let file = write(dir.path(), "broken.sgd5", &sgd5_file(&[record]));

        let text = report_of(|out| run(vec![file.clone()], true, false, false, out));
        assert_eq!(line(&text, "  corrupt records"), "  corrupt records      1");
        assert_eq!(
            line(&text, "containers found"),
            "containers found       0 (in 0 files)"
        );
        let named = line(&text, &format!("{}@", file.display()));
        assert!(
            named.ends_with("VS_Broken_Wrapped: corrupt record"),
            "{text}"
        );
    }

    #[test]
    fn a_path_that_is_not_there_is_reported_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "vs.bin", &vertex_container());
        let missing = dir.path().join("not-there");

        let roots = vec![missing.clone(), dir.path().to_path_buf()];
        let text = report_of(|out| run(roots, false, false, false, out));
        assert_eq!(
            line(&text, "files "),
            "files                  1 searched, 1 unreadable, 0 over 2 GiB"
        );
        let why = line(&text, &format!("  unreadable {}: ", missing.display()));
        assert!(why.len() > format!("  unreadable {}: ", missing.display()).len());
        // The walk goes on with what it can read.
        assert_eq!(
            line(&text, "containers found"),
            "containers found       1 (in 1 files)"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_files_are_counted() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "readable.bin", &vertex_container());
        let locked = write(dir.path(), "locked.bin", &pixel_container());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&locked).is_ok() {
            // The superuser reads it all the same.
            return crate::testing::skip("this user can read a file without read permission");
        }

        let text = report_of(|out| run(vec![dir.path().into()], false, false, false, out));
        assert_eq!(
            line(&text, "files "),
            "files                  1 searched, 1 unreadable, 0 over 2 GiB"
        );
        line(&text, &format!("  unreadable {}: ", locked.display()));
        assert_eq!(
            line(&text, "containers found"),
            "containers found       1 (in 1 files)"
        );
    }

    #[test]
    fn files_over_the_limit_are_reported_and_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let small = vertex_container();
        let mut large = pixel_container();
        large.resize(small.len() + 1, 0);
        write(dir.path(), "small.bin", &small);
        let large_path = write(dir.path(), "large.bin", &large);
        let limit = small.len() as u64;

        // At the limit a file is read; one byte more and it is not.
        assert_eq!(
            read_bounded(&dir.path().join("small.bin"), limit).unwrap(),
            Some(small)
        );
        assert_eq!(read_bounded(&large_path, limit).unwrap(), None);
        assert_eq!(read_bounded(&large_path, limit + 1).unwrap(), Some(large));

        let text = report_of(|out| scan(&[dir.path().into()], false, false, false, limit, out));
        assert_eq!(
            line(&text, "files "),
            format!("files                  1 searched, 0 unreadable, 1 over {limit} bytes")
        );
        assert_eq!(
            line(&text, "  too large"),
            format!("  too large {}", large_path.display())
        );
        assert_eq!(
            line(&text, "containers found"),
            "containers found       1 (in 1 files)"
        );
    }

    #[test]
    fn the_limit_is_two_gibibytes_and_is_named_so() {
        assert_eq!(MAX_FILE, 2 * 1024 * 1024 * 1024);
        assert_eq!(size_text(MAX_FILE), "2 GiB");
        assert_eq!(size_text(1 << 30), "1 GiB");
        assert_eq!(size_text(1500), "1500 bytes");
        assert_eq!(size_text(0), "0 bytes");
    }
}
