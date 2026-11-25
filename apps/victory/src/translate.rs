//! The offline batch: every shader found goes through the translator, and
//! what it cannot handle yet is tallied (decision 0003).

//!
//! Everything up to the emitted DXIL runs on any host. The two checks
//! after it need Windows: `--validate` loads `dxil.dll` and `--pipelines`
//! (and `VICTORY_PAIRS`) creates D3D12 pipelines. On another host they are
//! refused before anything is read.

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use victory_shader::emit;
#[cfg(windows)]
use victory_shader::emit::host::{self, Host};
#[cfg(windows)]
use victory_shader::emit::validator::Validator;
use victory_shader::structure::{self, Atom, LaneExit};
use victory_shader::{
    Container, FourCC, HwStage, NativeProgram, RootType, ShaderError, ir, native_programs,
};
use victory_shader::{io, resource};

use crate::shader::{MAX_FILE, Skipped, Source, sources, walk};

#[derive(Default)]
struct Tally {
    containers: usize,
    duplicates: usize,
    programs: usize,
    translated: usize,
    instructions: usize,
    structured: usize,
    vector: usize,
    guarded: usize,
    /// Guards that read a mask the pass cannot see into.
    opaque: usize,
    lane_branches: usize,
    uniform_branches: usize,
    tracked: usize,
    loaded: usize,
    built: usize,
    constant: usize,
    /// Descriptors and scalar accesses with a computed address term.
    dynamic: usize,
    scalar: usize,
    used: usize,
    /// Used-resource entries whose root parameter no descriptor traces
    /// to, by resource and root type.
    untraced_used: BTreeMap<String, usize>,
    /// Root parameters descriptors trace to that no used entry names.
    unlisted: usize,
    /// Vertex and pixel programs, and those whose I/O was raised.
    io_programs: usize,
    io_raised: usize,
    interpolated: usize,
    vertex_values: usize,
    /// Exports by target kind.
    exports: BTreeMap<&'static str, usize>,
    /// Exports still packed as f16 pairs after the export patch.
    compressed: usize,
    /// Vertex and pixel programs every earlier stage handled, and how far
    /// emission got with them.
    emit_programs: usize,
    emitted: usize,
    validated: usize,
    pipelines: usize,
    /// The same for compute programs.
    cs_programs: usize,
    cs_emitted: usize,
    cs_validated: usize,
    cs_pipelines: usize,
    /// The same for mesh programs.
    ms_programs: usize,
    ms_emitted: usize,
    ms_validated: usize,
    ms_pipelines: usize,
    /// Reason → programs stopped by it, and the first one.
    failed: BTreeMap<String, (usize, String)>,
    /// With `--list`: one line per program and outcome.
    list: bool,
    lines: Vec<String>,
}

impl Tally {
    fn fail(&mut self, reason: String, example: impl FnOnce() -> String) {
        if self.list {
            let e = example();
            self.lines.push(format!("{e}  {reason}"));
            self.failed.entry(reason).or_insert((0, e)).0 += 1;
            return;
        }
        self.failed
            .entry(reason)
            .or_insert_with(|| (0, example()))
            .0 += 1;
    }

    fn merge(&mut self, other: Tally) {
        self.containers += other.containers;
        self.duplicates += other.duplicates;
        self.programs += other.programs;
        self.translated += other.translated;
        self.instructions += other.instructions;
        self.structured += other.structured;
        self.vector += other.vector;
        self.guarded += other.guarded;
        self.opaque += other.opaque;
        self.lane_branches += other.lane_branches;
        self.uniform_branches += other.uniform_branches;
        self.tracked += other.tracked;
        self.loaded += other.loaded;
        self.built += other.built;
        self.constant += other.constant;
        self.dynamic += other.dynamic;
        self.scalar += other.scalar;
        self.used += other.used;
        for (k, n) in other.untraced_used {
            *self.untraced_used.entry(k).or_default() += n;
        }
        self.unlisted += other.unlisted;
        self.io_programs += other.io_programs;
        self.io_raised += other.io_raised;
        self.interpolated += other.interpolated;
        self.vertex_values += other.vertex_values;
        self.compressed += other.compressed;
        self.emit_programs += other.emit_programs;
        self.emitted += other.emitted;
        self.validated += other.validated;
        self.pipelines += other.pipelines;
        self.cs_programs += other.cs_programs;
        self.cs_emitted += other.cs_emitted;
        self.cs_validated += other.cs_validated;
        self.cs_pipelines += other.cs_pipelines;
        self.ms_programs += other.ms_programs;
        self.ms_emitted += other.ms_emitted;
        self.ms_validated += other.ms_validated;
        self.ms_pipelines += other.ms_pipelines;
        for (k, n) in other.exports {
            *self.exports.entry(k).or_default() += n;
        }
        self.lines.extend(other.lines);
        for (reason, (n, example)) in other.failed {
            self.failed.entry(reason).or_insert((0, example)).0 += n;
        }
    }
}

/// Why a program of a stage without a host counterpart here stops.
const OTHER_STAGE: &str = "emit: not a vertex, pixel, compute or mesh program";

/// Emitted shaders by label, shared between threads.
type Kept = std::sync::Arc<Mutex<Vec<(String, emit::HostShader)>>>;

/// The optional checks after emission, one set per thread.
struct Checks {
    /// Shaders named in `VICTORY_PAIRS` (`vs+ps,...`, label substrings),
    /// kept to build those pipelines after the walk.
    keep: Option<Kept>,
    #[cfg(windows)]
    validator: Option<Validator>,
    #[cfg(windows)]
    host: Option<(Host, emit::HostShader)>,
}

/// What `translate` was asked to do besides translating.
#[derive(Default)]
pub struct Options {
    /// Check emitted DXIL with `dxil.dll`'s validator.
    pub validate: bool,
    /// Create a host D3D12 pipeline for each emitted shader.
    pub pipelines: bool,
    /// One line per program and outcome.
    pub list: bool,
    /// Worker threads; `None` for every logical core.
    pub jobs: Option<usize>,
    /// `VICTORY_PAIRS`: (vertex or mesh, pixel) label substrings whose
    /// pipelines are created after the walk.
    pub pairs: Vec<(String, String)>,
}

impl Options {
    /// The pairs `VICTORY_PAIRS` names (`vs+ps,...`).
    pub fn pairs_of(value: &str) -> Vec<(String, String)> {
        value
            .split(',')
            .filter_map(|p| p.split_once('+'))
            .map(|(v, p)| (v.trim().to_owned(), p.trim().to_owned()))
            .collect()
    }

    /// Refuses what this host cannot do, before anything is read: the
    /// validator is a Windows DLL and pipelines need a D3D12 device.
    fn check_host(&self) -> Result<()> {
        #[cfg(not(windows))]
        for (asked, what, why) in [
            (self.validate, "--validate", "dxil.dll is a Windows library"),
            (self.pipelines, "--pipelines", "it creates D3D12 pipelines"),
            (
                !self.pairs.is_empty(),
                "VICTORY_PAIRS",
                "it creates D3D12 pipelines",
            ),
        ] {
            anyhow::ensure!(!asked, "{what} needs a Windows host ({why})");
        }
        Ok(())
    }
}

pub fn run(paths: Vec<PathBuf>, options: &Options, out: &mut dyn Write) -> Result<()> {
    options.check_host()?;
    let paths = crate::default_paths(paths)?;
    scan(&paths, options, MAX_FILE, out)
}

/// [`run`] on `paths` as they are, reading no file above `max_file` bytes.
fn scan(paths: &[PathBuf], options: &Options, max_file: u64, out: &mut dyn Write) -> Result<()> {
    options.check_host()?;
    let &Options {
        validate,
        pipelines,
        list,
        jobs,
        ref pairs,
    } = options;
    #[cfg(windows)]
    let dll = validate.then(Validator::default_path).flatten();
    #[cfg(windows)]
    if validate {
        Validator::load(dll.as_deref()).map_err(anyhow::Error::msg)?;
        writeln!(
            out,
            "validator    {}",
            dll.as_ref()
                .map_or("dxil.dll".into(), |p| p.display().to_string())
        )?;
    }
    #[cfg(windows)]
    if pipelines {
        Host::new(false).map_err(anyhow::Error::msg)?;
    }
    // Refused above.
    #[cfg(not(windows))]
    let _ = (validate, pipelines);
    let kept = std::sync::Arc::new(Mutex::new(Vec::new()));
    let threads =
        jobs.unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
    let seen = Mutex::new(HashSet::new());
    let total = Mutex::new(Tally::default());
    let mut skipped = Skipped::default();
    // One file is in memory at a time; its sources are shared out.
    walk(paths, max_file, &mut skipped, |path, data| {
        let jobs = sources(data);
        let next = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..threads.min(jobs.len()) {
                s.spawn(|| {
                    let mut t = Tally {
                        list,
                        ..Tally::default()
                    };
                    let mut checks = Checks {
                        keep: (!pairs.is_empty()).then(|| kept.clone()),
                        #[cfg(windows)]
                        validator: validate
                            .then(|| Validator::load(dll.as_deref()).ok())
                            .flatten(),
                        #[cfg(windows)]
                        host: pipelines
                            .then(|| Host::new(false).ok().map(|h| (h, host::stub_ps())))
                            .flatten(),
                    };
                    while let Some(job) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                        translate(job, path, &seen, &mut t, &mut checks);
                    }
                    lock(&total).merge(t);
                });
            }
        });
        Ok(())
    })?;
    let t = total.into_inner().unwrap_or_else(|e| e.into_inner());
    skipped.report(max_file, out)?;
    writeln!(
        out,
        "containers   {} ({} duplicates skipped)",
        t.containers, t.duplicates
    )?;
    writeln!(
        out,
        "programs     {} translated of {} ({} IR instructions)",
        t.translated, t.programs, t.instructions
    )?;
    writeln!(
        out,
        "structured   {} of {} ({} vector instructions, {} guarded, {} on opaque masks; \
         branches {} per lane, {} uniform)",
        t.structured,
        t.translated,
        t.vector,
        t.guarded,
        t.opaque,
        t.lane_branches,
        t.uniform_branches
    )?;
    writeln!(
        out,
        "resources    {} of {} tracked (descriptors {} loaded, {} built, {} constant; \
         {} scalar accesses; {} with a computed term)",
        t.tracked, t.translated, t.loaded, t.built, t.constant, t.scalar, t.dynamic
    )?;
    writeln!(
        out,
        "  used-resource entries {}: {} not traced {:?}; {} traced parameters not listed",
        t.used,
        t.untraced_used.values().sum::<usize>(),
        t.untraced_used,
        t.unlisted
    )?;
    writeln!(
        out,
        "I/O          {} of {} vertex and pixel programs raised ({} interpolations, \
         {} vertex values; exports {:?}, {} still f16 pairs)",
        t.io_raised, t.io_programs, t.interpolated, t.vertex_values, t.exports, t.compressed
    )?;
    writeln!(
        out,
        "DXIL         {} emitted of {} vertex and pixel programs; {} validated, {} pipelines",
        t.emitted, t.emit_programs, t.validated, t.pipelines
    )?;
    writeln!(
        out,
        "             {} emitted of {} compute programs; {} validated, {} pipelines",
        t.cs_emitted, t.cs_programs, t.cs_validated, t.cs_pipelines
    )?;
    writeln!(
        out,
        "             {} emitted of {} mesh programs; {} validated, {} pipelines",
        t.ms_emitted, t.ms_programs, t.ms_validated, t.ms_pipelines
    )?;
    #[cfg(windows)]
    if !pairs.is_empty() {
        let host = Host::new(false).map_err(anyhow::Error::msg)?;
        let kept = lock(&kept);
        let find = |name: &str| kept.iter().find(|(l, _)| l.contains(name));
        for (v, p) in pairs {
            let made = match (find(v), find(p)) {
                (Some((_, vs)), Some((_, ps))) => if vs.stage == HwStage::Ms {
                    host.mesh_pipeline(vs, ps)
                } else {
                    host.pipeline(vs, ps)
                }
                .map(|_| "pipeline created".to_owned())
                .unwrap_or_else(|e| e),
                _ => "not emitted".to_owned(),
            };
            writeln!(out, "pair {v} + {p}: {made}")?;
        }
    }
    let mut lines = t.lines;
    lines.sort();
    for l in lines {
        writeln!(out, "{l}")?;
    }
    let mut failed: Vec<_> = t.failed.into_iter().collect();
    failed.sort_by_key(|(_, (n, _))| std::cmp::Reverse(*n));
    for (reason, (n, example)) in failed {
        writeln!(out, "  {n:>7}  {reason}  (e.g. {example})")?;
    }
    Ok(())
}

fn translate(
    source: &Source,
    path: &Path,
    seen: &Mutex<HashSet<blake3::Hash>>,
    t: &mut Tally,
    checks: &mut Checks,
) {
    let example = || format!("{}{}", path.display(), source.label());
    let Some(bytes) = source.bytes() else {
        return t.fail("corrupt record".into(), example);
    };
    t.containers += 1;
    let Ok(container) = Container::parse(&bytes) else {
        return t.fail("no DXIL part (SM5 or older)".into(), example);
    };
    let Some(dxil) = container.part(FourCC::DXIL).map(|p| p.data) else {
        return t.fail("no DXIL part (SM5 or older)".into(), example);
    };
    if !lock(seen).insert(blake3::hash(dxil)) {
        t.duplicates += 1;
        return;
    }
    let programs = match native_programs(dxil) {
        Ok(p) => p,
        Err(e) => return t.fail(e.to_string(), example),
    };
    for p in programs {
        t.programs += 1;
        match p
            .patched_code()
            .and_then(|code| ir::build(&code, p.wave64, p.vgprs))
        {
            Ok(program) => {
                t.translated += 1;
                t.instructions += program.blocks.iter().map(|b| b.insts.len()).sum::<usize>();
                // Per-lane descriptors are traced on the per-lane form; its
                // failure is listed after the resources' and I/O's.
                let structured = structure::structure(&program);
                let lanes = structured.as_ref().ok();
                let res = match resource::track(&program, &p.layout, p.stage, lanes) {
                    Ok(r) => {
                        tally_resources(&p, &r, t);
                        Some(r)
                    }
                    Err(ShaderError::Unsupported { offset, what }) => {
                        t.fail(format!("resources: {what}"), || {
                            format!("{} {:?} at {offset:#x}", example(), p.stage)
                        });
                        None
                    }
                    Err(e) => {
                        t.fail(e.to_string(), || format!("{} {:?}", example(), p.stage));
                        None
                    }
                };
                let graphics = matches!(p.stage, HwStage::Vs | HwStage::Ps);
                let compute = p.stage == HwStage::Cs;
                let mut raised = None;
                if graphics || compute || p.stage == HwStage::Ms {
                    t.io_programs += usize::from(graphics);
                    match io::raise(&program, &p) {
                        Ok(r) => {
                            if graphics {
                                tally_io(&r, t);
                            }
                            raised = Some(r);
                        }
                        Err(ShaderError::Unsupported { offset, what }) => t
                            .fail(format!("I/O: {what}"), || {
                                format!("{} {:?} at {offset:#x}", example(), p.stage)
                            }),
                        Err(e) => t.fail(e.to_string(), || format!("{} {:?}", example(), p.stage)),
                    }
                } else {
                    // Hull, domain and geometry programs: the runtime's
                    // translator takes none of them (`emit::translate`),
                    // and nothing below would say that they stop here.
                    t.fail(OTHER_STAGE.into(), || {
                        format!("{} {:?}", example(), p.stage)
                    });
                }
                let structured = match structured {
                    Ok(s) => {
                        tally_structured(&program, &s, t);
                        Some(s)
                    }
                    Err(e) => {
                        let reason = match e {
                            ShaderError::Unsupported { what, .. } => format!("structure: {what}"),
                            ShaderError::Isa { reason, .. } => format!("structure: {reason}"),
                            e => e.to_string(),
                        };
                        t.fail(reason, || format!("{} {:?}", example(), p.stage));
                        None
                    }
                };
                if let (Some(r), Some(i), Some(s)) = (res, raised, structured) {
                    let label = || format!("{} {:?}", example(), p.stage);
                    *counter(t, p.stage, 0) += 1;
                    match emit::emit(&container, &p, &program, &s, &i, &r) {
                        Ok(shader) => {
                            *counter(t, p.stage, 1) += 1;
                            check(shader, t, checks, label);
                        }
                        Err(ShaderError::Unsupported { offset, what }) => t
                            .fail(format!("emit: {what}"), || {
                                format!("{} at {offset:#x}", label())
                            }),
                        Err(e) => t.fail(format!("emit: {e}"), label),
                    }
                }
            }
            Err(e) => {
                let reason = match e {
                    ShaderError::Unsupported { what, .. } => format!("unsupported: {what}"),
                    e => e.to_string(),
                };
                t.fail(reason, || format!("{} {:?}", example(), p.stage));
            }
        }
    }
}

/// Validates an emitted shader if asked to.
fn check(shader: emit::HostShader, t: &mut Tally, checks: &mut Checks, label: impl Fn() -> String) {
    if let Some(keep) = &checks.keep {
        lock(keep).push((label(), shader.clone()));
    }
    // `VICTORY_EMIT_DUMP` with `--list`: every emitted shader as a file.
    if t.list
        && let Ok(dir) = std::env::var("VICTORY_EMIT_DUMP")
    {
        let name: String = label()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let _ = std::fs::write(
            format!("{dir}/{}.dxil", &name[name.len().saturating_sub(100)..]),
            &shader.dxil,
        );
    }
    #[cfg(windows)]
    if let Some((h, stub_ps)) = &checks.host {
        let made = match shader.stage {
            HwStage::Vs => h.pipeline(&shader, stub_ps).map(drop),
            HwStage::Cs => h.compute_pipeline(&shader).map(drop),
            HwStage::Ms => h.mesh_pipeline(&shader, stub_ps).map(drop),
            _ => host::stub_vs(&shader).and_then(|vs| h.pipeline(&vs, &shader).map(drop)),
        };
        match made {
            Ok(()) => {
                *counter(t, shader.stage, 3) += 1;
                if t.list {
                    t.lines.push(format!("{}  pipeline created", label()));
                }
            }
            Err(e) => t.fail(format!("pipeline: {e}"), &label),
        }
    }
    #[cfg(windows)]
    validate(shader, t, checks, label);
}

/// Has the validator check an emitted shader, if one was asked for.
#[cfg(windows)]
fn validate(
    mut shader: emit::HostShader,
    t: &mut Tally,
    checks: &mut Checks,
    label: impl Fn() -> String,
) {
    let Some(v) = checks.validator.as_mut() else {
        return;
    };
    match v.validate(&mut shader.dxil) {
        Ok(()) => {
            *counter(t, shader.stage, 2) += 1;
            if t.list {
                t.lines.push(format!("{}  validated", label()));
            }
        }
        Err(text) => {
            let first = text
                .lines()
                .find(|l| l.contains("error"))
                .unwrap_or(text.lines().next().unwrap_or(""))
                .to_owned();
            if let Ok(dir) = std::env::var("VICTORY_EMIT_DUMP") {
                let n = t
                    .failed
                    .get(&format!("validator: {first}"))
                    .map_or(0, |f| f.0);
                if n < 3 {
                    let name = format!(
                        "{dir}/{}_{n}",
                        first
                            .chars()
                            .filter(char::is_ascii_alphanumeric)
                            .take(40)
                            .collect::<String>()
                    );
                    let _ = std::fs::write(format!("{name}.dxil"), &shader.dxil);
                    let _ = std::fs::write(format!("{name}.txt"), format!("{}\n{text}", label()));
                }
            }
            t.fail(format!("validator: {first}"), label);
        }
    }
}

/// Emission counts of `stage`'s kind: 0 programs, 1 emitted, 2 validated,
/// 3 pipelines.
fn counter(t: &mut Tally, stage: HwStage, k: usize) -> &mut usize {
    let [a, b, c, d] = match stage {
        HwStage::Cs => [
            &mut t.cs_programs,
            &mut t.cs_emitted,
            &mut t.cs_validated,
            &mut t.cs_pipelines,
        ],
        HwStage::Ms => [
            &mut t.ms_programs,
            &mut t.ms_emitted,
            &mut t.ms_validated,
            &mut t.ms_pipelines,
        ],
        _ => [
            &mut t.emit_programs,
            &mut t.emitted,
            &mut t.validated,
            &mut t.pipelines,
        ],
    };
    [a, b, c, d].into_iter().nth(k).expect("0 to 3")
}

fn tally_io(r: &io::Io, t: &mut Tally) {
    t.io_raised += 1;
    for (_, i) in &r.interps {
        match i {
            io::Interp::Interpolated { .. } => t.interpolated += 1,
            io::Interp::Vertex { .. } => t.vertex_values += 1,
        }
    }
    for e in &r.exports {
        let kind = match e.target {
            io::Target::Color(_) => "color",
            io::Target::Depth => "depth",
            io::Target::Null => "null",
            io::Target::Position(_) => "position",
            io::Target::Param { .. } => "param",
            io::Target::Unread => "unread param",
            io::Target::Primitive => "primitive",
            io::Target::PrimitiveParam { .. } => "primitive param",
        };
        *t.exports.entry(kind).or_default() += 1;
        t.compressed += usize::from(e.compressed);
    }
}

fn tally_resources(p: &NativeProgram, r: &resource::Resources, t: &mut Tally) {
    t.tracked += 1;
    t.scalar += r.scalar.len();
    t.dynamic += r.scalar.iter().filter(|s| s.1.dynamic.is_some()).count();
    let address = |load| r.scalar.iter().find(|s| s.0 == load).map(|s| s.1);
    let mut traced = HashSet::new();
    for leaf in r.descriptors.iter().flat_map(|d| d.source.leaves()) {
        let a = match *leaf {
            resource::Source::Loaded { load, .. } | resource::Source::Fetched { load, .. } => {
                t.loaded += 1;
                address(load)
            }
            resource::Source::Built { address, .. } => {
                t.built += 1;
                t.dynamic += usize::from(address.dynamic.is_some());
                Some(address)
            }
            resource::Source::Constant(_) => {
                t.constant += 1;
                None
            }
            resource::Source::Phi { .. }
            | resource::Source::Select { .. }
            | resource::Source::Lanes { .. }
            | resource::Source::LanePhi { .. } => {
                unreachable!("leaves are no phis, selects or per-lane choices")
            }
        };
        // Heaps hold dynamic resources, which no used entry lists; code
        // tables are no root parameter.
        if let Some(a) = a
            && a.param != resource::CODE
            && !matches!(
                p.layout[a.param].root,
                RootType::ResourceHeap | RootType::SamplerHeap
            )
        {
            traced.insert(p.layout[a.param].user_data);
        }
    }
    // Root constants are listed as a CBV, read straight from the SGPRs.
    traced.extend(
        r.params
            .iter()
            .map(|&(_, param, _)| &p.layout[param])
            .filter(|l| l.root == RootType::Constants)
            .map(|l| l.user_data),
    );
    let listed: HashSet<u8> = p.used.iter().map(|u| u.user_data).collect();
    t.used += p.used.len();
    // Immediate samplers are literals in the code: the distinct sampler
    // values (decoded) are matched against the listed entries, which do not
    // say which value is whose.
    let mut values: Vec<emit::StaticSampler> = Vec::new();
    for d in r
        .descriptors
        .iter()
        .filter(|d| d.role == resource::Role::Sampler)
    {
        for leaf in d.source.leaves() {
            if let resource::Source::Constant(words) = leaf
                && let Ok(w) = <[u32; 4]>::try_from(&words[..words.len().min(4)])
            {
                let s = emit::StaticSampler::from_words(w);
                if !values.contains(&s) {
                    values.push(s);
                }
            }
        }
    }
    // Engines often declare equal samplers at several registers, so equal
    // values may stand for several entries, but never more values than
    // entries.
    let immediate = p
        .used
        .iter()
        .filter(|u| u.root == RootType::ImmediateSampler)
        .count();
    let samplers_match = !values.is_empty() && values.len() <= immediate;
    let matched = |u: &&victory_shader::UsedResource| {
        if u.root == RootType::ImmediateSampler {
            samplers_match
        } else {
            traced.contains(&u.user_data)
        }
    };
    for u in p.used.iter().filter(|u| !matched(u)) {
        *t.untraced_used
            .entry(format!("{:?} via {:?}", u.kind, u.root))
            .or_default() += 1;
    }
    t.unlisted += traced.difference(&listed).count();
}

fn tally_structured(program: &ir::Program, s: &structure::Structured, t: &mut Tally) {
    t.structured += 1;
    for (block, guards) in program.blocks.iter().zip(&s.guards) {
        t.vector += block
            .insts
            .iter()
            .filter(|i| i.read(ir::Reg::S(ir::EXEC)).is_some())
            .count();
        for &g in guards.iter().flatten() {
            t.guarded += 1;
            let support = s.bools.support(g);
            t.opaque += usize::from(support.iter().any(|a| matches!(a, Atom::Opaque(_))));
        }
    }
    for exit in &s.exits {
        if let LaneExit::Branch { when, .. } = *exit {
            if s.bools
                .support(when)
                .iter()
                .all(|a| matches!(a, Atom::Uniform(_)))
            {
                t.uniform_branches += 1;
            } else {
                t.lane_branches += 1;
            }
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{
        hull_container, line, pixel_container, report, report_of, sgd5_file, sgd5_record,
        vertex_container, write,
    };

    /// One thread: the tests share the machine.
    fn options() -> Options {
        Options {
            jobs: Some(1),
            ..Options::default()
        }
    }

    /// A folder with a vertex container as it is, a pixel container in a
    /// packed SGD5 record, the same vertex container again and a hull
    /// container.
    fn shaders() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a-vs.bin", &vertex_container());
        let record = sgd5_record("PS_Synthetic_0011aabb_Wrapped", 0x32, &pixel_container());
        write(dir.path(), "b-ps.sgd5", &sgd5_file(&[record]));
        write(dir.path(), "c-vs-again.bin", &vertex_container());
        write(dir.path(), "d-hs.bin", &hull_container());
        dir
    }

    #[test]
    fn translate_tallies_a_synthetic_container() {
        let dir = shaders();
        let text = report_of(|out| run(vec![dir.path().into()], &options(), out));
        assert_eq!(
            line(&text, "files "),
            "files                  4 searched, 0 unreadable, 0 over 2 GiB"
        );
        assert_eq!(
            line(&text, "containers "),
            "containers   4 (1 duplicates skipped)"
        );
        assert!(
            line(&text, "programs ").starts_with("programs     3 translated of 3 ("),
            "{text}"
        );
        assert!(
            line(&text, "I/O ").starts_with("I/O          2 of 2 vertex and pixel programs raised"),
            "{text}"
        );
        // Emitted on every host; nothing was asked to check the result.
        assert_eq!(
            line(&text, "DXIL "),
            "DXIL         2 emitted of 2 vertex and pixel programs; 0 validated, 0 pipelines"
        );
        assert!(!text.contains("validator "), "{text}");
        // The hull program is counted as stopped, with where it is.
        let stopped = line(&text, "        1  emit: ");
        assert!(stopped.contains(OTHER_STAGE), "{text}");
        let hull = dir.path().join("d-hs.bin");
        assert!(
            stopped.ends_with(&format!("(e.g. {}@0x0 Hs)", hull.display())),
            "{text}"
        );
    }

    #[test]
    fn list_names_each_program_that_stopped() {
        let dir = shaders();
        let listing = Options {
            list: true,
            ..options()
        };
        let text = report_of(|out| run(vec![dir.path().into()], &listing, out));
        let hull = dir.path().join("d-hs.bin");
        assert_eq!(
            line(&text, &hull.display().to_string()),
            format!("{}@0x0 Hs  {OTHER_STAGE}", hull.display())
        );
        // Without --list the per-program lines are left out.
        let text = report_of(|out| run(vec![dir.path().into()], &options(), out));
        assert!(
            !text
                .lines()
                .any(|l| l.starts_with(&hull.display().to_string())),
            "{text}"
        );
    }

    #[test]
    fn a_path_that_is_not_there_is_reported_unreadable() {
        let dir = shaders();
        let missing = dir.path().join("not-there");
        let roots = vec![missing.clone(), dir.path().to_path_buf()];
        let text = report_of(|out| run(roots, &options(), out));
        assert_eq!(
            line(&text, "files "),
            "files                  4 searched, 1 unreadable, 0 over 2 GiB"
        );
        line(&text, &format!("  unreadable {}: ", missing.display()));
        assert_eq!(
            line(&text, "containers "),
            "containers   4 (1 duplicates skipped)"
        );
    }

    #[test]
    fn files_over_the_limit_are_reported_and_not_translated() {
        let dir = tempfile::tempdir().unwrap();
        let small = vertex_container();
        let mut large = pixel_container();
        large.resize(small.len() + 1, 0);
        write(dir.path(), "small.bin", &small);
        let large = write(dir.path(), "large.bin", &large);
        let limit = small.len() as u64;
        let text = report_of(|out| scan(&[dir.path().into()], &options(), limit, out));
        assert_eq!(
            line(&text, "files "),
            format!("files                  1 searched, 0 unreadable, 1 over {limit} bytes")
        );
        assert_eq!(
            line(&text, "  too large"),
            format!("  too large {}", large.display())
        );
        assert_eq!(
            line(&text, "containers "),
            "containers   1 (0 duplicates skipped)"
        );
    }

    #[test]
    fn victory_pairs_is_a_list_of_vertex_and_pixel_names() {
        assert_eq!(Options::pairs_of(""), []);
        assert_eq!(
            Options::pairs_of("VS_A+PS_A, VS_B + PS_B,not-a-pair"),
            [
                ("VS_A".to_owned(), "PS_A".to_owned()),
                ("VS_B".to_owned(), "PS_B".to_owned())
            ]
        );
    }

    /// Off Windows the two checks and the pairs are refused before any
    /// file is read: the report stays empty.
    #[cfg(not(windows))]
    #[test]
    fn validate_and_pipelines_need_a_windows_host() {
        let dir = shaders();
        let asked = [
            (
                "--validate",
                Options {
                    validate: true,
                    ..options()
                },
            ),
            (
                "--pipelines",
                Options {
                    pipelines: true,
                    ..options()
                },
            ),
            (
                "VICTORY_PAIRS",
                Options {
                    pairs: Options::pairs_of("a-vs+b-ps"),
                    ..options()
                },
            ),
        ];
        for (what, options) in asked {
            let (text, result) = report(|out| run(vec![dir.path().into()], &options, out));
            let error = result.expect_err(what).to_string();
            assert!(
                error.starts_with(&format!("{what} needs a Windows host (")),
                "{error}"
            );
            assert_eq!(text, "", "{what}");
        }
        // Without them the same folder is translated.
        let (_, result) = report(|out| run(vec![dir.path().into()], &options(), out));
        result.unwrap();
    }

    /// `--pipelines` against the host's device (D3DMetal under Wine): each
    /// emitted shader becomes a pipeline, the vertex program beside a stub
    /// pixel shader and the pixel program beside a stub vertex shader.
    #[cfg(windows)]
    #[test]
    fn translate_creates_a_pipeline_for_a_synthetic_container() {
        let _one_device_test = victory_gpu::test_support::serial();
        if let Err(e) = Host::new(false) {
            assert!(
                !victory_gpu::test_support::require_gpu(),
                "VICTORY_REQUIRE_GPU=1: no host D3D12 device: {e}"
            );
            return crate::testing::skip("no host D3D12 device");
        }
        let dir = shaders();
        let with_pipelines = Options {
            pipelines: true,
            list: true,
            ..options()
        };
        let text = report_of(|out| run(vec![dir.path().into()], &with_pipelines, out));
        assert_eq!(
            line(&text, "DXIL "),
            "DXIL         2 emitted of 2 vertex and pixel programs; 0 validated, 2 pipelines"
        );
        let vs = dir.path().join("a-vs.bin");
        assert_eq!(
            line(&text, &vs.display().to_string()),
            format!("{}@0x0 Vs  pipeline created", vs.display())
        );
        let ps = dir.path().join("b-ps.sgd5");
        let made = line(&text, &ps.display().to_string());
        assert!(made.ends_with(" Ps  pipeline created"), "{text}");
        assert!(!text.contains("pipeline: "), "{text}");
    }

    /// `--validate` with Microsoft's validator, where one can be loaded
    /// (`VICTORY_DXIL`, else the newest Windows SDK's).
    #[cfg(windows)]
    #[test]
    fn translate_validates_a_synthetic_container() {
        let dll = Validator::default_path();
        if let Err(e) = Validator::load(dll.as_deref()) {
            let required = std::env::var("VICTORY_REQUIRE_DXIL").is_ok_and(|v| v.trim() == "1");
            assert!(!required, "VICTORY_REQUIRE_DXIL=1: DXIL validation: {e}");
            // Asked for and not there, the command fails and says why.
            let validating = Options {
                validate: true,
                ..options()
            };
            let dir = shaders();
            let (text, result) = report(|out| run(vec![dir.path().into()], &validating, out));
            assert_eq!(result.expect_err("no validator").to_string(), e);
            assert_eq!(text, "");
            return crate::testing::skip(&format!("DXIL validation: {e}"));
        }
        let dir = shaders();
        let validating = Options {
            validate: true,
            list: true,
            ..options()
        };
        let text = report_of(|out| run(vec![dir.path().into()], &validating, out));
        assert!(text.starts_with("validator    "), "{text}");
        assert_eq!(
            line(&text, "DXIL "),
            "DXIL         2 emitted of 2 vertex and pixel programs; 2 validated, 0 pipelines"
        );
        let vs = dir.path().join("a-vs.bin");
        assert_eq!(
            line(&text, &vs.display().to_string()),
            format!("{}@0x0 Vs  validated", vs.display())
        );
        assert!(!text.contains("validator: "), "{text}");
    }
}
