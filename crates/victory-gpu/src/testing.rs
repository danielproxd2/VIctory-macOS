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
/// not run. With `VICTORY_REQUIRE_HLSL=1` it fails instead.
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
#[cfg(windows)]
pub(crate) fn compiled_as(
    test: &str,
    source: &str,
    (entry, target): (&std::ffi::CStr, &std::ffi::CStr),
    flags: u32,
    rooted: bool,
) -> Option<Vec<u8>> {
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
            let text = match text.trim_end_matches(['\0', '\n', '\r']) {
                "" => format!("{test}: {result:?} and no messages"),
                text => text.to_owned(),
            };
            crate::test_support::no_hlsl(test, &text);
            return None;
        }
    };
    // vkd3d-shader's compiler builds some of these sources and leaves the
    // `[RootSignature]` attribute out of what it writes, and without that
    // part there is neither a root signature nor a pipeline to make.
    if rooted && !crate::root::holds_a_root_signature(&code) {
        let text = "the compiler left the [RootSignature] out: no RTS0 part in the shader";
        crate::test_support::no_hlsl(test, text);
        return None;
    }
    Some(code)
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
}
