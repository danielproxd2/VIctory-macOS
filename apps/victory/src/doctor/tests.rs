//! The doctor's decisions, on plain values: these run on every host.

use std::path::Path;

use clap::Parser;
use tracing::Level;

use super::*;

/// The one line of the report about `name`.
#[track_caller]
fn item<'a>(items: &'a [Item], name: &str) -> &'a Item {
    let mut found = items.iter().filter(|i| i.name == name);
    match (found.next(), found.next()) {
        (Some(item), None) => item,
        (None, _) => panic!("no item {name:?} in {items:?}"),
        _ => panic!("several items {name:?} in {items:?}"),
    }
}

fn logged(level: Level, message: &str, fields: &[(&str, &str)]) -> Logged {
    Logged {
        level,
        message: message.to_owned(),
        fields: fields
            .iter()
            .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
            .collect(),
    }
}

/// What `victory-gpu` logs for a device whose atomics probe wrote nothing.
fn d3dmetal_log() -> Vec<Logged> {
    vec![
        logged(Level::INFO, "host D3D12 device ready", &[]),
        logged(
            Level::WARN,
            "the host GPU reports 64-bit atomics on typed heap resources and did not run a \
             compute program that has one: pipelines with them are refused and their dispatches \
             dropped",
            &[
                ("host", "AMD Compatibility Mode"),
                ("texel", "0x0"),
                ("stored", "0x0"),
            ],
        ),
        logged(Level::INFO, RUNTIME_ATOMICS, &[("atomics", "false")]),
    ]
}

fn facts() -> DeviceFacts {
    DeviceFacts {
        adapter: Some("Some Adapter".to_owned()),
        shader_model: Some(0x66),
        mesh_tier: Some(10),
        atomics_claimed: Some(true),
        casts: Casts::Asked(vec![
            CastAsked {
                own: 28,
                cast: 29,
                taken: true,
            },
            CastAsked {
                own: 28,
                cast: 87,
                taken: true,
            },
        ]),
        logged: vec![logged(Level::INFO, RUNTIME_ATOMICS, &[("atomics", "true")])],
    }
}

fn wine() -> HostKind {
    HostKind::Wine(Some("11.0".to_owned()))
}

#[test]
fn doctor_parses_and_takes_no_arguments() {
    let cli = crate::Cli::try_parse_from(["victory", "doctor"]).unwrap();
    assert!(matches!(cli.command, crate::Command::Doctor));
    let Err(refused) = crate::Cli::try_parse_from(["victory", "doctor", "game.exe"]) else {
        panic!("doctor took a path");
    };
    assert_eq!(refused.kind(), clap::error::ErrorKind::UnknownArgument);
    let Err(refused) = crate::Cli::try_parse_from(["victory", "doctor", "--fix"]) else {
        panic!("doctor took an option");
    };
    assert_eq!(refused.kind(), clap::error::ErrorKind::UnknownArgument);
    // It is in the help, with a description.
    use clap::CommandFactory;
    let command = crate::Cli::command();
    let doctor = command
        .get_subcommands()
        .find(|c| c.get_name() == "doctor")
        .expect("doctor is a command");
    assert!(doctor.get_about().is_some());
}

#[test]
fn each_line_starts_with_its_mark_in_a_column() {
    let lines: Vec<String> = [
        Item::ok("host", "Windows"),
        Item::warning("game_dir", "x"),
        Item::problem("cpu", "no AVX2"),
        Item::not_checked("adapter", "why"),
    ]
    .iter()
    .map(Item::line)
    .collect();
    assert_eq!(
        lines,
        [
            "ok          host: Windows",
            "warning     game_dir: x",
            "PROBLEM     cpu: no AVX2",
            "not checked adapter: why",
        ]
    );
}

#[test]
fn only_a_problem_fails_the_report() {
    let passing = [
        Item::ok("host", "Windows"),
        Item::warning("game_dir", "x"),
        Item::not_checked("adapter", "why"),
    ];
    let mut out = Vec::new();
    assert!(!write_report(&passing, &mut out).unwrap());
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("\ndoctor: 1 ok, 1 warning, 0 PROBLEM, 1 not checked\n"),
        "{text}"
    );
    assert!(
        text.contains("No problem was found in what was checked"),
        "{text}"
    );

    let mut failing = passing.to_vec();
    failing.push(Item::problem("cpu", "no AVX2"));
    let mut out = Vec::new();
    assert!(write_report(&failing, &mut out).unwrap());
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("\ndoctor: 1 ok, 1 warning, 1 PROBLEM, 1 not checked\n"),
        "{text}"
    );
    assert!(
        text.contains("This machine is not ready for a run"),
        "{text}"
    );

    // With nothing left out the report says that too.
    let mut out = Vec::new();
    assert!(!write_report(&passing[..1], &mut out).unwrap());
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("every item was checked"), "{text}");
}

#[test]
fn the_host_is_named_with_wines_version() {
    assert_eq!(
        host_item(&HostKind::Windows).line(),
        "ok          host: Windows"
    );
    assert_eq!(host_item(&wine()).line(), "ok          host: Wine 11.0");
    assert_eq!(host_item(&HostKind::Wine(None)).mark, Mark::Ok);
    let native = host_item(&HostKind::Other);
    assert_eq!(native.mark, Mark::NotChecked);
    assert!(native.text.ends_with(WINE_ONLY), "{}", native.text);
}

#[test]
fn the_cpu_item_follows_the_check_of_run() {
    let ok = cpu_item(true, None, Some("1"), &wine());
    assert_eq!(ok.mark, Mark::Ok);
    assert!(
        ok.text.contains("ROSETTA_ADVERTISE_AVX is 1"),
        "{}",
        ok.text
    );
    // Windows itself has no Rosetta to speak of.
    let ok = cpu_item(true, None, None, &HostKind::Windows);
    assert_eq!(ok.mark, Mark::Ok);
    assert!(!ok.text.contains("ROSETTA"), "{}", ok.text);

    // The refusal is the one `run --execute` ends with, and under Wine the
    // variable Rosetta reads is named with what it holds.
    let refused = cpu_item(false, None, None, &wine());
    assert_eq!(refused.mark, Mark::Problem);
    assert!(
        refused
            .text
            .starts_with("this CPU does not support AVX2, which Xbox Series titles require;"),
        "{}",
        refused.text
    );
    assert!(
        refused.text.contains("ROSETTA_ADVERTISE_AVX is not set"),
        "{}",
        refused.text
    );
    let refused = cpu_item(false, Some("0"), None, &HostKind::Windows);
    assert_eq!(refused.mark, Mark::Problem);
    assert!(!refused.text.contains("ROSETTA"), "{}", refused.text);

    // The check turned off does not give the processor AVX2.
    let off = cpu_item(false, Some("1"), Some("1"), &wine());
    assert_eq!(off.mark, Mark::Problem);
    assert!(
        off.text
            .contains("VICTORY_SKIP_CPU_CHECK=1 turns the check"),
        "{}",
        off.text
    );
    assert_eq!(cpu_item(true, Some("1"), None, &wine()).mark, Mark::Ok);
}

#[test]
fn the_native_build_checks_no_device() {
    let items = device_items(&Device::NotWindows, &HostKind::Other);
    assert_eq!(items.len(), 6);
    for item in &items {
        assert_eq!(item.mark, Mark::NotChecked, "{item:?}");
        assert_eq!(item.text, WINE_ONLY, "{item:?}");
    }
    assert_eq!(
        item(&items, "d3d12 device").line(),
        "not checked d3d12 device: run under Wine via tools/macos/victory.sh doctor"
    );
}

#[test]
fn no_device_is_a_problem_and_leaves_the_rest_unchecked() {
    let none = Device::None("Unspecified error".to_owned());
    let items = device_items(&none, &wine());
    let device = item(&items, "d3d12 device");
    assert_eq!(device.mark, Mark::Problem);
    assert!(device.text.contains("Unspecified error"), "{}", device.text);
    assert!(device.text.contains("D3DMetal"), "{}", device.text);
    for name in [
        "adapter",
        "shader model",
        "mesh shaders",
        "64-bit atomics",
        "castable formats",
    ] {
        assert_eq!(item(&items, name).mark, Mark::NotChecked, "{name}");
    }
    // Windows itself has no launcher and no D3DMetal to point at.
    let items = device_items(&none, &HostKind::Windows);
    let device = item(&items, "d3d12 device");
    assert_eq!(device.mark, Mark::Problem);
    assert!(!device.text.contains("D3DMetal"), "{}", device.text);
}

#[test]
fn a_device_is_reported_with_what_it_said() {
    let items = device_items(&Device::Made(facts()), &wine());
    assert_eq!(item(&items, "d3d12 device").mark, Mark::Ok);
    assert_eq!(
        item(&items, "adapter").line(),
        "ok          adapter: Some Adapter"
    );
    assert_eq!(
        item(&items, "shader model").line(),
        "ok          shader model: 6.6 (6.6 is required)"
    );
    assert_eq!(
        item(&items, "mesh shaders").line(),
        "ok          mesh shaders: tier 1"
    );
    assert_eq!(item(&items, "64-bit atomics").mark, Mark::Ok);
    assert_eq!(item(&items, "castable formats").mark, Mark::Ok);

    // A newer model is as good; an older one and no mesh shaders are what
    // the runtime refuses pipelines for.
    let newer = DeviceFacts {
        shader_model: Some(0x68),
        ..facts()
    };
    let items = device_items(&Device::Made(newer), &wine());
    assert_eq!(
        item(&items, "shader model").line(),
        "ok          shader model: 6.8 (6.6 is required)"
    );
    let older = DeviceFacts {
        shader_model: Some(0x65),
        mesh_tier: Some(0),
        ..facts()
    };
    let items = device_items(&Device::Made(older), &wine());
    let model = item(&items, "shader model");
    assert_eq!(model.mark, Mark::Problem);
    assert!(model.text.starts_with("6.5, "), "{}", model.text);
    assert_eq!(item(&items, "mesh shaders").mark, Mark::Problem);

    // What the device did not answer is not guessed.
    let silent = DeviceFacts {
        adapter: None,
        shader_model: None,
        mesh_tier: None,
        ..facts()
    };
    let items = device_items(&Device::Made(silent), &wine());
    for name in ["adapter", "shader model", "mesh shaders"] {
        assert_eq!(item(&items, name).mark, Mark::NotChecked, "{name}");
    }
}

#[test]
fn the_atomics_verdict_is_the_runtimes_own() {
    assert_eq!(atomics_of(&facts().logged), Atomics::Works);
    assert_eq!(
        atomics_of(&d3dmetal_log()),
        Atomics::NotRun {
            texel: "0x0".to_owned(),
            stored: "0x0".to_owned()
        }
    );
    assert_eq!(
        atomics_of(&[logged(
            Level::INFO,
            RUNTIME_ATOMICS,
            &[("atomics", "false")]
        )]),
        Atomics::Unclaimed
    );
    assert_eq!(
        atomics_of(&[
            logged(
                Level::WARN,
                RUNTIME_ATOMICS_UNCHECKED,
                &[("host", "X"), ("error", "no pipeline")]
            ),
            logged(Level::INFO, RUNTIME_ATOMICS, &[("atomics", "true")]),
        ]),
        Atomics::Unchecked("no pipeline".to_owned())
    );
    // Lines that say nothing of the probe give no verdict.
    assert_eq!(atomics_of(&[]), Atomics::Unknown);
    assert_eq!(
        atomics_of(&[logged(Level::INFO, "host D3D12 device ready", &[])]),
        Atomics::Unknown
    );
}

#[test]
fn atomics_that_do_not_run_are_a_warning_with_what_was_measured() {
    let d3dmetal = DeviceFacts {
        logged: d3dmetal_log(),
        ..facts()
    };
    let items = device_items(&Device::Made(d3dmetal), &wine());
    let atomics = item(&items, "64-bit atomics");
    assert_eq!(atomics.mark, Mark::Warning);
    assert!(
        atomics.text.contains("(measured: texel 0x0, stored 0x0)"),
        "{}",
        atomics.text
    );
    assert!(
        atomics.text.contains("dispatches dropped"),
        "{}",
        atomics.text
    );

    let unclaimed = DeviceFacts {
        atomics_claimed: Some(false),
        logged: vec![logged(
            Level::INFO,
            RUNTIME_ATOMICS,
            &[("atomics", "false")],
        )],
        ..facts()
    };
    let items = device_items(&Device::Made(unclaimed), &wine());
    let atomics = item(&items, "64-bit atomics");
    assert_eq!(atomics.mark, Mark::Warning);
    assert!(atomics.text.starts_with("the device does not report them"));

    // No verdict in the log, or one that contradicts the device: said so.
    for logged in [
        Vec::new(),
        vec![logged(
            Level::INFO,
            RUNTIME_ATOMICS,
            &[("atomics", "false")],
        )],
    ] {
        let odd = DeviceFacts { logged, ..facts() };
        let items = device_items(&Device::Made(odd), &wine());
        assert_eq!(item(&items, "64-bit atomics").mark, Mark::NotChecked);
    }
}

/// The doctor reads the runtime's verdict from its log lines: the lines it
/// looks for are the ones `victory-gpu` writes.
#[test]
fn the_runtime_still_logs_the_lines_the_doctor_reads() {
    let host = include_str!("../../../../crates/victory-gpu/src/host.rs");
    for line in [
        RUNTIME_NO_DEVICE,
        RUNTIME_ATOMICS,
        RUNTIME_ATOMICS_NOT_RUN,
        RUNTIME_ATOMICS_UNCHECKED,
    ] {
        assert!(
            host.contains(&format!("\"{line}")),
            "host.rs no longer logs {line:?}"
        );
    }
    for field in [
        "texel = ",
        "stored = ",
        "error = %why",
        "tracing::info!(atomics, ",
    ] {
        assert!(
            host.contains(field),
            "host.rs no longer logs the field {field:?}"
        );
    }
}

#[test]
fn refused_castable_formats_are_named() {
    let asked = |refused: &[u32]| {
        Casts::Asked(
            [(28, 29), (28, 87), (28, 89), (56, 49), (56, 68), (56, 69)]
                .into_iter()
                .map(|(own, cast)| CastAsked {
                    own,
                    cast,
                    taken: !refused.contains(&cast),
                })
                .collect(),
        )
    };
    let all = casts_item(&asked(&[]));
    assert_eq!(all.mark, Mark::Ok);
    assert!(all.text.contains("takes each of the 6"), "{}", all.text);

    // What D3DMetal was measured to refuse: left out by VIctory, so ok.
    let some = casts_item(&asked(&[89, 68, 69]));
    assert_eq!(some.mark, Mark::Ok);
    assert!(
        some.text.contains(
            "refuses R10G10B10_XR_BIAS_A2_UNORM (89), R8G8_B8G8_UNORM (68), G8R8_G8B8_UNORM (69)"
        ),
        "{}",
        some.text
    );
    assert!(
        some.text.contains("takes the other 3 asked"),
        "{}",
        some.text
    );

    let none = casts_item(&asked(&[29, 87, 89, 49, 68, 69]));
    assert_eq!(none.mark, Mark::Warning);

    let not_asked = casts_item(&Casts::NotAsked("the device has no ID3D12Device10".into()));
    assert_eq!(not_asked.mark, Mark::NotChecked);
    assert_eq!(not_asked.text, "the device has no ID3D12Device10");
    // A format without a name here is still told apart.
    assert_eq!(format_name(2), "format 2");
}

fn config(game: Option<&str>, source: Option<&str>) -> victory_core::Config {
    victory_core::Config {
        game_dir: game.map(Into::into),
        cache_dir: "cache".into(),
        data_dir: "data".into(),
        test_titles: Vec::new(),
        fidelityfx_dir: None,
        source: source.map(Into::into),
    }
}

#[test]
fn the_config_file_is_found_or_not_and_a_broken_one_is_a_problem() {
    let found = config_file_item(&Ok(config(None, Some("victory.toml"))));
    assert_eq!(found.line(), "ok          config file: found: victory.toml");
    let none = config_file_item(&Ok(config(None, None)));
    assert_eq!(none.mark, Mark::Ok);
    assert!(none.text.starts_with("none ("), "{}", none.text);
    let broken = config_file_item(&Err("parsing victory.toml: unknown field `gmae_dir`".into()));
    assert_eq!(broken.mark, Mark::Problem);
    assert!(broken.text.contains("unknown field"), "{}", broken.text);
}

/// The file system of these tests: `there` is a directory, and what lies
/// below `repo` is inside the repository.
fn state(game: Option<&str>, windows_host: bool, there: &[&str]) -> GameDir {
    game_dir_state(
        game.map(Path::new),
        windows_host,
        &|p| there.iter().any(|t| Path::new(t) == p),
        &|p| {
            if p.starts_with("repo") {
                Err(format!("{} is inside the repository", p.display()))
            } else {
                Ok(())
            }
        },
    )
}

#[test]
fn the_example_game_dir_is_known_by_its_text() {
    // Nothing is asked of the file system for it: the two closures panic.
    for written in [
        r"D:\victory-data\game",
        r"d:\Victory-Data\GAME",
        r"D:\victory-data\game\",
        "D:/victory-data/game",
    ] {
        for windows_host in [true, false] {
            let found = game_dir_state(
                Some(Path::new(written)),
                windows_host,
                &|_| panic!("the example is not looked for"),
                &|_| panic!("the example is not resolved"),
            );
            assert_eq!(found, GameDir::Example, "{written}");
        }
    }
    let item = game_dir_item(Some(Path::new(EXAMPLE_GAME_DIR)), &GameDir::Example);
    assert_eq!(item.mark, Mark::Warning);
    assert!(
        item.text
            .starts_with(r"still the example value D:\victory-data\game, "),
        "{}",
        item.text
    );
    // Another folder on that drive is somebody's choice.
    assert_eq!(
        state(Some(r"D:\victory-data\games"), true, &[]),
        GameDir::Missing
    );
    // The value is the one the example file still shows, commented out.
    let example = include_str!("../../../../victory.example.toml");
    assert!(
        example.contains(&format!("# game_dir = '{EXAMPLE_GAME_DIR}'")),
        "victory.example.toml no longer shows {EXAMPLE_GAME_DIR}"
    );
}

#[test]
fn the_game_dir_is_classified_without_being_required() {
    assert_eq!(state(None, true, &[]), GameDir::NotSet);
    assert_eq!(
        state(Some(r"Z:\games\x"), true, &[r"Z:\games\x"]),
        GameDir::Folder
    );
    assert_eq!(state(Some(r"Z:\games\x"), true, &[]), GameDir::Missing);
    assert_eq!(
        state(Some("repo/game"), true, &["repo/game"]),
        GameDir::Refused("repo/game is inside the repository".to_owned())
    );
    // The native build cannot follow a Windows path, and says so instead
    // of calling the folder missing.
    assert_eq!(state(Some(r"Z:\games\x"), false, &[]), GameDir::WindowsPath);
    assert_eq!(state(Some(r"\\server\x"), false, &[]), GameDir::WindowsPath);
    assert_eq!(
        state(Some("/games/x"), false, &["/games/x"]),
        GameDir::Folder
    );
    assert_eq!(state(Some("/games/x"), false, &[]), GameDir::Missing);

    let game = Path::new(r"Z:\games\x");
    for (state, mark) in [
        (GameDir::NotSet, Mark::Ok),
        (GameDir::Folder, Mark::Ok),
        (GameDir::Missing, Mark::Warning),
        (GameDir::Example, Mark::Warning),
        (GameDir::WindowsPath, Mark::NotChecked),
        (GameDir::Refused("inside".into()), Mark::Problem),
    ] {
        assert_eq!(game_dir_item(Some(game), &state).mark, mark, "{state:?}");
    }
    let missing = game_dir_item(Some(game), &GameDir::Missing);
    assert!(
        missing.text.starts_with(r"Z:\games\x is not a directory"),
        "{}",
        missing.text
    );
    let foreign = game_dir_item(Some(game), &GameDir::WindowsPath);
    assert!(foreign.text.ends_with(WINE_ONLY), "{}", foreign.text);
}

#[test]
fn a_folder_is_usable_when_a_file_can_be_written_and_removed() {
    let tmp = tempfile::tempdir().unwrap();
    let allowed = |_: &Path| Ok(());
    // Made if it is missing, as a run makes it, and left empty.
    let dir = tmp.path().join("data").join("deeper");
    assert_eq!(usable(&dir, &allowed), Ok(()));
    assert!(dir.is_dir());
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    assert_eq!(folder_item("data_dir", &dir, &Ok(())).mark, Mark::Ok);

    // A file where the folder should be.
    let file = crate::testing::write(tmp.path(), "a-file", b"not a folder");
    let refused = usable(&file, &allowed);
    let why = refused.clone().expect_err("a file is no folder");
    assert!(why.starts_with("cannot be made: "), "{why}");
    let item = folder_item("data_dir", &file, &refused);
    assert_eq!(item.mark, Mark::Problem);
    assert!(
        item.text
            .starts_with(&format!("{} cannot be made: ", file.display())),
        "{}",
        item.text
    );

    // The asset policy comes first: nothing is made where it refuses.
    let inside = tmp.path().join("inside");
    let policy = |p: &Path| Err(format!("refusing to use {}", p.display()));
    let refused = usable(&inside, &policy);
    assert!(!inside.exists());
    let item = folder_item("cache_dir", &inside, &refused);
    assert_eq!(item.mark, Mark::Problem);
    assert_eq!(item.text, format!("refusing to use {}", inside.display()));
}

#[test]
fn the_validator_is_never_a_problem() {
    let path = PathBuf::from(r"Z:\dxc\dxil.dll");
    let cases = [
        (ValidatorFound::NotWindows, Mark::NotChecked, WINE_ONLY),
        (ValidatorFound::NotNamed, Mark::NotChecked, "none is named"),
        (
            ValidatorFound::Absent(path.clone()),
            Mark::Warning,
            "no file is there",
        ),
        (
            ValidatorFound::Unloadable(path.clone(), "cannot load it".into()),
            Mark::Warning,
            "does not load (cannot load it)",
        ),
        (
            ValidatorFound::Loaded(
                path.clone(),
                vec![("clamp.hlsl", Ok(())), ("depth.hlsl", Ok(()))],
            ),
            Mark::Ok,
            "loads and accepts the prebuilt DXIL of clamp.hlsl and depth.hlsl",
        ),
        (
            ValidatorFound::Loaded(
                path.clone(),
                vec![
                    ("clamp.hlsl", Ok(())),
                    ("depth.hlsl", Err("bad part\n".into())),
                ],
            ),
            Mark::Warning,
            "refuses prebuilt bytecode (depth.hlsl: bad part)",
        ),
        (ValidatorFound::Loaded(path, Vec::new()), Mark::Ok, "loads"),
    ];
    for (found, mark, said) in cases {
        let item = validator_item(&found);
        assert_eq!(item.mark, mark, "{found:?}");
        assert!(item.text.contains(said), "{found:?}: {}", item.text);
    }
}

/// The same vectors as `victory-gpu`'s `fnv_matches_embed_clamp` and
/// xtask's `fnv_matches_the_gpu_crates_source_hash`.
#[test]
fn fnv_is_the_hash_the_bytecode_is_named_by() {
    assert_eq!(fnv(b""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(fnv(b"a"), 0xaf63_dc4c_8601_ec8c);
    assert_eq!(fnv(b"foobar"), 0x8594_4171_f739_67e8);
}

/// A container of `len` bytes that says so in its header.
fn container(len: u32) -> &'static [u8] {
    let mut bytes = vec![0u8; len as usize];
    bytes[..4].copy_from_slice(b"DXBC");
    bytes[24..28].copy_from_slice(&len.to_le_bytes());
    bytes.leak()
}

fn shader(embedded_hash: u64, dxil: &'static [u8], dxbc: &'static [u8]) -> Prebuilt {
    Prebuilt {
        source: "clamp.hlsl",
        embedded_hash,
        source_hash: 0x1234,
        dxil,
        dxbc,
        lost: "every indirect draw and dispatch is dropped",
        embed: "cargo xtask embed-clamp",
    }
}

#[test]
fn prebuilt_bytecode_must_be_of_the_present_source() {
    let name = "clamp bytecode";
    let current = shader(0x1234, container(64), container(32));
    for host_compiles in [true, false] {
        let item = prebuilt_item(name, &current, host_compiles);
        assert_eq!(item.mark, Mark::Ok);
        assert!(
            item.text.contains("DXIL 64 bytes, DXBC 32 bytes"),
            "{}",
            item.text
        );
    }
    // One of the two forms is enough.
    assert_eq!(
        prebuilt_item(name, &shader(0x1234, container(64), &[]), false).mark,
        Mark::Ok
    );

    let faults = [
        (
            shader(0x9999, container(64), container(32)),
            "was built from another clamp.hlsl",
        ),
        (shader(0, &[], &[]), "no bytecode of clamp.hlsl is embedded"),
        (
            shader(0x1234, b"not a container at all, whatever it is", &[]),
            "not a whole shader container",
        ),
        // A container cut short: its header names more bytes than it has.
        (
            shader(0x1234, &container(64)[..48], &[]),
            "not a whole shader container",
        ),
    ];
    for (shader, said) in faults {
        // Wine cannot compile the shader: without bytecode there is none.
        let on_a_mac = prebuilt_item(name, &shader, false);
        assert_eq!(on_a_mac.mark, Mark::Problem, "{said}");
        assert!(on_a_mac.text.contains(said), "{}", on_a_mac.text);
        assert!(
            on_a_mac
                .text
                .contains("every indirect draw and dispatch is dropped")
                && on_a_mac.text.contains("`cargo xtask embed-clamp`"),
            "{}",
            on_a_mac.text
        );
        // Windows' compiler builds it at run time.
        let on_windows = prebuilt_item(name, &shader, true);
        assert_eq!(on_windows.mark, Mark::Warning, "{said}");
        assert!(on_windows.text.contains(said), "{}", on_windows.text);
    }
}

/// The bytecode in the tree is current: what the doctor would say on a Mac
/// about the two shaders as they are embedded now.
#[test]
fn the_embedded_bytecode_is_of_the_present_sources() {
    for (name, shader) in [("clamp bytecode", clamp()), ("depth bytecode", depth())] {
        let item = prebuilt_item(name, &shader, false);
        assert_eq!(item.mark, Mark::Ok, "{}", item.line());
        assert!(!shader.dxil.is_empty(), "{name} has no DXIL");
    }
}

/// The whole command in the native build: it ends without an error, asks
/// Windows nothing and says so on each line that would.
#[cfg(not(windows))]
#[test]
fn the_native_build_reports_and_leaves_windows_out() {
    let cli = crate::Cli::try_parse_from(["victory", "doctor"]).unwrap();
    let mut code = None;
    let (text, result) = crate::testing::report(|out| {
        code = Some(crate::dispatch(cli, out)?);
        Ok(())
    });
    result.expect("doctor ends without an error");
    for name in [
        "cpu",
        "d3d12 device",
        "adapter",
        "shader model",
        "mesh shaders",
        "64-bit atomics",
        "castable formats",
        "dxil validator",
    ] {
        assert!(
            text.contains(&format!("\nnot checked {name}: {WINE_ONLY}\n")),
            "{name} in:\n{text}"
        );
    }
    assert!(text.starts_with("not checked host: "), "{text}");
    // The prebuilt shaders are in the binary and need no Windows.
    for name in ["clamp bytecode", "depth bytecode"] {
        assert!(text.contains(&format!("\nok          {name}: ")), "{text}");
    }
    let problems = text.lines().filter(|l| l.starts_with("PROBLEM")).count();
    assert_eq!(code, Some(u32::from(problems > 0)), "{text}");
}
