//! `victory doctor`: whether this machine is ready for a run, asked before
//! the first one and without a game.
//!
//! One line per item, each starting with one of four marks:
//!
//! * `ok`: checked, and as a run needs it;
//! * `warning`: checked, and not as on the PC hardware VIctory was written
//!   on, or not usable, but nothing a run stops on (the line says what
//!   follows from it);
//! * `PROBLEM`: checked, and a run needs it otherwise. Any such line makes
//!   the command end with status 1;
//! * `not checked`: could not be asked here, with the reason. The native
//!   macOS build cannot ask Windows anything, so there every such item
//!   reads "run under Wine via tools/macos/victory.sh doctor".
//!
//! What is asked of Windows is in [`device`]; what an answer means is
//! decided here by functions of plain values, which the tests run on every
//! host. Nothing is printed but what the checks found and the paths the
//! configuration names.

// The native build finds nothing of what only Windows can be asked, so it
// constructs none of those findings; what they mean is compiled and tested
// there all the same.
#![cfg_attr(not(windows), allow(dead_code))]

mod capture;
#[cfg(windows)]
mod device;

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;

use self::capture::Logged;

/// The prebuilt argument clamp, as `victory-gpu` embeds it. The file is
/// that crate's; it is read here as well because the crate tells nobody
/// whether its bytecode is current.
#[allow(dead_code)]
#[path = "../../../crates/victory-gpu/src/clamp_bytecode.rs"]
mod clamp_bytecode;
/// The prebuilt depth conversion, likewise.
#[allow(dead_code)]
#[path = "../../../crates/victory-gpu/src/depth_bytecode.rs"]
mod depth_bytecode;

/// The sources the two were built from (`victory-gpu` uses the bytecode
/// only while its `SOURCE_HASH` names the present text).
const CLAMP_SOURCE: &str = include_str!("../../../crates/victory-gpu/src/clamp.hlsl");
const DEPTH_SOURCE: &str = include_str!("../../../crates/victory-gpu/src/depth.hlsl");

/// What an item that needs Windows says in the native build.
pub(crate) const WINE_ONLY: &str = "run under Wine via tools/macos/victory.sh doctor";

/// The `game_dir` older copies of victory.example.toml set, on a drive no
/// Mac has. A victory.toml copied from one still names it.
pub(crate) const EXAMPLE_GAME_DIR: &str = r"D:\victory-data\game";

/// The lines of `victory-gpu`'s device start (`host.rs`) the doctor reads.
pub(crate) const RUNTIME_NO_DEVICE: &str = "no host D3D12 device; command lists will not run";
const RUNTIME_ATOMICS: &str = "host 64-bit atomics on typed heap resources";
const RUNTIME_ATOMICS_NOT_RUN: &str =
    "the host GPU reports 64-bit atomics on typed heap resources and did not run a";
const RUNTIME_ATOMICS_UNCHECKED: &str =
    "64-bit atomics on typed heap resources not checked; the host reports them";

/// How an item came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mark {
    Ok,
    Warning,
    Problem,
    NotChecked,
}

impl Mark {
    /// The word a line starts with.
    pub(crate) fn word(self) -> &'static str {
        match self {
            Mark::Ok => "ok",
            Mark::Warning => "warning",
            Mark::Problem => "PROBLEM",
            Mark::NotChecked => "not checked",
        }
    }
}

/// One line of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Item {
    pub(crate) mark: Mark,
    pub(crate) name: &'static str,
    pub(crate) text: String,
}

impl Item {
    fn new(mark: Mark, name: &'static str, text: impl Into<String>) -> Item {
        Item {
            mark,
            name,
            text: text.into(),
        }
    }

    fn ok(name: &'static str, text: impl Into<String>) -> Item {
        Item::new(Mark::Ok, name, text)
    }

    fn warning(name: &'static str, text: impl Into<String>) -> Item {
        Item::new(Mark::Warning, name, text)
    }

    fn problem(name: &'static str, text: impl Into<String>) -> Item {
        Item::new(Mark::Problem, name, text)
    }

    fn not_checked(name: &'static str, text: impl Into<String>) -> Item {
        Item::new(Mark::NotChecked, name, text)
    }

    /// The line as the report prints it: the mark in a column of its own.
    pub(crate) fn line(&self) -> String {
        format!("{:<11} {}: {}", self.mark.word(), self.name, self.text)
    }
}

/// Writes the report: a line per item and the count of each mark. Returns
/// whether any item is a problem, which is what the exit status says.
pub(crate) fn write_report(items: &[Item], out: &mut dyn Write) -> std::io::Result<bool> {
    for item in items {
        writeln!(out, "{}", item.line())?;
    }
    let count = |mark| items.iter().filter(|i| i.mark == mark).count();
    let (problems, unchecked) = (count(Mark::Problem), count(Mark::NotChecked));
    writeln!(out)?;
    writeln!(
        out,
        "doctor: {} ok, {} warning, {} PROBLEM, {} not checked",
        count(Mark::Ok),
        count(Mark::Warning),
        problems,
        unchecked
    )?;
    if problems > 0 {
        writeln!(
            out,
            "This machine is not ready for a run: see the lines marked PROBLEM."
        )?;
    } else if unchecked > 0 {
        writeln!(
            out,
            "No problem was found in what was checked; the lines marked `not checked` say what was left out."
        )?;
    } else {
        writeln!(out, "No problem was found: every item was checked.")?;
    }
    Ok(problems > 0)
}

/// What the process runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostKind {
    /// Windows itself.
    Windows,
    /// Wine, with the version its ntdll gives.
    Wine(Option<String>),
    /// Neither: the native build of another system.
    #[cfg_attr(windows, allow(dead_code))]
    Other,
}

impl HostKind {
    /// Whether paths are Windows paths and Windows can be asked.
    fn windows(&self) -> bool {
        !matches!(self, HostKind::Other)
    }
}

pub(crate) fn host_item(host: &HostKind) -> Item {
    match host {
        HostKind::Windows => Item::ok("host", "Windows"),
        HostKind::Wine(Some(version)) => Item::ok("host", format!("Wine {version}")),
        HostKind::Wine(None) => Item::ok("host", "Wine (it gives no version)"),
        HostKind::Other => Item::not_checked(
            "host",
            format!(
                "this is the native build for {}, which runs no title; {WINE_ONLY}",
                std::env::consts::OS
            ),
        ),
    }
}

/// The processor item, from what `run --execute` checks before it starts a
/// title (`run::check_cpu_features_with`), with what `ROSETTA_ADVERTISE_AVX`
/// holds: under Wine on a Mac that variable is what the answer depends on.
#[cfg(any(windows, test))]
pub(crate) fn cpu_item(
    has_avx2: bool,
    skip: Option<&str>,
    rosetta: Option<&str>,
    host: &HostKind,
) -> Item {
    let rosetta = || match rosetta {
        Some(value) => format!("ROSETTA_ADVERTISE_AVX is {value}"),
        None => "ROSETTA_ADVERTISE_AVX is not set".to_owned(),
    };
    let wine = matches!(host, HostKind::Wine(_));
    match crate::run::check_cpu_features_with(has_avx2, skip) {
        Ok(()) if has_avx2 && wine => Item::ok(
            "cpu",
            format!("AVX2 is there, as `run --execute` requires ({})", rosetta()),
        ),
        Ok(()) if has_avx2 => Item::ok("cpu", "AVX2 is there, as `run --execute` requires"),
        // The check is off, and what it guards against is still the case.
        Ok(()) => Item::problem(
            "cpu",
            format!(
                "no AVX2, and {}=1 turns the check of `run --execute` off: a title would stop at \
                 its first AVX2 instruction",
                crate::run::SKIP_CPU_CHECK_ENV
            ),
        ),
        Err(e) if wine => Item::problem(
            "cpu",
            format!(
                "{e}. Under Rosetta 2 a process has AVX2 only if it was started with \
                 ROSETTA_ADVERTISE_AVX=1 ({}); tools/macos/victory.sh sets it",
                rosetta()
            ),
        ),
        Err(e) => Item::problem("cpu", e.to_string()),
    }
}

/// One castable format the device was asked about: whether it makes a
/// small texture of format `own` castable to `cast`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CastAsked {
    pub(crate) own: u32,
    pub(crate) cast: u32,
    pub(crate) taken: bool,
}

/// What asking the device about castable formats gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Casts {
    /// It could not be asked (why).
    NotAsked(String),
    Asked(Vec<CastAsked>),
}

/// What the device said and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeviceFacts {
    pub(crate) adapter: Option<String>,
    /// The highest shader model, as D3D12 numbers them (0x66 is 6.6).
    pub(crate) shader_model: Option<u32>,
    /// The mesh shader tier, as D3D12 numbers them (0: none, 10: tier 1).
    pub(crate) mesh_tier: Option<u32>,
    /// Whether the device reports 64-bit atomics on typed heap resources.
    pub(crate) atomics_claimed: Option<bool>,
    pub(crate) casts: Casts,
    /// What `victory-gpu` logged while it made its device.
    pub(crate) logged: Vec<Logged>,
}

/// The D3D12 device of this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Device {
    /// The native build of a system without D3D12.
    #[cfg_attr(windows, allow(dead_code))]
    NotWindows,
    /// None could be made (why).
    None(String),
    Made(DeviceFacts),
}

/// What `victory-gpu`'s probe of 64-bit atomics found, read from its log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Atomics {
    /// The device does not report them; nothing was run.
    Unclaimed,
    /// Reported, and the probe's program ran.
    Works,
    /// Reported, and the probe's program left both of its writes undone:
    /// what it read back.
    NotRun { texel: String, stored: String },
    /// Reported, and the probe could not be run (why).
    Unchecked(String),
    /// The runtime's log has no verdict.
    Unknown,
}

/// The verdict in `logged`, the lines of the runtime's device start.
pub(crate) fn atomics_of(logged: &[Logged]) -> Atomics {
    let field = |line: &Logged, name: &str| line.field(name).unwrap_or("?").to_owned();
    let usable = logged
        .iter()
        .find(|l| l.message == RUNTIME_ATOMICS)
        .and_then(|l| l.field("atomics"));
    let not_run = logged
        .iter()
        .find(|l| l.message.starts_with(RUNTIME_ATOMICS_NOT_RUN));
    let unchecked = logged
        .iter()
        .find(|l| l.message == RUNTIME_ATOMICS_UNCHECKED);
    match (usable, not_run, unchecked) {
        (Some("false"), Some(line), _) => Atomics::NotRun {
            texel: field(line, "texel"),
            stored: field(line, "stored"),
        },
        (Some("false"), None, _) => Atomics::Unclaimed,
        (Some("true"), _, Some(line)) => Atomics::Unchecked(field(line, "error")),
        (Some("true"), _, None) => Atomics::Works,
        _ => Atomics::Unknown,
    }
}

const DROPPED: &str = "pipelines that use them are refused and their dispatches dropped";

fn atomics_item(claimed: Option<bool>, found: &Atomics) -> Item {
    let name = "64-bit atomics";
    match found {
        Atomics::Works => Item::ok(
            name,
            "the device reports them, and VIctory's probe ran one (measured: its atomic and the \
             store beside it both arrived)",
        ),
        Atomics::NotRun { texel, stored } => Item::warning(
            name,
            format!(
                "the device reports them, and VIctory's probe program with one wrote nothing \
                 (measured: texel {texel}, stored {stored}): {DROPPED}. Known under D3DMetal \
                 (docs/macos.md, \"Open differences under D3DMetal\"); nothing in the setup \
                 changes it"
            ),
        ),
        Atomics::Unclaimed if claimed == Some(true) => Item::not_checked(
            name,
            "the device reports them and VIctory's runtime logged that it does not: the two \
             disagree",
        ),
        Atomics::Unclaimed => {
            Item::warning(name, format!("the device does not report them: {DROPPED}"))
        }
        Atomics::Unchecked(why) => Item::warning(
            name,
            format!(
                "the device reports them; VIctory's probe could not run ({why}), so the device's \
                 word is taken"
            ),
        ),
        Atomics::Unknown => Item::not_checked(
            name,
            "VIctory's D3D12 runtime logged no verdict of its probe when it made its device",
        ),
    }
}

/// The DXGI name of a format the doctor asks about.
fn format_name(format: u32) -> String {
    let name = match format {
        24 => "R10G10B10A2_UNORM",
        26 => "R11G11B10_FLOAT",
        28 => "R8G8B8A8_UNORM",
        29 => "R8G8B8A8_UNORM_SRGB",
        35 => "R16G16_UNORM",
        41 => "R32_FLOAT",
        42 => "R32_UINT",
        49 => "R8G8_UNORM",
        56 => "R16_UNORM",
        57 => "R16_UINT",
        67 => "R9G9B9E5_SHAREDEXP",
        68 => "R8G8_B8G8_UNORM",
        69 => "G8R8_G8B8_UNORM",
        85 => "B5G6R5_UNORM",
        87 => "B8G8R8A8_UNORM",
        89 => "R10G10B10_XR_BIAS_A2_UNORM",
        other => return format!("format {other}"),
    };
    format!("{name} ({format})")
}

fn casts_item(casts: &Casts) -> Item {
    let name = "castable formats";
    let asked = match casts {
        Casts::NotAsked(why) => return Item::not_checked(name, why.clone()),
        Casts::Asked(asked) => asked,
    };
    // What D3DMetal was measured to refuse (docs/macos.md): the only
    // refusals the runtime is known to get by without.
    const KNOWN: [u32; 3] = [89, 68, 69];
    let unknown = asked.iter().any(|a| !a.taken && !KNOWN.contains(&a.cast));
    let refused: Vec<String> = asked
        .iter()
        .filter(|a| !a.taken)
        .map(|a| format_name(a.cast))
        .collect();
    let taken = asked.len() - refused.len();
    if refused.is_empty() {
        Item::ok(
            name,
            format!(
                "measured on 4 by 4 textures: the device takes each of the {taken} castable \
                 formats asked"
            ),
        )
    } else if taken == 0 {
        Item::warning(
            name,
            format!(
                "measured on 4 by 4 textures: the device refuses each of the {} castable formats \
                 asked, so no texture is castable: views in another format are refused or copied",
                refused.len()
            ),
        )
    } else if unknown {
        Item::warning(
            name,
            format!(
                "measured on 4 by 4 textures: the device refuses {} in a texture's castable \
                 formats and takes the other {taken} asked. Not all of these are refusals seen \
                 before (docs/macos.md names 89, 68 and 69): VIctory leaves the refused ones \
                 out, and what a title does with views in them is not known",
                refused.join(", ")
            ),
        )
    } else {
        Item::ok(
            name,
            format!(
                "measured on 4 by 4 textures: the device refuses {} in a texture's castable \
                 formats and takes the other {taken} asked; VIctory leaves the refused ones out \
                 (only a view in one of them over a texture of another format is refused)",
                refused.join(", ")
            ),
        )
    }
}

/// The six lines about the D3D12 device.
pub(crate) fn device_items(device: &Device, host: &HostKind) -> Vec<Item> {
    const NAMES: [&str; 6] = [
        "d3d12 device",
        "adapter",
        "shader model",
        "mesh shaders",
        "64-bit atomics",
        "castable formats",
    ];
    let facts = match device {
        Device::NotWindows => {
            return NAMES
                .iter()
                .map(|name| Item::not_checked(name, WINE_ONLY))
                .collect();
        }
        Device::None(why) => {
            let advice = if matches!(host, HostKind::Wine(_)) {
                " Start the doctor through tools/macos/victory.sh, which sets the DLL overrides, \
                 and check that Apple's D3DMetal is in the Wine build (./setup.sh)."
            } else {
                ""
            };
            let mut items = vec![Item::problem(
                NAMES[0],
                format!("none could be made ({why}): nothing would be drawn.{advice}"),
            )];
            items.extend(
                NAMES[1..]
                    .iter()
                    .map(|name| Item::not_checked(name, "there is no D3D12 device")),
            );
            return items;
        }
        Device::Made(facts) => facts,
    };
    let adapter = match &facts.adapter {
        Some(name) => Item::ok(NAMES[1], name.clone()),
        None => Item::not_checked(NAMES[1], "DXGI did not name the device's adapter"),
    };
    let model = match facts.shader_model {
        Some(m) if m >= 0x66 => Item::ok(
            NAMES[2],
            format!("{}.{} (6.6 is required)", m >> 4, m & 0xf),
        ),
        Some(m) => Item::problem(
            NAMES[2],
            format!(
                "{}.{}, and translated shaders need 6.6: their pipelines would be refused",
                m >> 4,
                m & 0xf
            ),
        ),
        None => Item::not_checked(NAMES[2], "the device did not answer the query"),
    };
    let mesh = match facts.mesh_tier {
        Some(0) => Item::problem(
            NAMES[3],
            "not supported: every draw of a title's mesh shaders would be dropped",
        ),
        Some(10) => Item::ok(NAMES[3], "tier 1"),
        Some(tier) => Item::ok(NAMES[3], format!("supported (D3D12 tier value {tier})")),
        None => Item::not_checked(NAMES[3], "the device did not answer the query"),
    };
    vec![
        Item::ok(
            NAMES[0],
            "made, by VIctory's D3D12 runtime and by the doctor",
        ),
        adapter,
        model,
        mesh,
        atomics_item(facts.atomics_claimed, &atomics_of(&facts.logged)),
        casts_item(&facts.casts),
    ]
}

pub(crate) fn config_file_item(loaded: &Result<victory_core::Config, String>) -> Item {
    let name = "config file";
    match loaded {
        Ok(cfg) => match &cfg.source {
            Some(path) => Item::ok(name, format!("found: {}", path.display())),
            None => Item::ok(
                name,
                "none (VICTORY_CONFIG is not set and this directory has no victory.toml): the \
                 defaults and the VICTORY_* variables apply",
            ),
        },
        Err(e) => Item::problem(name, format!("cannot be used: {e}")),
    }
}

/// What the configured game directory is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GameDir {
    NotSet,
    /// [`EXAMPLE_GAME_DIR`]: never anybody's folder.
    Example,
    /// A Windows path, in a build that cannot follow one.
    WindowsPath,
    Missing,
    /// There, and refused by the asset policy (why).
    Refused(String),
    Folder,
}

/// Whether `path` is the example's value, however it is written.
fn is_example(path: &Path) -> bool {
    let text = path.to_string_lossy().replace('/', "\\");
    text.trim_end_matches('\\')
        .eq_ignore_ascii_case(EXAMPLE_GAME_DIR)
}

/// Whether `path` starts as Windows paths do: a drive, or two backslashes.
fn is_windows_path(path: &Path) -> bool {
    let text = path.to_string_lossy();
    let bytes = text.as_bytes();
    text.starts_with(r"\\")
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

/// Classifies the configured game directory. The example's value is known
/// by its text before anything is asked of the file system: under Wine its
/// drive does not exist, and resolving it is what `victory config` fails
/// on.
pub(crate) fn game_dir_state(
    game: Option<&Path>,
    windows_host: bool,
    is_dir: &dyn Fn(&Path) -> bool,
    outside_repo: &dyn Fn(&Path) -> Result<(), String>,
) -> GameDir {
    let Some(game) = game else {
        return GameDir::NotSet;
    };
    if is_example(game) {
        GameDir::Example
    } else if !windows_host && is_windows_path(game) {
        GameDir::WindowsPath
    } else if !is_dir(game) {
        GameDir::Missing
    } else if let Err(why) = outside_repo(game) {
        GameDir::Refused(why)
    } else {
        GameDir::Folder
    }
}

/// A game directory is not needed for a run, so a wrong one is a warning;
/// one inside the repository breaks the asset policy and is a problem.
pub(crate) fn game_dir_item(game: Option<&Path>, state: &GameDir) -> Item {
    let name = "game_dir";
    let shown = game.map(|p| p.display().to_string()).unwrap_or_default();
    const NOT_NEEDED: &str = "`run` and `inspect` take the executable on the command line and do \
                              not need it";
    match state {
        GameDir::NotSet => Item::ok(name, format!("not set: {NOT_NEEDED}")),
        GameDir::Example => Item::warning(
            name,
            format!(
                "still the example value {shown}, which is nobody's folder: in victory.toml name \
                 the folder your game is in (on a Mac in the Z:\\ form: /Users/you/games/x is \
                 Z:\\Users\\you\\games\\x) or remove the line. {NOT_NEEDED}; `victory config` \
                 fails until it is corrected"
            ),
        ),
        GameDir::WindowsPath => {
            Item::not_checked(name, format!("{shown} is a Windows path; {WINE_ONLY}"))
        }
        GameDir::Missing => Item::warning(
            name,
            format!(
                "{shown} is not a directory: name the folder your game is in or remove the line. \
                 {NOT_NEEDED}; `victory config` fails until it is corrected"
            ),
        ),
        GameDir::Refused(why) => Item::problem(name, why.clone()),
        GameDir::Folder => Item::ok(name, format!("{shown} is a directory")),
    }
}

/// Whether a run can keep files in `dir`: the asset policy allows the
/// folder, it is there or can be made, and a file can be written in it and
/// removed. The folder is made if it is missing, as a run makes it.
pub(crate) fn usable(
    dir: &Path,
    outside_repo: &dyn Fn(&Path) -> Result<(), String>,
) -> Result<(), String> {
    outside_repo(dir)?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot be made: {e}"))?;
    let probe = dir.join(format!(".victory-doctor-{}", std::process::id()));
    std::fs::write(&probe, b"victory doctor").map_err(|e| format!("cannot be written to: {e}"))?;
    std::fs::remove_file(&probe).map_err(|e| format!("a file written there cannot be removed: {e}"))
}

pub(crate) fn folder_item(name: &'static str, dir: &Path, usable: &Result<(), String>) -> Item {
    match usable {
        Ok(()) => Item::ok(name, format!("{} can be written to", dir.display())),
        Err(why) if why.contains(&dir.display().to_string()) => Item::problem(name, why.clone()),
        Err(why) => Item::problem(name, format!("{} {why}", dir.display())),
    }
}

/// The lines about the configuration: the file, the game directory and
/// the two folders a run writes to.
fn config_items(host: &HostKind) -> Vec<Item> {
    let outside = |path: &Path| victory_core::ensure_outside_repo(path).map_err(|e| e.to_string());
    let loaded = victory_core::Config::load().map_err(|e| format!("{e:#}"));
    let mut items = vec![config_file_item(&loaded)];
    let cfg = match &loaded {
        Ok(cfg) => cfg,
        Err(_) => {
            items.extend(["game_dir", "cache_dir", "data_dir"].map(|name| {
                Item::not_checked(name, "the configuration cannot be used (see above)")
            }));
            return items;
        }
    };
    let game = cfg.game_dir.as_deref();
    let state = game_dir_state(game, host.windows(), &|p| p.is_dir(), &outside);
    items.push(game_dir_item(game, &state));
    for (name, dir) in [("cache_dir", &cfg.cache_dir), ("data_dir", &cfg.data_dir)] {
        items.push(if host.windows() {
            folder_item(name, dir, &usable(dir, &outside))
        } else {
            // The folders of a run are the ones the Windows build resolves
            // from what the launcher sets.
            Item::not_checked(name, WINE_ONLY)
        });
    }
    items
}

/// What was found of the DXIL validator (`dxil.dll`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ValidatorFound {
    /// The native build cannot load a Windows DLL.
    #[cfg_attr(windows, allow(dead_code))]
    NotWindows,
    /// `VICTORY_DXIL` is not set and no Windows SDK has one.
    NotNamed,
    /// Named, and no file is there.
    Absent(PathBuf),
    /// There, and it did not load (why).
    Unloadable(PathBuf, String),
    /// Loaded: what it said about each prebuilt shader.
    Loaded(PathBuf, Vec<(&'static str, Result<(), String>)>),
}

/// The validator is for `translate --validate` and the tests; a run does
/// not load it, so nothing about it is a problem.
pub(crate) fn validator_item(found: &ValidatorFound) -> Item {
    let name = "dxil validator";
    const USE: &str = "only `translate --validate` and the tests use it, a run does not";
    match found {
        ValidatorFound::NotWindows => Item::not_checked(name, WINE_ONLY),
        ValidatorFound::NotNamed => Item::not_checked(
            name,
            format!("none is named (VICTORY_DXIL, the Z:\\ path of a dxil.dll); {USE}"),
        ),
        ValidatorFound::Absent(path) => Item::warning(
            name,
            format!(
                "VICTORY_DXIL names {}, and no file is there; {USE}",
                path.display()
            ),
        ),
        ValidatorFound::Unloadable(path, why) => Item::warning(
            name,
            format!("{} does not load ({why}); {USE}", path.display()),
        ),
        ValidatorFound::Loaded(path, verdicts) => {
            let refused: Vec<String> = verdicts
                .iter()
                .filter_map(|(what, verdict)| {
                    verdict
                        .as_ref()
                        .err()
                        .map(|why| format!("{what}: {}", why.trim()))
                })
                .collect();
            let names: Vec<&str> = verdicts.iter().map(|(what, _)| *what).collect();
            if !refused.is_empty() {
                Item::warning(
                    name,
                    format!(
                        "{} loads and refuses prebuilt bytecode ({})",
                        path.display(),
                        refused.join("; ")
                    ),
                )
            } else if names.is_empty() {
                Item::ok(name, format!("{} loads", path.display()))
            } else {
                Item::ok(
                    name,
                    format!(
                        "{} loads and accepts the prebuilt DXIL of {}",
                        path.display(),
                        names.join(" and ")
                    ),
                )
            }
        }
    }
}

/// FNV-1a, the hash `cargo xtask embed-clamp` writes beside the bytecode
/// and `victory-gpu` compares (`indirect::fnv`).
pub(crate) fn fnv(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    })
}

/// One of the two shaders VIctory embeds as bytecode.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Prebuilt {
    /// The source file's name.
    pub(crate) source: &'static str,
    /// The hash written beside the bytecode: of the source it was built from.
    pub(crate) embedded_hash: u64,
    /// The hash of the present source.
    pub(crate) source_hash: u64,
    pub(crate) dxil: &'static [u8],
    pub(crate) dxbc: &'static [u8],
    /// What a title loses without the shader.
    pub(crate) lost: &'static str,
    /// The command that embeds new bytecode.
    pub(crate) embed: &'static str,
}

fn clamp() -> Prebuilt {
    Prebuilt {
        source: "clamp.hlsl",
        embedded_hash: clamp_bytecode::SOURCE_HASH,
        source_hash: fnv(CLAMP_SOURCE.as_bytes()),
        dxil: clamp_bytecode::DXIL,
        dxbc: clamp_bytecode::DXBC,
        lost: "every indirect draw and dispatch is dropped",
        embed: "cargo xtask embed-clamp",
    }
}

fn depth() -> Prebuilt {
    Prebuilt {
        source: "depth.hlsl",
        embedded_hash: depth_bytecode::SOURCE_HASH,
        source_hash: fnv(DEPTH_SOURCE.as_bytes()),
        dxil: depth_bytecode::DXIL,
        dxbc: depth_bytecode::DXBC,
        lost: "16-bit depth written through unordered views does not reach the depth texture",
        embed: "cargo xtask embed-depth",
    }
}

/// Whether `bytes` are one whole shader container: the `DXBC` mark, and
/// the length the header gives (a little-endian dword at byte 24).
fn whole_container(bytes: &[u8]) -> bool {
    bytes.starts_with(b"DXBC")
        && bytes.get(24..28).is_some_and(|len| {
            u32::from_le_bytes([len[0], len[1], len[2], len[3]]) as usize == bytes.len()
        })
}

/// The item of a prebuilt shader. `victory-gpu` uses the bytecode only
/// while its hash names the present source, and otherwise has the system
/// compile the shader, which Windows' compiler does and Wine's cannot
/// (`host_compiles`): on a Mac stale bytecode means no shader at all.
pub(crate) fn prebuilt_item(name: &'static str, shader: &Prebuilt, host_compiles: bool) -> Item {
    let fault = if shader.dxil.is_empty() && shader.dxbc.is_empty() {
        Some(format!("no bytecode of {} is embedded", shader.source))
    } else if shader.embedded_hash != shader.source_hash {
        Some(format!(
            "the embedded bytecode was built from another {} (its source hash is {:#018x}, the \
             present source's {:#018x}) and is not used",
            shader.source, shader.embedded_hash, shader.source_hash
        ))
    } else if [shader.dxil, shader.dxbc]
        .iter()
        .any(|bytes| !bytes.is_empty() && !whole_container(bytes))
    {
        Some("the embedded bytecode is not a whole shader container".to_owned())
    } else {
        None
    };
    match fault {
        None => Item::ok(
            name,
            format!(
                "embedded and built from the present {} (source hash {:#018x}; DXIL {} bytes, \
                 DXBC {} bytes)",
                shader.source,
                shader.source_hash,
                shader.dxil.len(),
                shader.dxbc.len()
            ),
        ),
        Some(fault) if host_compiles => Item::warning(
            name,
            format!("{fault}; Windows' own compiler builds the shader when a title starts"),
        ),
        Some(fault) => Item::problem(
            name,
            format!(
                "{fault}. Wine's compiler cannot build the shader, so {}. Embed bytecode from \
                 the bytecode workflow with `{}`",
                shader.lost, shader.embed
            ),
        ),
    }
}

#[cfg(windows)]
fn host_kind() -> HostKind {
    device::host()
}

#[cfg(not(windows))]
fn host_kind() -> HostKind {
    HostKind::Other
}

#[cfg(windows)]
fn cpu(host: &HostKind) -> Item {
    let var = |name: &str| std::env::var(name).ok();
    cpu_item(
        device::has_avx2(),
        var(crate::run::SKIP_CPU_CHECK_ENV).as_deref(),
        var("ROSETTA_ADVERTISE_AVX").as_deref(),
        host,
    )
}

#[cfg(not(windows))]
fn cpu(_: &HostKind) -> Item {
    Item::not_checked("cpu", WINE_ONLY)
}

#[cfg(windows)]
fn measured_device() -> Device {
    device::measure()
}

#[cfg(not(windows))]
fn measured_device() -> Device {
    Device::NotWindows
}

#[cfg(windows)]
fn validator() -> ValidatorFound {
    let Some(path) = victory_shader::emit::validator::Validator::default_path() else {
        return ValidatorFound::NotNamed;
    };
    if !path.is_file() {
        return ValidatorFound::Absent(path);
    }
    let containers: Vec<(&'static str, &[u8])> =
        [("clamp.hlsl", clamp().dxil), ("depth.hlsl", depth().dxil)]
            .into_iter()
            .filter(|(_, bytes)| !bytes.is_empty())
            .collect();
    match device::validate(&path, &containers) {
        Ok(verdicts) => ValidatorFound::Loaded(path, verdicts),
        Err(why) => ValidatorFound::Unloadable(path, why),
    }
}

#[cfg(not(windows))]
fn validator() -> ValidatorFound {
    ValidatorFound::NotWindows
}

/// Checks this machine and writes the report to `out`. Returns the exit
/// status: 1 if an item is a problem, else 0.
pub fn run(out: &mut dyn Write) -> Result<u32> {
    let host = host_kind();
    let mut items = vec![host_item(&host), cpu(&host)];
    items.extend(device_items(&measured_device(), &host));
    items.extend(config_items(&host));
    items.push(validator_item(&validator()));
    let host_compiles = host == HostKind::Windows;
    items.push(prebuilt_item("clamp bytecode", &clamp(), host_compiles));
    items.push(prebuilt_item("depth bytecode", &depth(), host_compiles));
    Ok(u32::from(write_report(&items, out)?))
}

#[cfg(test)]
mod tests;
