//! `victory`: the command-line front end.
//!
//! Every command reads the user's own files and writes only to stdout or to
//! the user's cache directory (outside the repository).

mod census;
mod inspect;
// The D3D12 runtime it drives is built for Windows only.
#[cfg(windows)]
mod prebuild;
mod run;
mod shader;
#[cfg(test)]
mod testing;
mod translate;

use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "victory",
    version,
    about = "Run an Xbox Series title natively on Windows"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Summarise one PE image: headers, sections, imports, TLS, build id.
    Inspect {
        file: PathBuf,
        /// List every imported function.
        #[arg(long)]
        imports: bool,
        /// List every exported function.
        #[arg(long)]
        exports: bool,
    },
    /// Aggregate the imports of every PE image under the given paths and
    /// show which ones our runtime covers. Defaults to the configured
    /// game and test-title directories.
    Census {
        paths: Vec<PathBuf>,
        /// Do not count host pass-through as covered.
        #[arg(long)]
        strict: bool,
        /// List every missing `dll!function`, most used first.
        #[arg(long)]
        missing: bool,
    },
    /// List the `dll!export` pairs an import library describes.
    LibExports {
        lib: PathBuf,
        /// List every export, not just per-DLL counts.
        #[arg(long)]
        list: bool,
    },
    /// Find shader containers in files or directories (also inside engine
    /// archives, if uncompressed) and report their layout. Shader packs
    /// among the paths are listed last, their stages named from the
    /// containers found.
    ShaderInfo {
        paths: Vec<PathBuf>,
        /// Print one line per container found (and per pack pipeline).
        #[arg(long)]
        verbose: bool,
        /// Also list the decoded native instructions (implies --verbose).
        #[arg(long)]
        disasm: bool,
        /// Instead list the effects of shader databases: technique, pass,
        /// shaders (name, container hash, engine hash) and pass state.
        #[arg(long)]
        effects: bool,
    },
    /// Run every shader found (containers, SGD5 files) through the
    /// translator and report what it cannot handle yet.
    Translate {
        paths: Vec<PathBuf>,
        /// Check emitted DXIL with dxil.dll's validator (`VICTORY_DXIL`,
        /// else the newest Windows SDK's).
        #[arg(long)]
        validate: bool,
        /// Also create a host D3D12 pipeline for each emitted shader. With
        /// `--cache`: then make every pipeline of the title's shader pack
        /// into the host pipeline library `run` reads (on this GPU and
        /// driver; a second time only what is not there yet).
        #[arg(long)]
        pipelines: bool,
        /// Print one line per program and outcome.
        #[arg(long)]
        list: bool,
        /// Worker threads (default: every logical core).
        #[arg(long)]
        jobs: Option<usize>,
        /// Only translate into the shader cache `run` reads (the warm-up):
        /// every container found, once, nothing else checked or listed
        /// (with `--pipelines`, then the shader pack's pipelines).
        #[arg(long, conflicts_with_all = ["validate", "list"])]
        cache: bool,
    },
    /// Prepare, map and bind a title executable; with --execute, run it.
    Run {
        exe: PathBuf,
        /// What unimplemented imports do when called.
        #[arg(long, value_enum, default_value_t = Unimplemented::Trap)]
        unimplemented: Unimplemented,
        /// Disable host pass-through (everything must be implemented by us).
        #[arg(long)]
        strict: bool,
        /// Actually start the title. Without it, stop after binding.
        #[arg(long)]
        execute: bool,
        /// The console model the title sees (device type, memory).
        #[arg(long, value_enum, default_value_t = Console::SeriesX)]
        console: Console,
        /// Arguments for the title, after `--` (it reads the whole command
        /// line; a title that relaunches itself passes some).
        #[arg(last = true)]
        title_args: Vec<String>,
    },
    /// Show the resolved configuration and check it: fails if a path
    /// lies inside the repository or the configured game directory is not
    /// there. No game directory configured is said, and is no failure.
    Config,
}

#[derive(Clone, Copy, ValueEnum)]
enum Unimplemented {
    Trap,
    Zero,
}

#[derive(Clone, Copy, ValueEnum)]
enum Console {
    SeriesX,
    SeriesS,
}

/// Diagnostics: the heap bytes Rust code holds, for the statistics.
#[cfg(all(windows, feature = "diagnostics"))]
#[global_allocator]
static ALLOC: victory_gpu::CountingAlloc = victory_gpu::CountingAlloc;

/// An error ends the process with status 1 (and clap, which refuses the
/// arguments, with 2). A title that ran to the end of its entry point ends
/// it with what that returned; everything else that succeeds, with 0.
///
/// Upstream ends a title's nonzero status with `std::process::exit`; here
/// it is [`end_with`], for the reason given there.
fn main() -> Result<()> {
    victory_core::logging::init("info");
    // The audio recorder `run` starts for `VICTORY_AUDIO_CAPTURE`.
    #[cfg(all(windows, feature = "diagnostics"))]
    if victory_hle::modules::audio::capture::child() {
        return Ok(());
    }
    // Not locked: a running title's threads log while `run` is in here.
    match dispatch(Cli::parse(), &mut std::io::stdout())? {
        0 => Ok(()),
        // All 32 bits of the Windows process status.
        code => end_with(code),
    }
}

/// Ends the process with the exit code of the title that ran in it.
///
/// Through the C runtime's `exit`, which is how a title ends the process
/// itself and what returning from `main` comes to: its exit functions
/// include the loader's detach of the title. `std::process::exit` goes
/// to `ExitProcess` on Windows, past them.
fn end_with(code: u32) -> ! {
    unsafe extern "C" {
        fn exit(code: i32) -> !;
    }
    // The runtime's own flush at the end of `main` is not reached.
    let _ = std::io::stdout().flush();
    // SAFETY: the C runtime's `exit`, with the same bits as the code.
    unsafe { exit(code.cast_signed()) }
}

/// Runs the command; its report goes to `out`. Returns the exit code of
/// the title `run --execute` ran to the end of its entry point, and 0 for
/// everything else that succeeds.
fn dispatch(cli: Cli, out: &mut dyn Write) -> Result<u32> {
    match cli.command {
        Command::Inspect {
            file,
            imports,
            exports,
        } => inspect::run(&file, imports, exports, out)?,
        Command::Census {
            paths,
            strict,
            missing,
        } => census::run(paths, strict, missing, out)?,
        Command::LibExports { lib, list } => inspect::lib_exports(&lib, list, out)?,
        Command::ShaderInfo {
            paths,
            verbose,
            disasm,
            effects,
        } => shader::run(paths, verbose || disasm, disasm, effects, out)?,
        Command::Translate {
            paths,
            jobs,
            pipelines,
            cache: true,
            ..
        } => translate::warm(paths, jobs, pipelines)?,
        Command::Translate {
            paths,
            validate,
            pipelines,
            list,
            jobs,
            cache: false,
        } => {
            let options = translate::Options {
                validate,
                pipelines,
                list,
                jobs,
                pairs: translate::Options::pairs_of(
                    &std::env::var("VICTORY_PAIRS").unwrap_or_default(),
                ),
                emit_dump: std::env::var_os("VICTORY_EMIT_DUMP").map(PathBuf::from),
            };
            translate::run(paths, &options, out)?
        }
        Command::Run {
            exe,
            unimplemented,
            strict,
            execute,
            console,
            title_args: _,
        } => return run::run(&exe, unimplemented, strict, execute, console, out),
        Command::Config => show_config(
            &victory_core::Config::load()?,
            &|path| victory_core::ensure_outside_repo(path).map_err(|e| e.to_string()),
            out,
        )?,
    }
    Ok(0)
}

/// What `config` says when no game directory is configured.
const GAME_DIR_NOT_SET: &str = "NOTE: game_dir is not set: no game is configured yet. `run` and \
     `inspect` take the executable on the command line and do not need it; to configure one, set \
     VICTORY_GAME_DIR or [paths].game_dir in victory.toml (see victory.example.toml)";

/// Prints `cfg` in full, then fails if it cannot be used as it is: a path
/// `outside_repo` refuses (the asset policy), or a game directory that is
/// configured and is not there. No game directory configured at all is
/// said, and is no failure.
fn show_config(
    cfg: &victory_core::Config,
    outside_repo: &dyn Fn(&std::path::Path) -> Result<(), String>,
    out: &mut dyn Write,
) -> Result<()> {
    let status = |p: &PathBuf| {
        if p.exists() {
            format!("{}", p.display())
        } else {
            format!("{}  (does not exist)", p.display())
        }
    };
    writeln!(
        out,
        "config file : {}",
        cfg.source
            .as_ref()
            .map_or("(none)".into(), |p| p.display().to_string())
    )?;
    writeln!(
        out,
        "game_dir    : {}",
        cfg.game_dir.as_ref().map_or("(not set)".into(), status)
    )?;
    writeln!(out, "cache_dir   : {}", cfg.cache_dir.display())?;
    writeln!(out, "data_dir    : {}", cfg.data_dir.display())?;
    writeln!(
        out,
        "fidelityfx  : {}",
        cfg.fidelityfx_dir
            .as_ref()
            .map_or("(not set)".into(), status)
    )?;
    for t in &cfg.test_titles {
        writeln!(out, "test_title  : {}", status(t))?;
    }
    let mut problems = Vec::new();
    let mut violations = 0usize;
    for p in cfg
        .game_dir
        .iter()
        .chain(&cfg.test_titles)
        .chain([&cfg.cache_dir, &cfg.data_dir])
    {
        if let Err(e) = outside_repo(p) {
            writeln!(out, "POLICY VIOLATION: {e}")?;
            violations += 1;
        }
    }
    if violations > 0 {
        problems.push(format!(
            "{violations} configured path(s) violate the asset policy (see docs/asset-policy.md)"
        ));
    }
    // No game configured yet is a state of its own, and no fault: a fresh
    // setup is in it, and `run`, `inspect` and every command that is given
    // its paths work without one. It is said in words a script can find.
    if cfg.game_dir.is_none() {
        writeln!(out, "{GAME_DIR_NOT_SET}")?;
    }
    // The cache and data directories are created on first use; the game
    // directory is the user's and nothing here can make it.
    if let Some(game) = cfg.game_dir.as_ref().filter(|p| !p.is_dir()) {
        writeln!(
            out,
            "MISSING: game_dir {} is not a directory",
            game.display()
        )?;
        problems.push(format!(
            "the configured game_dir {} is not a directory",
            game.display()
        ));
    }
    // Scripts read the exit status, not the text.
    anyhow::ensure!(problems.is_empty(), "{}", problems.join("; "));
    Ok(())
}

/// Paths given on the command line, or the configured game + test titles.
pub(crate) fn default_paths(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    if !paths.is_empty() {
        return Ok(paths);
    }
    let cfg = victory_core::Config::load()?;
    let all: Vec<PathBuf> = cfg.game_dir.into_iter().chain(cfg.test_titles).collect();
    anyhow::ensure!(
        !all.is_empty(),
        "no paths given and none configured (see victory.example.toml)"
    );
    Ok(all)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::CommandFactory;
    use victory_core::Config;

    use super::*;
    use crate::testing::{line, report, report_of};

    fn parse(args: &[&str]) -> Result<Command, clap::Error> {
        Cli::try_parse_from(args).map(|cli| cli.command)
    }

    /// What clap refuses `args` with.
    #[track_caller]
    fn refused(args: &[&str]) -> clap::error::ErrorKind {
        match parse(args) {
            Ok(_) => panic!("{args:?} parsed"),
            Err(e) => e.kind(),
        }
    }

    #[test]
    fn the_argument_tree_is_consistent() {
        Cli::command().debug_assert();
        // The same check for release builds, where clap's own is compiled
        // out: every command is there under the name the docs use.
        let command = Cli::command();
        let names: Vec<&str> = command.get_subcommands().map(|c| c.get_name()).collect();
        for name in [
            "inspect",
            "census",
            "lib-exports",
            "shader-info",
            "translate",
            "run",
            "config",
        ] {
            assert!(names.contains(&name), "{name} is missing from {names:?}");
        }
        for sub in command.get_subcommands() {
            assert!(sub.get_about().is_some(), "{} has no help", sub.get_name());
        }
    }

    #[test]
    fn every_subcommand_parses() {
        // inspect
        let Ok(Command::Inspect {
            file,
            imports: false,
            exports: false,
        }) = parse(&["victory", "inspect", "title.exe"])
        else {
            panic!("inspect")
        };
        assert_eq!(file, Path::new("title.exe"));
        assert!(matches!(
            parse(&["victory", "inspect", "title.exe", "--imports", "--exports"]),
            Ok(Command::Inspect {
                imports: true,
                exports: true,
                ..
            })
        ));

        // census
        let Ok(Command::Census {
            paths,
            strict: false,
            missing: false,
        }) = parse(&["victory", "census"])
        else {
            panic!("census")
        };
        assert!(paths.is_empty());
        let Ok(Command::Census {
            paths,
            strict: true,
            missing: true,
        }) = parse(&["victory", "census", "a", "--strict", "b", "--missing"])
        else {
            panic!("census with flags")
        };
        assert_eq!(paths, [Path::new("a"), Path::new("b")]);

        // lib-exports
        let Ok(Command::LibExports { lib, list: false }) =
            parse(&["victory", "lib-exports", "x.lib"])
        else {
            panic!("lib-exports")
        };
        assert_eq!(lib, Path::new("x.lib"));
        assert!(matches!(
            parse(&["victory", "lib-exports", "x.lib", "--list"]),
            Ok(Command::LibExports { list: true, .. })
        ));

        // shader-info
        assert!(matches!(
            parse(&["victory", "shader-info"]),
            Ok(Command::ShaderInfo {
                verbose: false,
                disasm: false,
                ..
            })
        ));
        let Ok(Command::ShaderInfo {
            paths,
            verbose: true,
            disasm: true,
            effects: false,
        }) = parse(&["victory", "shader-info", "dir", "--verbose", "--disasm"])
        else {
            panic!("shader-info with flags")
        };
        assert_eq!(paths, [Path::new("dir")]);
        assert!(matches!(
            parse(&["victory", "shader-info", "dir", "--effects"]),
            Ok(Command::ShaderInfo {
                verbose: false,
                disasm: false,
                effects: true,
                ..
            })
        ));

        // translate
        assert!(matches!(
            parse(&["victory", "translate"]),
            Ok(Command::Translate {
                validate: false,
                pipelines: false,
                list: false,
                jobs: None,
                cache: false,
                ..
            })
        ));
        let Ok(Command::Translate {
            paths,
            validate: true,
            pipelines: true,
            list: true,
            jobs: Some(3),
            cache: false,
        }) = parse(&[
            "victory",
            "translate",
            "dir",
            "--validate",
            "--pipelines",
            "--list",
            "--jobs",
            "3",
        ])
        else {
            panic!("translate with flags")
        };
        assert_eq!(paths, [Path::new("dir")]);
        // The warm-up, alone or with the pipeline pre-build.
        assert!(matches!(
            parse(&["victory", "translate", "dir", "--cache", "--pipelines"]),
            Ok(Command::Translate {
                validate: false,
                pipelines: true,
                list: false,
                cache: true,
                ..
            })
        ));

        // run: the defaults, then every flag.
        let Ok(Command::Run {
            exe,
            unimplemented: Unimplemented::Trap,
            strict: false,
            execute: false,
            console: Console::SeriesX,
            title_args,
        }) = parse(&["victory", "run", "title.exe"])
        else {
            panic!("run")
        };
        assert_eq!(exe, Path::new("title.exe"));
        assert!(title_args.is_empty());
        assert!(matches!(
            parse(&[
                "victory",
                "run",
                "title.exe",
                "--unimplemented",
                "zero",
                "--strict",
                "--execute",
                "--console",
                "series-s",
            ]),
            Ok(Command::Run {
                unimplemented: Unimplemented::Zero,
                strict: true,
                execute: true,
                console: Console::SeriesS,
                ..
            })
        ));
        assert!(matches!(
            parse(&["victory", "run", "x.exe", "--unimplemented", "trap"]),
            Ok(Command::Run {
                unimplemented: Unimplemented::Trap,
                ..
            })
        ));
        assert!(matches!(
            parse(&["victory", "run", "x.exe", "--console", "series-x"]),
            Ok(Command::Run {
                console: Console::SeriesX,
                ..
            })
        ));

        // config
        assert!(matches!(parse(&["victory", "config"]), Ok(Command::Config)));
    }

    #[test]
    fn run_keeps_title_arguments_after_the_separator() {
        let Ok(Command::Run {
            exe,
            execute,
            strict,
            title_args,
            ..
        }) = parse(&[
            "victory",
            "run",
            "x.exe",
            "--execute",
            "--",
            "a",
            "--strict",
            "b c",
        ])
        else {
            panic!("run with title arguments")
        };
        assert_eq!(exe, Path::new("x.exe"));
        assert!(execute);
        // After the separator nothing is ours, whatever it looks like.
        assert!(!strict);
        assert_eq!(title_args, ["a", "--strict", "b c"]);
        // Without the separator a second word is not taken for one.
        use clap::error::ErrorKind;
        assert_eq!(
            refused(&["victory", "run", "x.exe", "a"]),
            ErrorKind::UnknownArgument
        );
    }

    #[test]
    fn arguments_outside_the_tree_are_refused() {
        use clap::error::ErrorKind;
        // No command at all prints the help and fails.
        assert_eq!(
            refused(&["victory"]),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
        assert_eq!(
            refused(&["victory", "no-such-command"]),
            ErrorKind::InvalidSubcommand
        );
        for needs_a_file in ["inspect", "lib-exports", "run"] {
            assert_eq!(
                refused(&["victory", needs_a_file]),
                ErrorKind::MissingRequiredArgument,
                "{needs_a_file}"
            );
        }
        assert_eq!(
            refused(&["victory", "inspect", "a.exe", "b.exe"]),
            ErrorKind::UnknownArgument
        );
        assert_eq!(
            refused(&["victory", "run", "x.exe", "--unimplemented", "ignore"]),
            ErrorKind::InvalidValue
        );
        assert_eq!(
            refused(&["victory", "run", "x.exe", "--console", "one"]),
            ErrorKind::InvalidValue
        );
        assert_eq!(
            refused(&["victory", "translate", "--jobs", "many"]),
            ErrorKind::ValueValidation
        );
        assert_eq!(
            refused(&["victory", "config", "extra"]),
            ErrorKind::UnknownArgument
        );
        assert_eq!(
            refused(&["victory", "census", "--list"]),
            ErrorKind::UnknownArgument
        );
        // The warm-up checks and lists nothing.
        for with in ["--validate", "--list"] {
            assert_eq!(
                refused(&["victory", "translate", "--cache", with]),
                ErrorKind::ArgumentConflict,
                "{with}"
            );
        }
    }

    /// A command's report goes where `dispatch` is told, and its failure
    /// comes back as an error.
    #[test]
    fn dispatch_writes_the_report_to_its_writer() {
        let dir = tempfile::tempdir().unwrap();
        let image = crate::testing::write(dir.path(), "title.dll", &crate::testing::title_image());
        let cli = Cli::try_parse_from(["victory", "inspect", image.to_str().unwrap()]).unwrap();
        let text = report_of(|out| dispatch(cli, out).map(drop));
        assert_eq!(line(&text, "kind "), "kind           dll");

        let absent = dir.path().join("absent.dll");
        let cli = Cli::try_parse_from(["victory", "inspect", absent.to_str().unwrap()]).unwrap();
        let (text, result) = report(|out| dispatch(cli, out).map(drop));
        assert!(result.is_err());
        assert_eq!(text, "");
    }

    /// The policy check of these tests: inside is what lies below `repo`.
    fn outside(repo: &Path) -> impl Fn(&Path) -> Result<(), String> + '_ {
        move |path| {
            if path.starts_with(repo) {
                Err(format!("{} is inside the repository", path.display()))
            } else {
                Ok(())
            }
        }
    }

    /// A machine in a temporary folder: `repo`, and a game directory that
    /// exists outside it. The cache and data directories do not exist.
    fn configured(root: &Path) -> Config {
        std::fs::create_dir(root.join("repo")).unwrap();
        std::fs::create_dir(root.join("game")).unwrap();
        Config {
            game_dir: Some(root.join("game")),
            cache_dir: root.join("cache"),
            data_dir: root.join("data"),
            test_titles: vec![root.join("titles")],
            fidelityfx_dir: None,
            source: Some(root.join("victory.toml")),
        }
    }

    #[test]
    fn config_reports_every_path_and_passes_when_they_can_be_used() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = configured(tmp.path());
        let repo = tmp.path().join("repo");
        let text = report_of(|out| show_config(&cfg, &outside(&repo), out));
        let shown = |name: &str| tmp.path().join(name).display().to_string();
        assert_eq!(
            line(&text, "config file : "),
            format!("config file : {}", shown("victory.toml"))
        );
        assert_eq!(
            line(&text, "game_dir    : "),
            format!("game_dir    : {}", shown("game"))
        );
        // Created on first use: not being there yet is no fault.
        assert_eq!(
            line(&text, "cache_dir   : "),
            format!("cache_dir   : {}", shown("cache"))
        );
        assert_eq!(
            line(&text, "data_dir    : "),
            format!("data_dir    : {}", shown("data"))
        );
        assert_eq!(
            line(&text, "test_title  : "),
            format!("test_title  : {}  (does not exist)", shown("titles"))
        );
        assert!(!text.contains("POLICY VIOLATION"), "{text}");
        assert!(!text.contains("MISSING"), "{text}");
        assert!(!text.contains("game_dir is not set"), "{text}");

        // No game directory configured is a configuration too.
        let cfg = Config {
            game_dir: None,
            source: None,
            ..cfg
        };
        let text = report_of(|out| show_config(&cfg, &outside(&repo), out));
        assert_eq!(line(&text, "config file : "), "config file : (none)");
        assert_eq!(line(&text, "game_dir    : "), "game_dir    : (not set)");
        assert_eq!(line(&text, "fidelityfx  : "), "fidelityfx  : (not set)");
        // Said in so many words, and no fault: `report_of` took an `Ok`.
        assert!(
            line(&text, "NOTE: ").starts_with("NOTE: game_dir is not set"),
            "{text}"
        );
        assert!(!text.contains("MISSING"), "{text}");

        // The host's FidelityFX folder is shown like the game directory.
        let cfg = Config {
            fidelityfx_dir: Some(tmp.path().join("ffx")),
            ..cfg
        };
        let text = report_of(|out| show_config(&cfg, &outside(&repo), out));
        assert_eq!(
            line(&text, "fidelityfx  : "),
            format!("fidelityfx  : {}  (does not exist)", shown("ffx"))
        );
    }

    #[test]
    fn config_fails_when_the_game_dir_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let absent = tmp.path().join("absent");
        let cfg = Config {
            game_dir: Some(absent.clone()),
            ..configured(tmp.path())
        };
        let (text, result) = report(|out| show_config(&cfg, &outside(&repo), out));
        // The report is printed in full, then the command fails.
        assert_eq!(
            line(&text, "game_dir    : "),
            format!("game_dir    : {}  (does not exist)", absent.display())
        );
        line(&text, "data_dir    : ");
        assert_eq!(
            line(&text, "MISSING: "),
            format!("MISSING: game_dir {} is not a directory", absent.display())
        );
        assert_eq!(
            result.expect_err("a missing game_dir").to_string(),
            format!(
                "the configured game_dir {} is not a directory",
                absent.display()
            )
        );

        // A file where the folder should be is no game directory either.
        let file = crate::testing::write(tmp.path(), "a-file", b"not a folder");
        let cfg = Config {
            game_dir: Some(file.clone()),
            ..cfg
        };
        let (text, result) = report(|out| show_config(&cfg, &outside(&repo), out));
        assert_eq!(
            line(&text, "game_dir    : "),
            format!("game_dir    : {}", file.display())
        );
        assert_eq!(
            result.expect_err("a file as game_dir").to_string(),
            format!(
                "the configured game_dir {} is not a directory",
                file.display()
            )
        );
    }

    #[test]
    fn config_fails_when_a_path_is_inside_the_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let cfg = Config {
            cache_dir: repo.join("cache"),
            data_dir: repo.join("data"),
            ..configured(tmp.path())
        };
        let (text, result) = report(|out| show_config(&cfg, &outside(&repo), out));
        line(&text, "data_dir    : ");
        assert_eq!(text.matches("POLICY VIOLATION: ").count(), 2, "{text}");
        assert_eq!(
            result.expect_err("two paths inside").to_string(),
            "2 configured path(s) violate the asset policy (see docs/asset-policy.md)"
        );

        // Both kinds of fault are named when both are there.
        let cfg = Config {
            game_dir: Some(repo.join("game")),
            ..cfg
        };
        let (text, result) = report(|out| show_config(&cfg, &outside(&repo), out));
        assert_eq!(text.matches("POLICY VIOLATION: ").count(), 3, "{text}");
        assert_eq!(
            result.expect_err("three paths inside").to_string(),
            format!(
                "3 configured path(s) violate the asset policy (see docs/asset-policy.md); \
                 the configured game_dir {} is not a directory",
                repo.join("game").display()
            )
        );
    }

    /// `translate` is a command on every host.
    #[test]
    fn translate_runs_on_this_host() {
        let dir = tempfile::tempdir().unwrap();
        crate::testing::write(dir.path(), "vs.bin", &crate::testing::vertex_container());
        let folder = dir.path().to_str().unwrap();
        let cli = Cli::try_parse_from(["victory", "translate", "--jobs", "1", folder]).unwrap();
        let text = report_of(|out| dispatch(cli, out).map(drop));
        assert!(
            line(&text, "programs ").starts_with("programs     1 translated of 1 ("),
            "{text}"
        );
        assert!(
            line(&text, "DXIL ").starts_with("DXIL         1 emitted of 1 vertex and pixel"),
            "{text}"
        );
    }

    /// What needs Windows says so on another host, and nothing else does.
    #[cfg(not(windows))]
    #[test]
    fn translate_refuses_only_its_windows_checks_off_windows() {
        let dir = tempfile::tempdir().unwrap();
        crate::testing::write(dir.path(), "vs.bin", &crate::testing::vertex_container());
        let folder = dir.path().to_str().unwrap();
        for flag in ["--validate", "--pipelines"] {
            let cli = Cli::try_parse_from(["victory", "translate", flag, folder]).unwrap();
            let (text, result) = report(|out| dispatch(cli, out).map(drop));
            let error = result.expect_err(flag).to_string();
            assert!(
                error.starts_with(&format!("{flag} needs a Windows host")),
                "{error}"
            );
            assert_eq!(text, "");
        }
        // The warm-up fills the caches of the Windows runtime.
        let cli = Cli::try_parse_from(["victory", "translate", "--cache", folder]).unwrap();
        let (text, result) = report(|out| dispatch(cli, out).map(drop));
        let error = result.expect_err("--cache").to_string();
        assert!(error.starts_with("--cache needs a Windows host"), "{error}");
        assert_eq!(text, "");
    }
}
