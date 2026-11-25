//! docs/first-run.md tells whoever runs a real game which line VIctory
//! prints at each stage that refuses to go on. A line quoted there that the
//! program no longer prints would send them looking for the wrong thing, so
//! each quoted message is checked here against the source that prints it.
//! With the tests of the scripts it names, this runs on the Mac itself.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace")
        .into()
}

/// A message, or the part of it that is on one line of the source, and the
/// file that prints it.
const MESSAGES: &[(&str, &str)] = &[
    // The launcher.
    (
        "no command given, and no game is started by default.",
        "tools/macos/victory.sh",
    ),
    (
        ": run tools/macos/build-crossover-wine.sh first",
        "tools/macos/victory.sh",
    ),
    (
        "D3DMetal is missing: copy the redist/lib folder",
        "tools/macos/victory.sh",
    ),
    (
        "started inside the VIctory source tree; the title runs in its own folder,",
        "tools/macos/victory.sh",
    ),
    (
        ": it is inside a VIctory source tree",
        "tools/macos/victory.sh",
    ),
    ("cannot read the config file", "tools/macos/victory.sh"),
    // The processor check.
    (
        "this CPU does not support AVX2, which Xbox Series titles require;",
        "apps/victory/src/run.rs",
    ),
    (
        "(for example under CPU emulation such as Intel SDE)",
        "apps/victory/src/run.rs",
    ),
    // The configuration and the folders.
    (
        "VICTORY_CONFIG points at",
        "crates/victory-core/src/config.rs",
    ),
    (
        ", which does not exist",
        "crates/victory-core/src/config.rs",
    ),
    (
        "for game-derived data: it is inside the VIctory source tree",
        "crates/victory-core/src/policy.rs",
    ),
    ("POLICY VIOLATION:", "apps/victory/src/main.rs"),
    ("NOTE: game_dir is not set", "apps/victory/src/main.rs"),
    ("MISSING: game_dir", "apps/victory/src/main.rs"),
    (
        "the title's own DLLs run",
        "crates/victory-hle/src/modules/fidelityfx.rs",
    ),
    (
        "no FidelityFX folder (VICTORY_FIDELITYFX_DIR): a title's own FidelityFX DLLs run",
        "crates/victory-hle/src/modules/fidelityfx.rs",
    ),
    // The executable, the build and its startup archive.
    (
        "is a DLL, which cannot be started as a title (without --execute it is mapped and bound)",
        "apps/victory/src/run.rs",
    ),
    ("build          unknown (", "apps/victory/src/run.rs"),
    ("Series 1.0.2.1", "crates/victory-game/src/lib.rs"),
    ("appdata0.rpf", "crates/victory-game/src/lib.rs"),
    ("Cannot read startup archive", "apps/victory/src/run.rs"),
    (
        "Check that the file from your package is present and readable.",
        "apps/victory/src/run.rs",
    ),
    ("Invalid RPF9 header in", "apps/victory/src/run.rs"),
    (
        "Restore the startup archive from your package.",
        "apps/victory/src/run.rs",
    ),
    // The manifest, the DLLs and the imports.
    (
        "manifest       not taken as it stands:",
        "apps/victory/src/run.rs",
    ),
    ("missing dlls   ", "apps/victory/src/run.rs"),
    (
        "incomplete; copy every file of the package next to the executable",
        "apps/victory/src/run.rs",
    ),
    (
        "the title's own DLLs are missing:",
        "apps/victory/src/run.rs",
    ),
    ("runs it anyway)", "apps/victory/src/run.rs"),
    ("  missing      ", "apps/victory/src/run.rs"),
    ("title exited with", "apps/victory/src/run.rs"),
    (
        "unimplemented import called",
        "crates/victory-loader/src/thunks.rs",
    ),
    (
        "unimplemented import called, returning 0",
        "crates/victory-loader/src/thunks.rs",
    ),
    (
        "VIctory: unimplemented import ",
        "crates/victory-loader/src/thunks.rs",
    ),
    (
        "a host DLL outside the host policy: refused",
        "crates/victory-hle/src/modules/kernel.rs",
    ),
    // The CPU.
    ("unhandled exception", "crates/victory-loader/src/crash.rs"),
    (
        "(unsupported AMD instruction ",
        "crates/victory-loader/src/crash.rs",
    ),
    ("movntss", "crates/victory-loader/src/crash.rs"),
    (
        "VIctory: panic on thread",
        "crates/victory-loader/src/crash.rs",
    ),
    // The GPU.
    (
        "no host D3D12 device; command lists will not run",
        "crates/victory-gpu/src/host.rs",
    ),
    (
        "the host GPU does not fuse multiply-adds: results of the console's fused v_fma and v_fmac can differ in kind here",
        "crates/victory-gpu/src/host.rs",
    ),
    (
        "shader does not translate; its pipelines' draws are dropped",
        "crates/victory-gpu/src/pipeline.rs",
    ),
    (
        "the translator panicked",
        "crates/victory-gpu/src/pipeline.rs",
    ),
    ("is not supported", "crates/victory-shader/src/lib.rs"),
    ("malformed shader:", "crates/victory-shader/src/lib.rs"),
    ("(logged once)", "crates/victory-gpu/src/pipeline/host.rs"),
    (
        "only vertex, mesh, pixel and compute stages are translated",
        "crates/victory-gpu/src/pipeline/host.rs",
    ),
    (
        "no vertex or mesh shader",
        "crates/victory-gpu/src/pipeline/host.rs",
    ),
    (
        "the host GPU has no mesh shaders (or no shader model 6.6)",
        "crates/victory-gpu/src/pipeline/host.rs",
    ),
    (
        "the host GPU has no 64-bit atomics on typed heap resources",
        "crates/victory-gpu/src/pipeline/host.rs",
    ),
    (
        "graphics state not kept (plain description)",
        "crates/victory-gpu/src/pipeline/host.rs",
    ),
    (
        "host root signature refused",
        "crates/victory-gpu/src/pipeline/host.rs",
    ),
    (
        "host pipeline refused",
        "crates/victory-gpu/src/pipeline/host.rs",
    ),
    (
        "host mesh pipeline refused",
        "crates/victory-gpu/src/pipeline/host.rs",
    ),
    (
        "making a host pipeline panicked; it has none and its draws are dropped",
        "crates/victory-gpu/src/pipeline/make.rs",
    ),
    (
        "no host pipeline library: pipelines are not kept",
        "crates/victory-gpu/src/pipeline/library.rs",
    ),
    (
        "the prebuilt argument clamp is older than clamp.hlsl and is not used",
        "crates/victory-gpu/src/indirect.rs",
    ),
    (
        "prebuilt argument clamp refused",
        "crates/victory-gpu/src/indirect.rs",
    ),
    (
        "no argument clamp: indirect execution dropped",
        "crates/victory-gpu/src/indirect.rs",
    ),
    (
        "no depth conversion: 16-bit depth written through unordered views does not reach the depth texture",
        "crates/victory-gpu/src/twin.rs",
    ),
    (
        "a draw or dispatch dropped: no dynamic slots for its tables",
        "crates/victory-gpu/src/draw.rs",
    ),
    (
        "no dynamic slots for root views: dropped",
        "crates/victory-gpu/src/draw.rs",
    ),
    (
        "a shader reads a root view the title did not set: dropped",
        "crates/victory-gpu/src/draw.rs",
    ),
    (
        "a dispatch without a host pipeline dropped",
        "crates/victory-gpu/src/draw.rs",
    ),
    (
        "indirect mesh dispatches dropped",
        "crates/victory-gpu/src/list.rs",
    ),
    (
        "more than D3D12 has; ignored",
        "crates/victory-gpu/src/draw.rs",
    ),
    (
        "without a host pipeline (",
        "crates/victory-gpu/src/host.rs",
    ),
    (
        "without a host pipeline: ",
        "crates/victory-gpu/src/host.rs",
    ),
    (
        "host-pipelines.bin",
        "crates/victory-gpu/src/pipeline/library.rs",
    ),
];

#[test]
fn the_first_run_guide_quotes_messages_the_program_prints() {
    let root = root();
    let guide = fs::read_to_string(root.join("docs/first-run.md")).unwrap();
    for (message, file) in MESSAGES {
        assert!(
            guide.contains(message),
            "docs/first-run.md does not quote {message:?} any more: remove it here"
        );
        let source = fs::read_to_string(root.join(file)).unwrap();
        assert!(
            source.contains(message),
            "docs/first-run.md quotes {message:?}, which {file} does not print any more"
        );
    }
}

/// The guide names the scripts and the switch it tells the user to use.
#[test]
fn the_first_run_guide_names_what_exists() {
    let root = root();
    let guide = fs::read_to_string(root.join("docs/first-run.md")).unwrap();
    for script in ["tools/macos/first-run.sh", "tools/macos/victory.sh"] {
        assert!(guide.contains(script), "{script}");
        assert!(root.join(script).is_file(), "{script}");
    }
    let launcher = fs::read_to_string(root.join("tools/macos/victory.sh")).unwrap();
    for variable in ["VICTORY_FEATURES", "ROSETTA_ADVERTISE_AVX", "WINEDEBUG"] {
        assert!(guide.contains(variable), "{variable}");
        assert!(launcher.contains(variable), "{variable}");
    }
    let aid = fs::read_to_string(root.join("tools/macos/first-run.sh")).unwrap();
    assert!(guide.contains("VICTORY_FIRST_RUN_DIR") && aid.contains("VICTORY_FIRST_RUN_DIR"));
    assert!(aid.contains("docs/first-run.md"));
}
