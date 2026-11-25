//! What the crate's tests share: the host device behind the device tests'
//! lock ([`crate::test_support`], which other crates' tests use too), and
//! method slots looked up by an object's own layout.

use crate::object::This;
#[cfg(windows)]
use crate::test_support::Serial;
pub(crate) use crate::test_support::serial;

/// The host device with the device tests' lock held
/// ([`crate::test_support::gpu`]), for a test that needs a device; tests
/// keep the guard to their end. Without a device this is `None` after the
/// "skipped" line, and the test returns; with `VICTORY_REQUIRE_GPU=1` it
/// panics instead, which fails the test.
#[cfg(windows)]
pub(crate) fn gpu() -> Option<(Serial, &'static crate::host::Host)> {
    let guard = crate::test_support::gpu()?;
    crate::host::host().map(|host| (guard, host))
}

/// Whether `host` is Apple's D3DMetal: the process runs under Wine and the
/// device's adapter names itself "AMD Compatibility Mode", as
/// `victory-shader`'s tests recognise it. For the tests that show a
/// difference measured there ([`crate::test_support::host_differs`]).
#[cfg(windows)]
pub(crate) fn d3dmetal(host: &crate::host::Host) -> bool {
    crate::test_support::under_wine()
        && crate::host::adapter_name(&host.device) == "AMD Compatibility Mode"
}

/// For a test whose own assertions hold without a device but which
/// reaches the device where there is one (it creates a command list): the
/// lock, and whether there is a device for the test to assert the host
/// half with. Without one the "skipped" line is printed for that half,
/// and `VICTORY_REQUIRE_GPU=1` fails the test, as in [`gpu`].
#[cfg(windows)]
pub(crate) fn serial_with_device() -> (Serial, bool) {
    crate::test_support::serial_with_device()
}

/// The compute shader `source` (HLSL, entry point `main`, with its root
/// signature inside) compiled for `test` by the system's compiler as shader
/// model 5.1 with `flags` (`D3DCOMPILE_*`): the bytecode.
///
/// Under Wine that compiler is vkd3d-shader's, which lacks parts of the
/// language and compiles other sources without their root signature. Where
/// it cannot build the source this is `None` after the line
/// `skipped: the host HLSL compiler cannot build this test shader
/// (<test>)` and the compiler's messages, and the test returns: it did
/// not run. With `VICTORY_REQUIRE_HLSL=1` it fails instead. A run that
/// names a `dxc.exe` in `VICTORY_PC_DXC` gets the source built by that
/// compiler there, as shader model 6.0 ([`compiled_as`]).
///
/// # Panics
///
/// Where the process is not under Wine and the source does not compile:
/// the compiler is Microsoft's, and that fails the test as it does
/// upstream ([`crate::test_support::no_hlsl`]).
#[cfg(windows)]
pub(crate) fn compiled(test: &str, source: &str, flags: u32) -> Option<Vec<u8>> {
    compiled_as(test, source, (c"main", c"cs_5_1"), flags, true)
}

/// As [`compiled`], for the entry point and target profile `stage` (say
/// `(c"vs", c"vs_5_1")`). `rooted`: the stage carries the source's
/// `[RootSignature]`, and bytecode without that part is a refusal too.
///
/// Where Wine's compiler refused the source and [`PC_DXC`] names a
/// `dxc.exe`, that compiler builds the same source instead, as shader
/// model 6.0 of the same stage ([`dxc_compiled`]), and the test runs on
/// what it built; the first time in a process, one line on standard error
/// says so. Only a refusal leads there: a host whose `D3DCompile` builds
/// the source (Microsoft's does) never starts DXC, and Microsoft's
/// refusing still fails the test. Where DXC cannot build the source
/// either, the test is skipped or fails as without it, with both
/// compilers' messages.
#[cfg(windows)]
pub(crate) fn compiled_as(
    test: &str,
    source: &str,
    (entry, target): (&std::ffi::CStr, &std::ffi::CStr),
    flags: u32,
    rooted: bool,
) -> Option<Vec<u8>> {
    let refusal = match host_compiled(test, source, (entry, target), flags, rooted) {
        Ok(code) => return Some(code),
        Err(text) => text,
    };
    built_by_dxc(test, source, (entry, target), flags, rooted, &refusal)
}

/// What a test goes on with after Wine's `D3DCompile` refused its shader
/// `source` with `refusal`: the container DXC built of it
/// ([`dxc_compiled`]) where [`PC_DXC`] names a `dxc.exe`, after the one
/// line a process says about that. Otherwise `None` after the test's
/// "skipped" line, or its failure with `VICTORY_REQUIRE_HLSL=1`
/// ([`crate::test_support::no_hlsl`]): no DXC is named, or it did not
/// build the source either, and then the messages are both compilers'.
///
/// # Panics
///
/// Where the process is not under Wine: Microsoft's compiler refusing a
/// test's shader fails the test, and DXC is not asked.
#[cfg(windows)]
pub(crate) fn built_by_dxc(
    test: &str,
    source: &str,
    stage: (&std::ffi::CStr, &std::ffi::CStr),
    flags: u32,
    rooted: bool,
    refusal: &str,
) -> Option<Vec<u8>> {
    let dxc = std::env::var_os(PC_DXC)
        .filter(|path| !path.is_empty() && crate::test_support::under_wine());
    let Some(dxc) = dxc else {
        crate::test_support::no_hlsl(test, refusal);
        return None;
    };
    let dxc = std::path::PathBuf::from(dxc);
    match dxc_compiled(&dxc, source, stage, flags, rooted) {
        Ok(code) => {
            static SAID: std::sync::Once = std::sync::Once::new();
            SAID.call_once(|| {
                use std::io::Write;
                let _ = writeln!(std::io::stderr(), "{}", dxc_note(&dxc.to_string_lossy()));
            });
            Some(code)
        }
        Err(why) => {
            let both = format!("{refusal}\nand DXC ({PC_DXC}) did not build it either: {why}");
            crate::test_support::no_hlsl(test, &both);
            None
        }
    }
}

/// The variable that names a PC `dxc.exe` (Microsoft's DirectX Shader
/// Compiler), as victory-shader's tests have it. Here it is the compiler
/// for the test shaders Wine's `D3DCompile` refuses ([`compiled_as`]);
/// `tools/macos/test-wine.sh` makes it from `VICTORY_PC_DXC_UNIX`.
#[cfg(windows)]
const PC_DXC: &str = "VICTORY_PC_DXC";

/// What a run says, once, when DXC (`dxc`) built test shaders in place of
/// the host's compiler: those tests ran on shader model 6.0 bytecode, not
/// on the 5.1 bytecode upstream's run on.
fn dxc_note(dxc: &str) -> String {
    format!(
        "note: the host HLSL compiler cannot build some test shaders; \
         DXC built them as shader model 6.0 (VICTORY_PC_DXC={dxc})"
    )
}

/// The shader model 6.0 profile of the stage of `target` (a `D3DCompile`
/// profile such as `cs_5_1`): `cs_6_0`. `None` for what is no profile of
/// a stage.
fn dxc_target(target: &str) -> Option<String> {
    let (stage, model) = target.split_once('_')?;
    let known = ["cs", "vs", "ps", "gs", "hs", "ds"].contains(&stage);
    let model = model.chars().all(|c| c.is_ascii_digit() || c == '_') && !model.is_empty();
    (known && model).then(|| format!("{stage}_6_0"))
}

/// `source` compiled by the system's `D3DCompile` as [`compiled_as`] has
/// it, or why it is refused: its messages, or that it left the root signature out.
#[cfg(windows)]
fn host_compiled(
    test: &str,
    source: &str,
    (entry, target): (&std::ffi::CStr, &std::ffi::CStr),
    flags: u32,
    rooted: bool,
) -> Result<Vec<u8>, String> {
    use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
    use windows::Win32::Graphics::Direct3D::ID3DBlob;
    use windows::core::PCSTR;
    // The messages name the source by the test.
    let name = std::ffi::CString::new(test).expect("a test name without NUL");
    let (mut code, mut errors): (Option<ID3DBlob>, Option<ID3DBlob>) = (None, None);
    // SAFETY: the source and names outlive the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            PCSTR(name.as_ptr().cast()),
            None,
            None,
            PCSTR(entry.as_ptr().cast()),
            PCSTR(target.as_ptr().cast()),
            flags,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    // SAFETY: a blob's own bytes, copied while it lives.
    let bytes_of = |blob: &ID3DBlob| unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
            .to_vec()
    };
    let code = match (compiled, code) {
        (Ok(()), Some(code)) => bytes_of(&code),
        (result, _) => {
            let text = errors.as_ref().map(bytes_of).unwrap_or_default();
            let text = String::from_utf8_lossy(&text);
            return Err(match text.trim_end_matches(['\0', '\n', '\r']) {
                "" => format!("{test}: {result:?} and no messages"),
                text => text.to_owned(),
            });
        }
    };
    // vkd3d-shader's compiler builds some of these sources and leaves the
    // `[RootSignature]` attribute out of what it writes, and without that
    // part there is neither a root signature nor a pipeline to make.
    if rooted && !crate::root::holds_a_root_signature(&code) {
        return Err(
            "the compiler left the [RootSignature] out: no RTS0 part in the shader".to_owned(),
        );
    }
    Ok(code)
}

/// `source` compiled by the `dxc.exe` at `dxc`, started as a process of
/// its own, for the entry point of `stage` and the shader model 6.0
/// profile of its stage ([`dxc_target`]), as HLSL 2018 (the language
/// `D3DCompile` reads; DXC's default is a later one): the container, or
/// why there is none. DXC writes the entry point's `[RootSignature]` into
/// the container as `D3DCompile` does, and with `rooted` a container
/// without that part is an error here too.
///
/// `flags` are `D3DCompile`'s. The one the tests pass,
/// `D3DCOMPILE_ENABLE_UNBOUNDED_DESCRIPTOR_TABLES`, is what shader model 6
/// always has; any other is an error, since nothing here hands it on.
#[cfg(windows)]
fn dxc_compiled(
    dxc: &std::path::Path,
    source: &str,
    (entry, target): (&std::ffi::CStr, &std::ffi::CStr),
    flags: u32,
    rooted: bool,
) -> Result<Vec<u8>, String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use windows::Win32::Graphics::Direct3D::Fxc::D3DCOMPILE_ENABLE_UNBOUNDED_DESCRIPTOR_TABLES;
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let profile = target.to_str().ok().and_then(dxc_target);
    let profile = profile.ok_or_else(|| format!("no shader model 6.0 profile for {target:?}"))?;
    let entry = entry
        .to_str()
        .map_err(|_| format!("entry point {entry:?}"))?;
    if flags & !D3DCOMPILE_ENABLE_UNBOUNDED_DESCRIPTOR_TABLES != 0 {
        return Err(format!(
            "D3DCompile flags {flags:#x} are not handed on to DXC"
        ));
    }
    let stem = format!(
        "victory-test-shader-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let input = std::env::temp_dir().join(format!("{stem}.hlsl"));
    let output = std::env::temp_dir().join(format!("{stem}.dxil"));
    std::fs::write(&input, source).map_err(|e| format!("{}: {e}", input.display()))?;
    let ran = std::process::Command::new(dxc)
        .args(["-nologo", "-HV", "2018", "-T", &profile, "-E", entry, "-Fo"])
        .arg(&output)
        .arg(&input)
        .output();
    let code = std::fs::read(&output);
    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
    let ran = ran.map_err(|e| format!("{} did not start: {e}", dxc.display()))?;
    if !ran.status.success() {
        let said = String::from_utf8_lossy(&ran.stderr);
        return Err(format!("{}: {}", ran.status, said.trim_end()));
    }
    let code = code.map_err(|e| format!("{} wrote no container: {e}", dxc.display()))?;
    if rooted && !crate::root::holds_a_root_signature(&code) {
        return Err("DXC left the [RootSignature] out: no RTS0 part in the container".to_owned());
    }
    Ok(code)
}

/// The root signature and pipeline of the compute shader `source`,
/// compiled for `test` as [`compiled`] compiles it: `None` where that is
/// (the test did not run, and its "skipped" line says so).
///
/// # Panics
///
/// If the host refuses the compiled shader: that is the host's fault or
/// the test's, not the compiler's.
#[cfg(windows)]
pub(crate) fn compute_shader(
    test: &str,
    source: &str,
    flags: u32,
) -> Option<(
    windows::Win32::Graphics::Direct3D12::ID3D12RootSignature,
    windows::Win32::Graphics::Direct3D12::ID3D12PipelineState,
)> {
    use windows::Win32::Graphics::Direct3D12::*;
    let host = crate::host::host().expect("a host device: the test holds it");
    let code = compiled(test, source, flags)?;
    // SAFETY: a compiled shader with its root signature inside; the bytes
    // live through both calls.
    unsafe {
        let root: ID3D12RootSignature = host.device.CreateRootSignature(0, &code).unwrap();
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: code.as_ptr().cast(),
                BytecodeLength: code.len(),
            },
            ..Default::default()
        };
        let pso = host.device.CreateComputePipelineState(&desc).unwrap();
        Some((root, pso))
    }
}

/// The list method that older lists have at `old_offset`, wherever
/// `list`'s own layout has it. Lists are made in the layout the device was
/// last created for (`device::newer`), which another test may switch for
/// the whole test binary at any time, so the object's table decides.
pub(crate) fn list_slot<F: Copy>(list: This, old_offset: usize) -> F {
    let slot = old_offset / 8;
    let slot = if crate::list::is_newer(list) {
        crate::list::newer_slot(slot)
    } else {
        slot
    };
    crate::object::slot(list, slot * 8)
}

/// As [`list_slot`], for a queue.
pub(crate) fn queue_slot<F: Copy>(queue: This, old_offset: usize) -> F {
    let slot = old_offset / 8;
    let slot = if crate::queue::is_newer(queue) {
        crate::queue::newer_slot(slot)
    } else {
        slot
    };
    crate::object::slot(queue, slot * 8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;

    /// ExecuteIndirect is at 0x1d8 in older lists and 0x1e8 in newer ones,
    /// a queue's Signal at 0x70 and 0x80: asked for by the older offset,
    /// both are found in either layout.
    #[test]
    fn slots_follow_the_objects_own_layout() {
        type Method = *const ();
        let (old, execute_indirect) = crate::list::test_layout(false);
        let (new, _) = crate::list::test_layout(true);
        let (old, new): (This, This) = (&old, &new);
        assert_eq!(list_slot::<Method>(old, 0x1d8), execute_indirect);
        assert_eq!(list_slot::<Method>(new, 0x1d8), execute_indirect);
        assert_eq!(slot::<Method>(new, 0x1e8), execute_indirect);
        assert_ne!(slot::<Method>(new, 0x1d8), execute_indirect);
        // Before the added methods nothing moved (Close, 0x48).
        assert_eq!(list_slot::<Method>(new, 0x48), slot::<Method>(old, 0x48));

        let (old, signal) = crate::queue::test_layout(false);
        let (new, _) = crate::queue::test_layout(true);
        let (old, new): (This, This) = (&old, &new);
        assert_eq!(queue_slot::<Method>(old, 0x70), signal);
        assert_eq!(queue_slot::<Method>(new, 0x70), signal);
        assert_eq!(slot::<Method>(new, 0x80), signal);
        assert_ne!(slot::<Method>(new, 0x70), signal);
        // Submission stays at 0x50.
        assert_eq!(queue_slot::<Method>(new, 0x50), slot::<Method>(old, 0x50));
    }

    /// DXC builds a test's shader for the stage `D3DCompile` was asked
    /// for, as shader model 6.0 whatever the model was; and its one line
    /// is no skip line, since those tests ran.
    #[test]
    fn dxc_builds_the_same_stage_as_shader_model_6_0() {
        assert_eq!(dxc_target("cs_5_1").as_deref(), Some("cs_6_0"));
        assert_eq!(dxc_target("vs_5_1").as_deref(), Some("vs_6_0"));
        assert_eq!(dxc_target("ps_5_0").as_deref(), Some("ps_6_0"));
        for other in ["", "cs", "cs_", "lib_6_3", "fx_5_0", "cs_5_1 -Od"] {
            assert_eq!(dxc_target(other), None, "{other:?}");
        }
        let note = dxc_note(r"Z:\dxc\dxc.exe");
        assert!(!note.starts_with("skipped: "), "{note}");
        assert!(
            note.contains("shader model 6.0") && note.ends_with(r"(VICTORY_PC_DXC=Z:\dxc\dxc.exe)")
        );
    }
}
