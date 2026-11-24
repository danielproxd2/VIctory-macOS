//! Builders of synthetic console shader containers, for tests here and in
//! the crates that build on this one (feature `test-support`).
//!
//! Everything is assembled in code from our own reading of the container
//! ([`crate::native`]) and from public GFX10 encodings ([`crate::gfx10::asm`]):
//! nothing comes from a compiled shader. The builders are the inverse of
//! the parser, so they prove that the two agree with each other, not that a
//! title's container parses. Fields the parser does not read are zero.
//!
//! Layers, lowest first: [`xbch`], [`part`] and [`fssm`] (the blob framing),
//! [`native_blob`] (every field the parser reads), [`container`] (a whole
//! DXBC container that [`crate::emit::translate`] takes), and the named
//! programs ([`vs_fullscreen_triangle`], [`ps_constant`]...).

use crate::dxbc::{FourCC, u64_at};
use crate::gfx10::asm::{
    self, Buffer, ExpFlags, NULL, S_ENDPGM, fconst, imm, lit32, sgpr, target, vgpr,
};
use crate::native::{
    ABSENT, CS_RSRC2, CS_THREADS, ComputeIo, ENTRY_SIZE, FLAG_WAVE32, FSSM_HEADER, FSSM_VERSION,
    GRAPHICS_SLOT_COUNT, HwStage, KIND_NATIVE, MRTS, MS_CLIP_CNTL, MS_INFO, MeshIo, NativeProgram,
    PATCH_BYTES, PATCH_FULL, PATCH_SLOT_DWORDS, PS_DB_SHADER_CONTROL, PS_INPUT_ADDR,
    PS_INPUT_COUNT, PS_INPUTS, PS_PATCH_SIZE, PS_PATCHES, RECORD_COUNTS, RECORD_FLAGS,
    RECORD_LAYOUT, RECORD_LDS, RECORD_REGS, RECORD_SIZE, RECORD_USED, ResourceType, RootParam,
    RootType, SLOTS, StageIo, Usage, UsedResource, VS_INPUT_COUNT, VS_INPUTS, VS_OUTPUT_COUNT,
    VS_OUTPUTS, VsInput, VsOutput, XBCH_VERSION, XDAT_OFFSET,
};

/// Bytes of stage data after each record: enough for every stage's layout
/// (the pixel stage's eight export patches end last).
const STAGE_DATA: usize = PS_PATCHES + MRTS * PATCH_BYTES;

/// Where `stage`'s record offset sits in the FSSM payload.
pub fn slot(stage: HwStage) -> usize {
    SLOTS
        .iter()
        .find(|s| s.0 == stage)
        .expect("every stage has a slot")
        .1
}

/// Where a one-program blob's stage data starts.
pub fn stage_data_at(blob: &[u8], stage: HwStage) -> usize {
    let record = u64_at(blob, FSSM_HEADER + slot(stage)).expect("an FSSM blob");
    FSSM_HEADER + record as usize + RECORD_SIZE as usize
}

/// A program of `stage` whose stage data is all zero (no semantics, no
/// export patches): wave32 for pixels and wave64 otherwise, 28 VGPRs.
pub fn bare_program(stage: HwStage, code: &[u8]) -> NativeProgram {
    NativeProgram {
        stage,
        wave64: stage != HwStage::Ps,
        vgprs: 28,
        layout: Vec::new(),
        used: Vec::new(),
        io: match stage {
            HwStage::Vs => StageIo::Vs {
                inputs: Vec::new(),
                outputs: Vec::new(),
            },
            HwStage::Ps => StageIo::Ps {
                inputs: Vec::new(),
                input_addr: 0,
                db_shader_control: 0,
                patches: Vec::new(),
            },
            HwStage::Cs => StageIo::Cs(ComputeIo {
                rsrc2: 0,
                threads: [0; 3],
            }),
            HwStage::Ms => StageIo::Ms(MeshIo {
                outputs: Vec::new(),
                clip_cntl: 0,
                threads: 0,
                max_vertices: 0,
                max_primitives: 0,
                topology: 0,
                lds_bytes: 0,
            }),
            _ => StageIo::None,
        },
        code: code.to_vec(),
    }
}

/// FSSM blob with `programs` as (stage, code) pairs, each a [`bare_program`].
pub fn fssm(programs: &[(HwStage, &[u8])]) -> Vec<u8> {
    let programs: Vec<NativeProgram> = programs
        .iter()
        .map(|&(stage, code)| bare_program(stage, code))
        .collect();
    native_blob(&programs)
}

fn put(out: &mut [u8], at: usize, bytes: &[u8]) {
    out[at..at + bytes.len()].copy_from_slice(bytes);
}

fn put32(out: &mut [u8], at: usize, value: u32) {
    put(out, at, &value.to_le_bytes());
}

fn root_raw(root: RootType) -> u32 {
    match root {
        RootType::Constants => 0,
        RootType::Descriptor => 1,
        RootType::Table => 3,
        RootType::ImmediateSampler => 4,
        RootType::ResourceHeap => 5,
        RootType::SamplerHeap => 6,
        RootType::Other(n) => n,
    }
}

fn usage_raw(usage: Usage) -> u8 {
    match usage {
        Usage::Generic => 6,
        Usage::ClipDistance => 7,
        Usage::Other(n) => n,
    }
}

fn layout_entry(p: &RootParam) -> Vec<u8> {
    let mut e = Vec::with_capacity(ENTRY_SIZE as usize);
    for v in [
        root_raw(p.root),
        p.table_offset_dwords,
        p.sgprs,
        p.base_register,
    ] {
        e.extend(v.to_le_bytes());
    }
    e.extend([p.user_data, p.slot, 0, 0]);
    e
}

fn used_entry(u: &UsedResource) -> Vec<u8> {
    let kind = match u.kind {
        ResourceType::Srv => 0,
        ResourceType::Sampler => 1,
        ResourceType::Cbv => 2,
        ResourceType::Uav => 3,
        ResourceType::Other(n) => n,
    };
    let root = u8::try_from(root_raw(u.root)).expect("a used resource's root type is a byte");
    let mut e = vec![kind, root, 0, 0];
    for v in [u.table_offset_dwords, u.lower_bound, u.stride] {
        e.extend(v.to_le_bytes());
    }
    e.extend([u.dwords, u.user_data, u.slot, 0]);
    e
}

fn vs_outputs(data: &mut [u8], outputs: &[VsOutput]) {
    for (i, o) in outputs.iter().enumerate() {
        let flags = u8::from(o.fp16) | u8::from(o.pack2) << 1;
        let entry = [usage_raw(o.usage), o.row, o.param, o.row, 0, flags];
        put(data, VS_OUTPUTS + 6 * i, &entry);
    }
    put32(data, VS_OUTPUT_COUNT, outputs.len() as u32);
}

/// The stage data of `p`: what the parser reads back as `p.io`.
fn stage_data(p: &NativeProgram) -> Vec<u8> {
    let mut data = vec![0; STAGE_DATA];
    // Absent export patches, whatever the stage (as the pixel layout has them).
    for mrt in 0..MRTS {
        put32(&mut data, PS_PATCHES + mrt * PATCH_BYTES, u32::MAX);
    }
    match &p.io {
        StageIo::None => {}
        StageIo::Vs { inputs, outputs } => {
            for (i, v) in inputs.iter().enumerate() {
                let VsInput {
                    usage,
                    row,
                    mask,
                    vgpr,
                } = *v;
                let highest = 7 - (mask & 0xF | 1).leading_zeros() as u8;
                let entry = [usage_raw(usage), row, mask, vgpr, highest];
                put(&mut data, VS_INPUTS + 5 * i, &entry);
            }
            put32(&mut data, VS_INPUT_COUNT, inputs.len() as u32);
            vs_outputs(&mut data, outputs);
        }
        StageIo::Ps {
            inputs,
            input_addr,
            db_shader_control,
            patches,
        } => {
            for (i, v) in inputs.iter().enumerate() {
                let flags = u8::from(v.flat) | u8::from(v.fp16) << 3 | u8::from(v.pack2) << 4;
                let entry = [
                    usage_raw(v.usage),
                    v.row,
                    v.mask,
                    v.attr,
                    0,
                    v.row2,
                    0,
                    flags,
                ];
                put(&mut data, PS_INPUTS + 8 * i, &entry);
            }
            put32(&mut data, PS_INPUT_COUNT, inputs.len() as u32);
            put32(&mut data, PS_INPUT_ADDR, *input_addr);
            put32(&mut data, PS_DB_SHADER_CONTROL, *db_shader_control);
            let size = patches.first().map_or(0, |p| p.code.len());
            assert!(size <= PATCH_SLOT_DWORDS, "an export patch has 10 dwords");
            put32(&mut data, PS_PATCH_SIZE, size as u32);
            for patch in patches {
                assert_eq!(patch.code.len(), size, "export patches have one size");
                let at = PS_PATCHES + usize::from(patch.mrt) * PATCH_BYTES;
                put32(&mut data, at, patch.offset);
                let full = at + 4 + PATCH_FULL * PATCH_SLOT_DWORDS * 4;
                put(&mut data, full, &asm::bytes(&patch.code));
            }
        }
        StageIo::Cs(cs) => {
            put32(&mut data, CS_RSRC2, cs.rsrc2);
            for (k, n) in cs.threads.into_iter().enumerate() {
                put32(&mut data, CS_THREADS + 4 * k, n);
            }
        }
        StageIo::Ms(ms) => {
            vs_outputs(&mut data, &ms.outputs);
            put32(&mut data, MS_CLIP_CNTL, ms.clip_cntl);
            let half = |n: u32| u16::try_from(n).expect("a 16-bit mesh limit").to_le_bytes();
            put(&mut data, MS_INFO, &half(ms.threads));
            put(&mut data, MS_INFO + 2, &half(ms.max_vertices));
            put(&mut data, MS_INFO + 4, &half(ms.max_primitives));
            data[MS_INFO + 7] = ms.topology;
        }
    }
    data
}

/// The unpacked native blob (`FSSM`) holding `programs`, one per stage:
/// per program a record, its stage data, the code, then the root layout
/// and used-resource tables.
pub fn native_blob(programs: &[NativeProgram]) -> Vec<u8> {
    let mut payload = GRAPHICS_SLOT_COUNT.to_le_bytes().to_vec();
    payload.resize(slot(HwStage::Cs) + 8, 0);
    for (_, at) in SLOTS {
        put(&mut payload, at, &ABSENT.to_le_bytes());
    }
    for p in programs {
        let record = payload.len();
        let slot = slot(p.stage);
        assert_eq!(
            payload[slot..slot + 8],
            ABSENT.to_le_bytes(),
            "one program per stage"
        );
        put(&mut payload, slot, &(record as u64).to_le_bytes());
        let code_at = record + RECORD_SIZE as usize + STAGE_DATA;
        let layout_at = code_at + p.code.len();
        let used_at = layout_at + ENTRY_SIZE as usize * p.layout.len();
        let mut rec = vec![0u8; RECORD_SIZE as usize];
        put(&mut rec, 0, &(code_at as u64).to_le_bytes());
        put(&mut rec, 8, &(p.code.len() as u64).to_le_bytes());
        put32(
            &mut rec,
            RECORD_FLAGS,
            if p.wave64 { 0 } else { FLAG_WAVE32 },
        );
        if !p.layout.is_empty() {
            put(&mut rec, RECORD_LAYOUT, &(layout_at as u64).to_le_bytes());
        }
        if !p.used.is_empty() {
            put(&mut rec, RECORD_USED, &(used_at as u64).to_le_bytes());
        }
        put32(&mut rec, RECORD_COUNTS, p.layout.len() as u32);
        put32(&mut rec, RECORD_COUNTS + 4, p.used.len() as u32);
        if let StageIo::Ms(ms) = &p.io {
            put32(&mut rec, RECORD_LDS, ms.lds_bytes);
        }
        assert!(p.vgprs <= 256, "at most 256 VGPRs");
        put32(&mut rec, RECORD_REGS, p.vgprs.into());
        payload.extend(rec);
        payload.extend(stage_data(p));
        payload.extend(&p.code);
        payload.extend(p.layout.iter().flat_map(layout_entry));
        payload.extend(p.used.iter().flat_map(used_entry));
    }
    let mut out = b"FSSM".to_vec();
    out.extend(FSSM_VERSION.to_le_bytes());
    out.extend((payload.len() as u64).to_le_bytes());
    out.extend(payload);
    out
}

/// `unpacked` as a packed native blob (`XBCH` around a zlib stream).
pub fn xbch(unpacked: &[u8]) -> Vec<u8> {
    let stream = miniz_oxide::deflate::compress_to_vec_zlib(unpacked, 6);
    let mut out = b"XBCH".to_vec();
    out.extend(XBCH_VERSION.to_le_bytes());
    out.extend((stream.len() as u64).to_le_bytes());
    out.extend((unpacked.len() as u64).to_le_bytes());
    out.extend(stream);
    out
}

/// `DXIL` part data: a zeroed program header, the XDAT index of `blobs` as
/// (kind, bytes), then the blobs.
pub fn part(blobs: &[(u32, &[u8])]) -> Vec<u8> {
    let mut out = vec![0u8; XDAT_OFFSET];
    out.extend(b"XDAT");
    out.extend((blobs.len() as u32).to_le_bytes());
    out.extend(0x24u32.to_le_bytes());
    let mut offset = out.len() + 12 * blobs.len();
    for (kind, blob) in blobs {
        for v in [*kind, offset as u32, blob.len() as u32] {
            out.extend(v.to_le_bytes());
        }
        offset += blob.len();
    }
    for (_, blob) in blobs {
        out.extend(*blob);
    }
    out
}

/// One row of a signature part (`ISG1`, `OSG1`, `PSG1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Element {
    pub name: &'static str,
    pub index: u32,
    /// `D3D_NAME`: 0 none, 1 position, 64 target, 65 depth...
    pub system_value: u32,
    /// `D3D_REGISTER_COMPONENT_TYPE`: 1 uint, 2 int, 3 float.
    pub component: u32,
    /// The row.
    pub register: u32,
    /// Channels of the register (bit 0 x).
    pub mask: u8,
}

impl Element {
    /// A float element in channels `mask` of `register`.
    pub const fn float(name: &'static str, system_value: u32, register: u32, mask: u8) -> Self {
        Element {
            name,
            index: 0,
            system_value,
            component: 3,
            register,
            mask,
        }
    }
}

/// `float4 SV_Position`, row 0.
pub const SV_POSITION: Element = Element::float("SV_Position", 1, 0, 0xF);
/// `float4 SV_Target0`, row 0.
pub const SV_TARGET: Element = Element::float("SV_Target", 64, 0, 0xF);

/// A signature part: a count, the offset of the rows (8), 32-byte rows,
/// then the names.
pub fn signature_part(elements: &[Element]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend((elements.len() as u32).to_le_bytes());
    out.extend(8u32.to_le_bytes());
    let mut names = Vec::new();
    for e in elements {
        let name_at = 8 + 32 * elements.len() + names.len();
        for v in [
            0, // stream
            name_at as u32,
            e.index,
            e.system_value,
            e.component,
            e.register,
        ] {
            out.extend(v.to_le_bytes());
        }
        // Mask, never-writes/always-reads mask, padding; minimum precision.
        out.extend([e.mask, 0, 0, 0]);
        out.extend(0u32.to_le_bytes());
        names.extend(e.name.bytes());
        names.push(0);
    }
    out.extend(names);
    out
}

/// A console shader container holding `programs` (one per stage) in its
/// `DXIL` part, with input and output signature parts when not empty.
pub fn container_of(programs: &[NativeProgram], isg1: &[Element], osg1: &[Element]) -> Vec<u8> {
    let dxil = part(&[(KIND_NATIVE, &xbch(&native_blob(programs)))]);
    let (isg1, osg1) = (signature_part(isg1), signature_part(osg1));
    let mut parts: Vec<(FourCC, &[u8])> = Vec::new();
    if isg1.len() > 8 {
        parts.push((FourCC::ISG1, &isg1));
    }
    if osg1.len() > 8 {
        parts.push((FourCC::OSG1, &osg1));
    }
    parts.push((FourCC::DXIL, &dxil));
    crate::dxbc::build(&parts)
}

/// A console shader container holding `program`.
pub fn container(program: &NativeProgram, isg1: &[Element], osg1: &[Element]) -> Vec<u8> {
    container_of(std::slice::from_ref(program), isg1, osg1)
}

/// A console shader container whose one program is `words` tagged as
/// `stage`, with zeroed stage data ([`bare_program`]) but wave32 and 32
/// VGPRs. Enough for whatever only looks at the stage (hull, domain and
/// geometry programs, which the translator refuses) and for vertex and
/// pixel programs without attributes, parameters or root arguments.
pub fn console_container(
    stage: HwStage,
    words: &[u32],
    isg1: &[Element],
    osg1: &[Element],
) -> Vec<u8> {
    let program = NativeProgram {
        wave64: false,
        vgprs: 32,
        ..bare_program(stage, &asm::bytes(words))
    };
    container(&program, isg1, osg1)
}

/// A wave32 compute program: `words` with thread-group size `threads` and
/// `COMPUTE_PGM_RSRC2` `rsrc2` (bits 1-5: user SGPRs), no root parameters.
pub fn compute_program(words: &[u32], threads: [u32; 3], rsrc2: u32) -> NativeProgram {
    NativeProgram {
        wave64: false,
        vgprs: 32,
        io: StageIo::Cs(ComputeIo { rsrc2, threads }),
        ..bare_program(HwStage::Cs, &asm::bytes(words))
    }
}

/// Root parameter 0: four dwords of root constants in s0-s3.
const ROOT_CONSTANTS: RootParam = RootParam {
    root: RootType::Constants,
    table_offset_dwords: 0,
    sgprs: 4,
    base_register: 0,
    user_data: 0,
    slot: 0,
};

/// A vertex program without inputs that covers the screen with vertices 0,
/// 1 and 2: clip-space positions (-1, -1), (-1, 3) and (3, -1) at z = 0,
/// w = 1, which is clockwise with y up. The position is computed from the
/// vertex id (v0): x = (id & 2) * 2 - 1, y = (id & 1) * 4 - 1.
///
/// Output signature: `SV_Position` (row 0). No parameters are exported.
pub fn vs_fullscreen_triangle() -> Vec<u8> {
    let none = asm::Vop3Mods::default();
    let pos = ExpFlags {
        done: true,
        ..ExpFlags::default()
    };
    let words = [
        asm::vop2("v_and_b32", 1, imm(2), 0),
        asm::vop2("v_and_b32", 2, imm(1), 0),
        asm::vop1("v_cvt_f32_u32", 1, vgpr(1)),
        asm::vop1("v_cvt_f32_u32", 2, vgpr(2)),
        asm::vop3("v_fma_f32", 1, &[vgpr(1), fconst(2.0), fconst(-1.0)], none),
        asm::vop3("v_fma_f32", 2, &[vgpr(2), fconst(4.0), fconst(-1.0)], none),
        asm::vop1("v_mov_b32", 3, imm(0)),
        asm::vop1("v_mov_b32", 4, fconst(1.0)),
        asm::exp(target::POS0, 0xF, [1, 2, 3, 4], pos),
        vec![S_ENDPGM],
    ]
    .concat();
    console_container(HwStage::Vs, &words, &[], &[SV_POSITION])
}

/// The VGPR the fetch shader loads [`vs_position_buffer`]'s attribute into
/// (channel k in this VGPR + k): the first above the system inputs (vertex
/// id v0, primitive id v2, instance id v3).
pub const POSITION_VGPR: u8 = 4;

/// A vertex program that passes one `float4` attribute through as the
/// position.
///
/// Input signature: `POSITION` (row 0, four float channels), which the
/// stage data says the fetch shader loads into v4-v7. Output signature:
/// `SV_Position` (row 0).
pub fn vs_position_buffer() -> Vec<u8> {
    let v = POSITION_VGPR;
    let pos = ExpFlags {
        done: true,
        ..ExpFlags::default()
    };
    let mut words = asm::exp(target::POS0, 0xF, [v, v + 1, v + 2, v + 3], pos);
    words.push(S_ENDPGM);
    let program = NativeProgram {
        wave64: false,
        vgprs: 8,
        io: StageIo::Vs {
            inputs: vec![VsInput {
                usage: Usage::Generic,
                row: 0,
                mask: 0xF,
                vgpr: v,
            }],
            outputs: Vec::new(),
        },
        ..bare_program(HwStage::Vs, &asm::bytes(&words))
    };
    container(
        &program,
        &[Element::float("POSITION", 0, 0, 0xF)],
        &[SV_POSITION],
    )
}

/// `exp mrt0 v0, v1, v2, v3 done vm; s_endpgm`: the colour export that ends
/// a pixel program. Without an export patch in the stage data the code is
/// used as it is, and the export carries plain 32-bit values.
fn export_color() -> Vec<u32> {
    let flags = ExpFlags {
        done: true,
        vm: true,
        ..ExpFlags::default()
    };
    let mut words = asm::exp(target::MRT0, 0xF, [0, 1, 2, 3], flags);
    words.push(S_ENDPGM);
    words
}

/// A pixel program that writes the constant colour `rgba` to render
/// target 0.
///
/// Output signature: `SV_Target0` (row 0, four float channels). No inputs.
pub fn ps_constant(rgba: [f32; 4]) -> Vec<u8> {
    let mut words = Vec::new();
    for (i, c) in rgba.into_iter().enumerate() {
        words.extend(asm::vop1("v_mov_b32", i as u8, lit32(c.to_bits())));
    }
    words.extend(export_color());
    console_container(HwStage::Ps, &words, &[], &[SV_TARGET])
}

/// A pixel program that writes the four dwords of root parameter 0 (root
/// constants, in s0-s3) to render target 0 as the bits of four floats.
///
/// Output signature: `SV_Target0` (row 0, four float channels). No inputs.
pub fn ps_root_constants() -> Vec<u8> {
    let mut words = Vec::new();
    for i in 0..4 {
        words.extend(asm::vop1("v_mov_b32", i, sgpr(i)));
    }
    words.extend(export_color());
    let program = NativeProgram {
        wave64: false,
        vgprs: 4,
        layout: vec![ROOT_CONSTANTS],
        ..bare_program(HwStage::Ps, &asm::bytes(&words))
    };
    container(&program, &[], &[SV_TARGET])
}

/// A one-thread compute program around `body`, with the root signature the
/// compute programs here share: parameter 0 is four dwords of root
/// constants (s0-s3), parameter 1 a descriptor table (s4) whose first
/// descriptor is a raw buffer UAV. The prologue loads that descriptor into
/// s[8:11], the way [`crate::resource`] traces a table descriptor: the
/// table's 32-bit address with a zero high half, read with
/// `s_load_dwordx4`. `body` may use s6, s7 and s12 up, and every VGPR.
pub fn compute_with_uav(body: &[u32]) -> NativeProgram {
    let mut words = asm::sop1("s_mov_b32", 5, imm(0));
    words.extend(asm::smem("s_load_dwordx4", 8, 4, 0, NULL));
    words.extend(body);
    words.push(S_ENDPGM);
    NativeProgram {
        layout: vec![
            ROOT_CONSTANTS,
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        }],
        // Five user SGPRs: s0-s4.
        ..compute_program(&words, [1, 1, 1], 5 << 1)
    }
}

/// `buffer_store_dword v<vdata>, off, s[8:11], 0 offset:<4 * dword>`: a
/// store to the UAV of [`compute_with_uav`].
pub fn store_dword(vdata: u8, dword: u16) -> Vec<u32> {
    asm::mubuf(
        "buffer_store_dword",
        Buffer {
            vdata,
            srsrc: 8,
            offset: 4 * dword,
            ..Buffer::default()
        },
    )
}

/// A compute program (one thread) that stores dword 0 of root parameter 0
/// (root constants) to dword 0 of the raw buffer UAV that is the first
/// descriptor of the table at root parameter 1.
pub fn cs_store_root_constant() -> Vec<u8> {
    let mut body = asm::vop1("v_mov_b32", 0, sgpr(0));
    body.extend(store_dword(0, 0));
    container(&compute_with_uav(&body), &[], &[])
}

/// A compute program (one thread) that stores `words` to dwords 0.. of the
/// raw buffer UAV that is the first descriptor of the table at root
/// parameter 1 (parameter 0 is four unused dwords of root constants, as in
/// [`cs_store_root_constant`]). At most 1024 words (the store's offset
/// field has 12 bits).
pub fn cs_store_words(words: &[u32]) -> Vec<u8> {
    assert!(words.len() <= 1024, "at most 1024 words");
    let mut body = Vec::new();
    for (i, &w) in words.iter().enumerate() {
        body.extend(asm::vop1("v_mov_b32", 0, lit32(w)));
        body.extend(store_dword(0, i as u16));
    }
    container(&compute_with_uav(&body), &[], &[])
}

/// A deterministic pseudo-random generator (xorshift64) for seeded tests.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// A generator for `seed` (0 is replaced: the state must not be zero).
    pub fn new(seed: u64) -> Self {
        Rng(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    /// A value below `bound` (which is not 0); slightly biased, which
    /// seeded tests do not mind.
    pub fn below(&mut self, bound: u32) -> u32 {
        assert!(bound > 0, "an empty range");
        self.next_u32() % bound
    }

    /// Fills `out` with pseudo-random bytes.
    pub fn fill(&mut self, out: &mut [u8]) {
        for chunk in out.chunks_mut(8) {
            let bytes = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dxbc::Container;
    use crate::native::{ExportPatch, PsInput, native_programs};

    fn programs_of(container: &[u8]) -> Vec<NativeProgram> {
        let c = Container::parse(container).unwrap();
        native_programs(c.part(FourCC::DXIL).unwrap().data).unwrap()
    }

    /// Every field the parser reads comes back as the builder was given
    /// it, for each stage's own stage data and with several programs in
    /// one blob.
    #[test]
    fn the_parser_reads_back_what_the_builder_wrote() {
        let output = |row, param, fp16, pack2| VsOutput {
            usage: Usage::Generic,
            row,
            param,
            fp16,
            pack2,
        };
        let layout = vec![
            ROOT_CONSTANTS,
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 2,
                sgprs: 1,
                base_register: 5,
                user_data: 9,
                slot: 1,
            },
            RootParam {
                root: RootType::Descriptor,
                table_offset_dwords: 0,
                sgprs: 2,
                base_register: 1,
                user_data: 10,
                slot: 2,
            },
        ];
        let used = vec![
            UsedResource {
                kind: ResourceType::Srv,
                root: RootType::Table,
                table_offset_dwords: 2,
                lower_bound: 5,
                stride: 4,
                dwords: 8,
                user_data: 9,
                slot: 1,
            },
            UsedResource {
                kind: ResourceType::Cbv,
                root: RootType::Descriptor,
                table_offset_dwords: 0,
                lower_bound: 1,
                stride: 0,
                dwords: 4,
                user_data: 10,
                slot: 2,
            },
        ];
        let vs = NativeProgram {
            stage: HwStage::Vs,
            wave64: true,
            vgprs: 12,
            layout: layout.clone(),
            used: used.clone(),
            io: StageIo::Vs {
                inputs: vec![
                    VsInput {
                        usage: Usage::Generic,
                        row: 0,
                        mask: 0xF,
                        vgpr: 4,
                    },
                    VsInput {
                        usage: Usage::Other(3),
                        row: 1,
                        mask: 0x3,
                        vgpr: 8,
                    },
                ],
                outputs: vec![output(1, 0, false, false), output(2, 1, true, true)],
            },
            code: asm::bytes(&[0xBF80_0000, S_ENDPGM]),
        };
        let ps = NativeProgram {
            stage: HwStage::Ps,
            wave64: false,
            vgprs: 6,
            layout: vec![ROOT_CONSTANTS],
            used: Vec::new(),
            io: StageIo::Ps {
                inputs: vec![PsInput {
                    usage: Usage::ClipDistance,
                    row: 1,
                    mask: 0x3,
                    attr: 2,
                    flat: true,
                    fp16: true,
                    pack2: true,
                    row2: 3,
                }],
                input_addr: 0x302,
                db_shader_control: 0x10,
                patches: vec![ExportPatch {
                    mrt: 1,
                    offset: 1,
                    code: asm::exp(target::MRT0 + 1, 0xF, [0, 1, 2, 3], ExpFlags::default()),
                }],
            },
            code: asm::bytes(&[0xBF80_0000, 0xBF80_0000, 0xBF80_0000, S_ENDPGM]),
        };
        let cs = NativeProgram {
            used,
            layout,
            ..compute_program(&[S_ENDPGM], [8, 4, 2], 0x1098A)
        };
        let ms = NativeProgram {
            stage: HwStage::Ms,
            wave64: true,
            vgprs: 256,
            layout: Vec::new(),
            used: Vec::new(),
            io: StageIo::Ms(MeshIo {
                outputs: vec![output(1, 0, false, false), output(0, 0x82, true, false)],
                clip_cntl: 0x0040_000F,
                threads: 128,
                max_vertices: 96,
                max_primitives: 64,
                topology: 2,
                lds_bytes: 16,
            }),
            code: asm::bytes(&[S_ENDPGM]),
        };
        for p in [&vs, &ps, &cs, &ms] {
            assert_eq!(
                programs_of(&container(p, &[], &[])),
                std::slice::from_ref(p)
            );
        }
        // Several in one blob come back in slot order, whatever order they
        // were given in.
        let all = [cs.clone(), ps.clone(), ms.clone(), vs.clone()];
        assert_eq!(programs_of(&container_of(&all, &[], &[])), [vs, ps, ms, cs]);
        // Stages without stage data keep the record alone.
        for stage in [HwStage::Ls, HwStage::Hs, HwStage::Es, HwStage::Gs] {
            let found = programs_of(&console_container(stage, &[S_ENDPGM], &[], &[]));
            assert_eq!(found.len(), 1);
            assert_eq!((found[0].stage, &found[0].io), (stage, &StageIo::None));
        }
    }

    /// The signature parts are written here without the emitter's writer,
    /// so the two readings of the format are checked against each other:
    /// the emitter's writer produces the same rows for the same elements.
    #[test]
    fn signature_parts_carry_their_rows() {
        let elements = [
            Element::float("POSITION", 0, 0, 0xF),
            Element {
                name: "TEXCOORD",
                index: 1,
                system_value: 0,
                component: 2,
                register: 1,
                mask: 0b0110,
            },
        ];
        let part = signature_part(&elements);
        let u32_at = |at: usize| u32::from_le_bytes(part[at..at + 4].try_into().unwrap());
        assert_eq!((u32_at(0), u32_at(4)), (2, 8));
        for (i, e) in elements.iter().enumerate() {
            let row = 8 + 32 * i;
            let name = &part[u32_at(row + 4) as usize..];
            assert_eq!(
                &name[..e.name.len() + 1],
                format!("{}\0", e.name).as_bytes()
            );
            assert_eq!(
                [
                    u32_at(row),
                    u32_at(row + 8),
                    u32_at(row + 12),
                    u32_at(row + 16),
                    u32_at(row + 20)
                ],
                [0, e.index, e.system_value, e.component, e.register]
            );
            assert_eq!(part[row + 24], e.mask);
        }
        // An empty signature is left out of the container.
        let c = container(&bare_program(HwStage::Vs, &[]), &[], &[SV_POSITION]);
        let c = Container::parse(&c).unwrap();
        assert!(c.part(FourCC::ISG1).is_none());
        assert_eq!(
            c.part(FourCC::OSG1).unwrap().data,
            signature_part(&[SV_POSITION])
        );
    }

    /// The generator is a fixed sequence per seed: the first values of
    /// xorshift64 (13, 7, 17) from state 1, by hand.
    #[test]
    fn the_rng_is_a_fixed_sequence_per_seed() {
        let mut r = Rng::new(1);
        // 1 ^ 1 << 13 = 0x2001; ^ >> 7 = 0x2041; ^ << 17 = 0x4082_2041.
        assert_eq!(r.next_u64(), 0x4082_2041);
        let (mut a, mut b) = (Rng::new(7), Rng::new(7));
        let mut bytes = [[0u8; 13]; 2];
        a.fill(&mut bytes[0]);
        b.fill(&mut bytes[1]);
        assert_eq!(bytes[0], bytes[1]);
        assert_ne!(bytes[0], [0; 13]);
        assert_ne!(Rng::new(8).next_u64(), Rng::new(7).next_u64());
        // A zero seed still moves.
        let mut z = Rng::new(0);
        assert_ne!(z.next_u64(), z.next_u64());
        assert!((0..100).all(|_| z.below(10) < 10));
    }
}
