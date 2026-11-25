//! What the host GPU does with a program it says it can run, asked once
//! per device by running one.
//!
//! D3D12 hosts report 64-bit atomics on typed resources from the heap as
//! two capability bits. Apple's D3DMetal sets both, creates the pipeline
//! of a compute program that has such an atomic, completes its dispatch
//! without an error and runs nothing of it: neither the atomic nor any
//! other store of the same program arrives (docs/macos.md, "Open
//! differences under D3DMetal"). A title's passes that use them (upstream:
//! the alpha prepasses) would write nothing, with nothing said.
//!
//! So the capability is believed only after [`atomics64`] ran a program
//! with one 64-bit atomic on a typed view and one plain store, and read
//! both back. The program is GFX10 code of our own, lowered as a title's
//! is (as `victory_shader`'s check of fused multiply-adds is), so it is the
//! DXIL a title's atomics become. The same program without the atomic runs
//! first: if its store does not arrive either, the probe is what failed,
//! and the host's word stands.

use victory_shader::ShaderError;
use victory_shader::emit::HostShader;

/// The 64-bit value the probe's atomic exchanges into its texel: the low
/// dword in the texel's first component, the high one in its second.
pub(crate) const KEY: u64 = 0x8765_4321_0123_4567;
/// The dword the probe stores to its raw buffer beside the atomic.
pub(crate) const STORED: u32 = 0x5AFE_C0DE;

/// A compute program of one thread: `image_atomic_swap` of the 64-bit
/// value in s0 (low) and s1 (high) into texel (0, 0) of the `R32G32_UINT`
/// texture whose descriptor the table at s4 names, then
/// `buffer_store_dword` of s2 to dword 0 of the raw buffer the table at s6
/// names. Lowered as a title's program is.
pub(crate) fn atomics64_program() -> Result<HostShader, ShaderError> {
    parts(true).map(|(shader, _)| shader)
}

/// [`atomics64_program`] with its atomic (`atomic`), or the same program
/// without it, which only stores s2 to the raw buffer; and the
/// descriptors the code reads.
fn parts(atomic: bool) -> Result<(HostShader, Vec<victory_shader::UsedResource>), ShaderError> {
    use victory_shader::native::{ComputeIo, StageIo};
    use victory_shader::{HwStage, NativeProgram, ResourceType, RootParam, RootType, UsedResource};
    const ATOMIC: [u32; 9] = [
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF40C_0202, // s_load_dwordx8 s[8:15], s[4:5], 0
        0xFA00_0000,
        0x7E02_0200, // v_mov_b32 v1, s0
        0x7E04_0201, // v_mov_b32 v2, s1
        0x7E06_0280, // v_mov_b32 v3, 0
        0x7E08_0280, // v_mov_b32 v4, 0
        0xF03C_1308, // image_atomic_swap v[1:2], v[3:4], s[8:15] dmask:0x3 dim:2D unorm
        0x0002_0103,
    ];
    const STORE: [u32; 7] = [
        0xBE87_0380, // s_mov_b32 s7, 0
        0xF408_0403, // s_load_dwordx4 s[16:19], s[6:7], 0
        0xFA00_0000,
        0x7E0A_0202, // v_mov_b32 v5, s2
        0xE070_0000, // buffer_store_dword v5, off, s[16:19], 0
        0x8004_0500,
        0xBF81_0000, // s_endpgm
    ];
    let words: Vec<u32> = if atomic {
        ATOMIC.iter().chain(&STORE).copied().collect()
    } else {
        STORE.to_vec()
    };
    let param = |root, user_data, sgprs, slot| RootParam {
        root,
        table_offset_dwords: 0,
        sgprs,
        base_register: 0,
        user_data,
        slot,
    };
    let used = |dwords, user_data, slot| UsedResource {
        kind: ResourceType::Uav,
        root: RootType::Table,
        table_offset_dwords: 0,
        lower_bound: 0,
        stride: 0,
        dwords,
        user_data,
        slot,
    };
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 8,
        layout: vec![
            param(RootType::Constants, 0, 4, 0),
            param(RootType::Table, 4, 1, TEXTURE_SLOT),
            param(RootType::Table, 6, 1, BUFFER_SLOT),
        ],
        used: if atomic {
            vec![used(8, 4, TEXTURE_SLOT), used(4, 6, BUFFER_SLOT)]
        } else {
            vec![used(4, 6, BUFFER_SLOT)]
        },
        io: StageIo::Cs(ComputeIo {
            rsrc2: 7 << 1,
            threads: [1, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = victory_shader::ir::build(&native.code, native.wave64, native.vgprs)?;
    let st = victory_shader::structure::structure(&p)?;
    let res = victory_shader::resource::track(&p, &native.layout, native.stage, Some(&st))?;
    let io = victory_shader::io::raise(&p, &native)?;
    // A compute program has no signature parts to take from a container.
    let container = victory_shader::Container {
        digest: [0; 16],
        version: (1, 0),
        parts: Vec::new(),
    };
    let shader = victory_shader::emit::emit(&container, &native, &p, &st, &io, &res)?;
    Ok((shader, native.used))
}

/// The root parameter of the table that names the probe's texture, whose
/// view is at index 0 of the probe's heap.
const TEXTURE_SLOT: u8 = 1;
/// The root parameter of the table that names the probe's raw buffer,
/// whose view is at index 1.
const BUFFER_SLOT: u8 = 2;

/// The user data the probe's program runs with: its three constants, and
/// for each table the byte offset of its view in the heap (the console's
/// descriptors are 32 bytes: the heap index times 32). `None` if the
/// program reads user data the probe does not know.
fn user_data(shader: &HostShader) -> Option<Vec<u32>> {
    use victory_shader::RootType;
    let constants = [KEY as u32, (KEY >> 32) as u32, STORED];
    let mut data = vec![0u32; shader.user_data_dwords().max(1) as usize];
    for u in &shader.user_data {
        data[u.index as usize] = match (u.root, u.slot) {
            (RootType::Constants, 0) => *constants.get(u.dword as usize)?,
            (RootType::Table, TEXTURE_SLOT) => 0,
            (RootType::Table, BUFFER_SLOT) => 32,
            _ => return None,
        };
    }
    Some(data)
}

/// What the probe read back after its dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ran {
    /// Texel (0, 0) of the texture, zero before.
    pub(crate) texel: u64,
    /// Dword 0 of the raw buffer, zero before.
    pub(crate) stored: u32,
}

impl Ran {
    /// Whether the program ran: its atomic and its store both arrived.
    pub(crate) fn whole(self) -> bool {
        self.texel == KEY && self.stored == STORED
    }
}

/// What a host that reports the capability is taken to do, by what the
/// probe found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The capability is not reported: nothing was run.
    Unclaimed,
    /// Reported, and the probe's program ran.
    Works,
    /// Reported, and the probe's program did not do what it says: the
    /// host is taken not to have the capability.
    Claimed(Ran),
    /// Reported, and the probe could not be run (why): the host's word is
    /// taken, as it was before there was a probe.
    Unchecked(String),
}

impl Verdict {
    /// Whether pipelines with 64-bit atomics on typed heap resources are
    /// made for this host.
    pub(crate) fn usable(&self) -> bool {
        matches!(self, Verdict::Works | Verdict::Unchecked(_))
    }
}

/// The verdict for a host that reports the capability or not (`claimed`),
/// from what running the probe gives; it is not run where nothing is
/// claimed.
pub(crate) fn verdict(claimed: bool, probe: impl FnOnce() -> Result<Ran, String>) -> Verdict {
    if !claimed {
        return Verdict::Unclaimed;
    }
    match probe() {
        Ok(ran) if ran.whole() => Verdict::Works,
        Ok(ran) => Verdict::Claimed(ran),
        Err(why) => Verdict::Unchecked(why),
    }
}

/// Runs the probe on `device`: the program without its atomic, whose
/// store must arrive, then the program itself, and what that one wrote.
/// An error if the probe itself failed: something before a read-back (the
/// translation, a pipeline, a resource, a removed device), or the store
/// of the program without the atomic.
#[cfg(windows)]
pub(crate) fn atomics64(
    device: &windows::Win32::Graphics::Direct3D12::ID3D12Device,
) -> Result<Ran, String> {
    let plain = run(device, false)?;
    if plain.stored != STORED {
        return Err(format!(
            "the probe's program without an atomic stored {:#x}",
            plain.stored
        ));
    }
    run(device, true)
}

/// Runs the probe's program (or, without `atomic`, the one that only
/// stores) once on `device` and reads back the texel and the buffer.
#[cfg(windows)]
pub(crate) fn run(
    device: &windows::Win32::Graphics::Direct3D12::ID3D12Device,
    atomic: bool,
) -> Result<Ran, String> {
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::core::Interface;
    let err = |e: windows::core::Error| e.message().to_string();
    let (shader, _) = parts(atomic).map_err(|e| e.to_string())?;
    let user_data = user_data(&shader).ok_or("the probe reads user data it does not set")?;
    let host = victory_shader::emit::host::Host {
        device: device.clone(),
    };
    let (root, pso) = host.compute_pipeline(&shader)?;
    let dev = device;
    let sample = DXGI_SAMPLE_DESC {
        Count: 1,
        Quality: 0,
    };
    let buffer_desc = |flags| D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: 512,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: sample,
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
        ..Default::default()
    };
    let texture_desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: 4,
        Height: 4,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_R32G32_UINT,
        SampleDesc: sample,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let make = |heap, desc: &D3D12_RESOURCE_DESC, state| -> Result<ID3D12Resource, String> {
        let mut r: Option<ID3D12Resource> = None;
        // SAFETY: plain creation with descriptions that outlive the call.
        unsafe {
            dev.CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: heap,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                desc,
                state,
                None,
                &mut r,
            )
        }
        .map_err(err)?;
        r.ok_or_else(|| "no resource".to_string())
    };
    let to_copy_source = |r: &ID3D12Resource| D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                // SAFETY: a borrowed pointer the barrier does not release.
                pResource: unsafe { std::mem::transmute_copy(r) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                StateAfter: D3D12_RESOURCE_STATE_COPY_SOURCE,
            }),
        },
    };
    // Committed resources start zeroed: both read as zero unless written.
    let texture = make(
        D3D12_HEAP_TYPE_DEFAULT,
        &texture_desc,
        D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
    )?;
    let buffer = make(
        D3D12_HEAP_TYPE_DEFAULT,
        &buffer_desc(D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS),
        D3D12_RESOURCE_STATE_COMMON,
    )?;
    // Read back: the texture's first row at 0, the buffer's dword at 256.
    let readback = make(
        D3D12_HEAP_TYPE_READBACK,
        &buffer_desc(D3D12_RESOURCE_FLAG_NONE),
        D3D12_RESOURCE_STATE_COPY_DEST,
    )?;
    // SAFETY: D3D12 calls with descriptions that outlive them; the
    // resources stay alive until the fence says the GPU is done, and the
    // mapped read-back buffer is 512 bytes.
    unsafe {
        let queue: ID3D12CommandQueue = dev
            .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                ..Default::default()
            })
            .map_err(err)?;
        let alloc: ID3D12CommandAllocator = dev
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .map_err(err)?;
        let list: ID3D12GraphicsCommandList = dev
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, &pso)
            .map_err(err)?;
        let views: ID3D12DescriptorHeap = dev
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 2,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                ..Default::default()
            })
            .map_err(err)?;
        let mut at = views.GetCPUDescriptorHandleForHeapStart();
        dev.CreateUnorderedAccessView(&texture, None, None, at);
        at.ptr +=
            dev.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize;
        dev.CreateUnorderedAccessView(
            &buffer,
            None,
            Some(&D3D12_UNORDERED_ACCESS_VIEW_DESC {
                Format: DXGI_FORMAT_R32_TYPELESS,
                ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_UAV {
                        NumElements: 64,
                        Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                        ..Default::default()
                    },
                },
            }),
            at,
        );
        list.SetDescriptorHeaps(&[Some(views.clone())]);
        list.SetComputeRootSignature(&root);
        list.SetComputeRoot32BitConstants(0, user_data.len() as u32, user_data.as_ptr().cast(), 0);
        list.Dispatch(1, 1, 1);
        // (The buffer was promoted from the common state by the dispatch.)
        list.ResourceBarrier(&[to_copy_source(&texture), to_copy_source(&buffer)]);
        list.CopyTextureRegion(
            &D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&readback),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: DXGI_FORMAT_R32G32_UINT,
                            Width: 4,
                            Height: 1,
                            Depth: 1,
                            RowPitch: 256,
                        },
                    },
                },
            },
            0,
            0,
            0,
            &D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&texture),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: 0,
                },
            },
            Some(&D3D12_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: 4,
                bottom: 1,
                back: 1,
            }),
        );
        list.CopyBufferRegion(&readback, 256, &buffer, 0, 4);
        list.Close().map_err(err)?;
        queue.ExecuteCommandLists(&[Some(list.cast().map_err(err)?)]);
        let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).map_err(err)?;
        queue.Signal(&fence, 1).map_err(err)?;
        while fence.GetCompletedValue() < 1 {
            if let Err(e) = dev.GetDeviceRemovedReason() {
                return Err(format!("device removed: {}", err(e)));
            }
            std::thread::yield_now();
        }
        let mut p = std::ptr::null_mut();
        readback.Map(0, None, Some(&mut p)).map_err(err)?;
        let words = p.cast::<u32>();
        let ran = Ran {
            texel: u64::from(words.read()) | u64::from(words.add(1).read()) << 32,
            stored: words.add(64).read(),
        };
        readback.Unmap(0, None);
        Ok(ran)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe's program is what the pipeline check looks for: its
    /// container says it needs 64-bit atomics on typed resources and on
    /// heap resources (the `SFI0` bits `HostPipeline::new` refuses a
    /// pipeline for where the host has none), and it reads exactly the
    /// user data the probe sets.
    #[test]
    fn the_probe_program_is_one_the_pipeline_check_refuses() {
        let shader = atomics64_program().expect("the probe lowers");
        let container = victory_shader::Container::parse(&shader.dxil).unwrap();
        let flags = container
            .part(victory_shader::FourCC::SFI0)
            .expect("feature flags")
            .data;
        let flags = u32::from_le_bytes(flags[..4].try_into().unwrap());
        assert_eq!(flags & 0x1040_0000, 0x1040_0000, "{flags:#x}");
        assert!(shader.heap || !shader.user_data.is_empty());
        let data = user_data(&shader).expect("only the user data the probe sets");
        // Without the atomic it is no such program, and stores the same.
        let (plain, used) = parts(false).expect("the store alone lowers");
        let container = victory_shader::Container::parse(&plain.dxil).unwrap();
        let flags = container
            .part(victory_shader::FourCC::SFI0)
            .map_or(0, |p| u32::from_le_bytes(p.data[..4].try_into().unwrap()));
        assert_eq!(flags & 0x1040_0000, 0, "{flags:#x}");
        assert_eq!(used.len(), 1);
        let plain = user_data(&plain).expect("only the user data the probe sets");
        assert_eq!(plain.iter().filter(|&&d| d == STORED).count(), 1);
        let low = data.iter().filter(|&&d| d == KEY as u32).count();
        let high = data.iter().filter(|&&d| d == (KEY >> 32) as u32).count();
        let stored = data.iter().filter(|&&d| d == STORED).count();
        assert_eq!((low, high, stored), (1, 1, 1), "{data:x?}");
    }

    /// A host is believed only if the whole program ran; one that reports
    /// nothing is not asked; and a probe that could not run leaves the
    /// host's word standing.
    #[test]
    fn a_claimed_capability_counts_only_after_the_program_ran() {
        let whole = Ran {
            texel: KEY,
            stored: STORED,
        };
        assert!(whole.whole());
        assert_eq!(verdict(true, || Ok(whole)), Verdict::Works);
        assert!(Verdict::Works.usable());
        // What D3DMetal leaves: nothing written at all.
        let nothing = Ran {
            texel: 0,
            stored: 0,
        };
        assert_eq!(verdict(true, || Ok(nothing)), Verdict::Claimed(nothing));
        assert!(!Verdict::Claimed(nothing).usable());
        // Half a program is not the capability either.
        for half in [
            Ran {
                texel: KEY,
                stored: 0,
            },
            Ran {
                texel: 0,
                stored: STORED,
            },
            Ran {
                texel: KEY & 0xffff_ffff,
                stored: STORED,
            },
        ] {
            assert!(!half.whole(), "{half:?}");
            assert!(!verdict(true, || Ok(half)).usable(), "{half:?}");
        }
        let unchecked = verdict(true, || Err("no pipeline".to_owned()));
        assert_eq!(unchecked, Verdict::Unchecked("no pipeline".to_owned()));
        assert!(unchecked.usable());
        let unclaimed = verdict(false, || panic!("not run where nothing is claimed"));
        assert_eq!(unclaimed, Verdict::Unclaimed);
        assert!(!unclaimed.usable());
    }

    /// Whether the host device is Apple's D3DMetal: under Wine, the
    /// adapter that names itself "AMD Compatibility Mode" (as
    /// `victory-shader`'s tests recognise it).
    #[cfg(windows)]
    fn d3dmetal(host: &crate::host::Host) -> bool {
        crate::test_support::under_wine()
            && crate::host::adapter_name(&host.device) == "AMD Compatibility Mode"
    }

    /// On the device: the host's answer to "64-bit atomics on typed heap
    /// resources" is what the probe's program did there, not what the
    /// host reports. Where the capability is reported the probe runs, and
    /// its program either ran whole (the capability is real) or, under
    /// D3DMetal, left both the texel and the buffer as they were (the
    /// measured difference): the host is then taken not to have it.
    #[cfg(windows)]
    #[test]
    fn the_host_has_64_bit_atomics_only_where_the_probe_ran() {
        let Some((_gpu, host)) = crate::testing::gpu() else {
            return;
        };
        if !crate::host::claims_atomics64(&host.device) {
            assert!(!host.heap_atomics64());
            return;
        }
        let ran = atomics64(&host.device).expect("the probe runs where the host reports them");
        assert_eq!(host.heap_atomics64(), ran.whole(), "{ran:x?}");
        // The probe's own machinery works here: the program without the
        // atomic stores its dword, and leaves the texture alone.
        let plain = Ran {
            texel: 0,
            stored: STORED,
        };
        assert_eq!(run(&host.device, false), Ok(plain));
        // The same answer when asked again: nothing of the first run is
        // left in the second's resources.
        assert_eq!(atomics64(&host.device), Ok(ran));
        if !ran.whole() {
            let nothing = Ran {
                texel: 0,
                stored: 0,
            };
            assert!(
                d3dmetal(host),
                "only D3DMetal is known to do this: {ran:x?}"
            );
            assert_eq!(ran, nothing, "D3DMetal ran nothing of it when measured");
        }
    }

    /// The pipeline of a compute program with a 64-bit atomic on a typed
    /// heap resource is made only where such a program runs. Where the
    /// host reports the capability and runs nothing (D3DMetal) it is
    /// refused, so its dispatches are dropped and counted as those of any
    /// pipeline without a host one, instead of running as nothing.
    #[cfg(windows)]
    #[test]
    fn a_pipeline_with_64_bit_atomics_is_made_only_where_they_run() {
        use crate::pipeline::Shader;
        use crate::pipeline::host::HostPipeline;
        let Some((_gpu, host)) = crate::testing::gpu() else {
            return;
        };
        let (shader, used) = parts(true).expect("the probe lowers");
        let shader = std::sync::Arc::new(Shader::translated_as("compute", shader, used));
        let made = HostPipeline::new(&[shader], None).is_some();
        let runs = crate::host::claims_atomics64(&host.device)
            && atomics64(&host.device).is_ok_and(Ran::whole);
        assert_eq!(
            made, runs,
            "a pipeline was made: {made}; its atomics run: {runs}"
        );
    }
}
