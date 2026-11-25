//! Indirect execution's arguments, bounded before the GPU reads them. The
//! title's own compute passes write most of them; a pass we drop or
//! translate wrongly leaves whatever the memory held, and a draw of four
//! billion vertices or a dispatch past the host's limits runs until the
//! host resets the display driver. So every record is clamped on the GPU,
//! just before its execution, by a compute shader of ours (HLSL compiled by
//! the system's compiler; a 5.1 shader with its root signature inside).
//!
//! Each list also has a budget (2^27 vertex instances or threads, groups
//! counted as 64 threads, mesh groups as 128): records past it run empty,
//! so a list of thousands of garbage records cannot run for seconds
//! either.
//!
//! shortcut: the bounds (2^24 vertices, 2^26 vertex instances, 65,535
//! groups per dimension and 2^20 per dispatch, one-dimensional mesh
//! dispatches, the budget) cut legitimate work beyond them; raise them if
//! the title needs more.

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::s;

use crate::host::{HostList, host, transition};

/// The clamp shader. The clamp-bytecode workflow compiles it on a Windows
/// runner, and `clamp_bytecode.rs` holds the result, because Wine's HLSL
/// compiler (vkd3d-shader) cannot compile it; that bytecode is used only
/// while its `SOURCE_HASH` names this text.
const SOURCE: &str = include_str!("clamp.hlsl");

struct Clamp {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Clamp {}
// SAFETY: as above.
unsafe impl Sync for Clamp {}

fn clamp() -> Option<&'static Clamp> {
    static CLAMP: std::sync::OnceLock<Option<Clamp>> = std::sync::OnceLock::new();
    CLAMP
        .get_or_init(|| match make() {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(error = %e, "no argument clamp: indirect execution dropped");
                None
            }
        })
        .as_ref()
}

/// FNV-1a, the same hash `cargo xtask embed-clamp` writes.
pub(crate) fn fnv(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    })
}

/// The hash of the shader source, which names the bytecode it was built
/// into.
pub(crate) fn source_hash() -> u64 {
    fnv(SOURCE.as_bytes())
}

/// Whether the prebuilt bytecode was built from another source than the
/// present `clamp.hlsl`. It is not used then: the shader's dispatch and
/// its records' layout are this file's, and the bytecode's may differ.
fn stale() -> bool {
    crate::clamp_bytecode::SOURCE_HASH != source_hash()
}

fn make() -> windows::core::Result<Clamp> {
    let device = &host().ok_or_else(windows::core::Error::empty)?.device;
    // Prebuilt bytecode first, as long as it was built from this source.
    if stale() {
        tracing::warn!(
            "the prebuilt argument clamp is older than clamp.hlsl and is not used \
             (rebuild it with the clamp bytecode workflow); compiling the shader here"
        );
    } else {
        for bytes in [crate::clamp_bytecode::DXIL, crate::clamp_bytecode::DXBC] {
            if bytes.is_empty() {
                continue;
            }
            match from_bytecode(device, bytes) {
                Ok(c) => return Ok(c),
                Err(e) => tracing::warn!(error = %e, "prebuilt argument clamp refused"),
            }
        }
    }
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(
            SOURCE.as_ptr().cast(),
            SOURCE.len(),
            s!("clamp"),
            None,
            None,
            s!("main"),
            s!("cs_5_1"),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = compiled {
        if let Some(errors) = errors {
            // SAFETY: the blob's own bytes.
            let text = unsafe {
                std::slice::from_raw_parts(errors.GetBufferPointer().cast::<u8>(), errors.GetBufferSize())
            };
            tracing::warn!("argument clamp: {}", String::from_utf8_lossy(text));
        }
        return Err(e);
    }
    let code = code.ok_or_else(windows::core::Error::empty)?;
    // SAFETY: the blob's own bytes.
    let bytes = unsafe {
        std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize())
    };
    from_bytecode(device, bytes)
}

/// The root signature and pipeline of compiled clamp bytecode, whose root
/// signature is inside it.
fn from_bytecode(device: &ID3D12Device, bytes: &[u8]) -> windows::core::Result<Clamp> {
    // SAFETY: the bytes live through both calls.
    unsafe {
        let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
        let pso = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: bytes.as_ptr().cast(),
                BytecodeLength: bytes.len(),
            },
            ..Default::default()
        })?;
        Ok(Clamp { root, pso })
    }
}

/// Records, in `list`, the clamp of `records` records of `kind` (PC
/// argument numbering) `stride` bytes apart at host address `gpu` in
/// `buffer`, and leaves the buffer ready to be read as arguments. Replaces
/// the list's compute root signature and pipeline (callers bind theirs
/// after). False if the clamp is not available.
pub(crate) fn bound(
    list: &HostList,
    buffer: &ID3D12Resource,
    gpu: u64,
    (kind, stride, records): (u32, u32, u32),
) -> bool {
    let (Some(c), Some(budget)) = (clamp(), list.budget()) else {
        return false;
    };
    if !gpu.is_multiple_of(4) || !stride.is_multiple_of(4) {
        return false;
    }
    let l = &list.list;
    let constants = [kind, stride, records, 0];
    // SAFETY: an open list; our root signature, pipeline and a live buffer
    // the caller's list holds.
    unsafe {
        // Earlier shader writes (the title's argument passes) first.
        l.ResourceBarrier(&[uav_barrier()]);
        l.SetComputeRootSignature(&c.root);
        l.SetPipelineState(&c.pso);
        l.SetComputeRoot32BitConstants(0, 4, constants.as_ptr().cast(), 0);
        l.SetComputeRootUnorderedAccessView(1, gpu);
        l.SetComputeRootUnorderedAccessView(2, budget);
        // One group walks every record.
        l.Dispatch(records.min(1), 1, 1);
        l.ResourceBarrier(&[
            uav_barrier(),
            transition(
                buffer,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
            ),
        ]);
    }
    true
}

/// After the execution: the buffer back to unordered access, where the
/// title's passes keep using it.
pub(crate) fn done(list: &HostList, buffer: &ID3D12Resource) {
    // SAFETY: an open list and a live buffer.
    unsafe {
        list.list.ResourceBarrier(&[transition(
            buffer,
            D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        )])
    };
}

fn uav_barrier() -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER { pResource: std::mem::ManuallyDrop::new(None) }),
        },
    }
}

#[cfg(test)]
mod tests {
    /// The same vectors as xtask's `fnv_matches_the_gpu_crates_source_hash`
    /// (the published ones for 64-bit FNV-1a): the two copies agree.
    #[test]
    fn fnv_matches_embed_clamp() {
        assert_eq!(super::fnv(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(super::fnv(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(super::fnv(b"foobar"), 0x8594_4171_f739_67e8);
    }

    /// The prebuilt bytecode is the one in use: its hash names this source.
    /// Otherwise the shader is compiled here, which Wine cannot do.
    ///
    /// While this fails the embed is stale: `clamp.hlsl` changed and
    /// `clamp_bytecode.rs` was not rebuilt (the clamp bytecode workflow on
    /// a Windows runner, then `cargo xtask embed-clamp` and the policy
    /// entry it prints). Under Wine there is no argument clamp then, and
    /// every indirect execution is dropped. The tests below that need the
    /// clamp say "skipped" meanwhile; this one must stay red.
    #[test]
    fn prebuilt_clamp_names_this_source() {
        assert_eq!(crate::clamp_bytecode::SOURCE_HASH, super::source_hash());
    }

    /// Whether there is a clamp for `test`, which needs one. Without one
    /// the test did not run: while the prebuilt bytecode is stale and the
    /// host cannot compile the shader itself, that is the line `skipped:
    /// the prebuilt clamp bytecode is older than clamp.hlsl; ...
    /// (<test>)`, or a failure with `VICTORY_REQUIRE_CLAMP=1`.
    ///
    /// # Panics
    ///
    /// Without a clamp for any other reason: the prebuilt bytecode names
    /// this source and the host refused it, which no rebuild explains.
    fn clamp_for(test: &str) -> bool {
        if super::clamp().is_some() {
            return true;
        }
        assert!(
            super::stale(),
            "{test}: no clamp, though the prebuilt bytecode names this source"
        );
        crate::test_support::stale_clamp(test);
        false
    }

    /// The clamp compiles with the system's compiler and the host takes it.
    #[test]
    fn argument_clamp_builds() {
        let Some(_gpu) = crate::testing::gpu() else {
            return;
        };
        if !clamp_for("indirect::argument_clamp_builds") {
            return;
        }
        assert!(super::clamp().is_some());
    }

    /// Clamps `records` records of `kind`, filled by repeating `words`,
    /// `calls` times in one list: each record's first word after (zero when
    /// it runs empty). One record more follows them and must stay as
    /// written. For a test that holds the host device.
    fn clamped(kind: u32, words: &[u32], records: u32, calls: u32) -> Vec<u32> {
        let host = super::host().expect("a host device: the test holds it");
        // Words in a record of `kind`.
        let per = match kind {
            0 => 4,
            1 => 5,
            _ => 3,
        };
        let spec = (kind, per as u32 * 4, records);
        let all = per * (records as usize + 1);
        let (buffer, data) = host.shared_buffer(all as u64 * 4).unwrap();
        let data = data.cast::<u32>();
        let list = host.new_list().unwrap();
        // SAFETY: our mapped buffer, written before the list runs and read
        // after it has; a live buffer.
        unsafe {
            for at in 0..all {
                data.add(at).write_volatile(words[at % words.len()]);
            }
            let gpu = buffer.GetGPUVirtualAddress();
            for _ in 0..calls {
                assert!(super::bound(&list, &buffer, gpu, spec));
                super::done(&list, &buffer);
            }
            list.close();
            host.execute(&[&list]);
            for at in all - per..all {
                assert_eq!(data.add(at).read_volatile(), words[at % words.len()]);
            }
            let first = |i: usize| data.add(i * per).read_volatile();
            (0..records as usize).map(first).collect()
        }
    }

    /// Rejected records do not wrap the budget: of 128 garbage draws (2^26
    /// vertex instances each once clamped) two run, where a wrapping add
    /// let four; of 600 garbage mesh dispatches 16, not 31.
    #[test]
    fn rejected_records_do_not_wrap_the_budget() {
        let Some(_gpu) = crate::testing::gpu() else {
            return;
        };
        if !clamp_for("indirect::rejected_records_do_not_wrap_the_budget") {
            return;
        }
        let draws = clamped(0, &[u32::MAX], 128, 1);
        assert_eq!(draws.iter().filter(|&&count| count != 0).count(), 2);
        let meshes = clamped(10, &[u32::MAX], 600, 1);
        assert_eq!(meshes.iter().filter(|&&groups| groups != 0).count(), 16);
    }

    /// The budget goes to the first records, in order: of 200 draws of
    /// 3 * 2^18 vertices the first 170 run (it ends inside the third tile);
    /// small records that all fit all run.
    #[test]
    fn the_budget_goes_to_the_first_records() {
        let Some(_gpu) = crate::testing::gpu() else {
            return;
        };
        if !clamp_for("indirect::the_budget_goes_to_the_first_records") {
            return;
        }
        let draws = clamped(0, &[3 << 18, 1, 0, 0], 200, 1);
        assert!(draws[..170].iter().all(|&count| count != 0));
        assert!(draws[170..].iter().all(|&count| count == 0));
        let small = clamped(0, &[1], 1 << 16, 1);
        assert!(small.iter().all(|&count| count == 1));
    }

    /// A record that does not fit ends the budget: after two draws of
    /// 3 * 2^24 vertex instances a third takes what is left, and the small
    /// ones after it run empty.
    #[test]
    fn a_record_that_does_not_fit_ends_the_budget() {
        let Some(_gpu) = crate::testing::gpu() else {
            return;
        };
        if !clamp_for("indirect::a_record_that_does_not_fit_ends_the_budget") {
            return;
        }
        let mut words = [1 << 24, 3, 0, 0].repeat(3);
        words.extend([3, 1, 0, 0].repeat(6));
        let draws = clamped(0, &words, 8, 1);
        let ran: Vec<bool> = draws.iter().map(|&count| count != 0).collect();
        assert_eq!(ran, [true, true, false, false, false, false, false, false]);
    }

    /// The budget is the list's: a second execution of the same records
    /// finds it spent, and they all run empty.
    #[test]
    fn a_spent_budget_stays_spent() {
        let Some(_gpu) = crate::testing::gpu() else {
            return;
        };
        if !clamp_for("indirect::a_spent_budget_stays_spent") {
            return;
        }
        let twice = clamped(1, &[u32::MAX], 64, 2);
        assert!(twice.iter().all(|&count| count == 0));
    }
}
