//! DXIL emission (decision 0003, step 3): a console vertex, pixel, compute
//! or mesh shader as a PC DXIL container (SM 6.6) that host D3D12 accepts.
//! Compute shaders keep the console's thread-group size; one that depends
//! on the lane layout (cross-lane operations) declares the console's wave
//! size (`WaveSize`), so the host device must support it.
//!
//! What the host binds for a translated shader ([`HostShader`]):
//!
//! * **User data** at `b0, space 0` (32-bit root constants or a constant
//!   buffer, per stage): dword `index` holds a root parameter's value, as
//!   [`UserData`] lists. A root-constant dword is the constant. A
//!   descriptor table is the byte offset of its first descriptor in its
//!   heap: the descriptor index times 32 for CBV/SRV/UAV tables, times 16
//!   for sampler tables (the console's descriptor sizes; the code adds
//!   offsets in those units). The heap bases of dynamic resources are 0
//!   and not listed.
//! * **Descriptors** through SM 6.6 `ResourceDescriptorHeap[]` and
//!   `SamplerDescriptorHeap[]`, at the index the console address gives, so
//!   the host heaps keep the title's descriptor order. The root signature
//!   needs `CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED` and
//!   `SAMPLER_HEAP_DIRECTLY_INDEXED`.
//! * **Immediate samplers** as static samplers `s<n>, space 0`, `n` the
//!   position in [`HostShader::samplers`] (the console sampler words) and
//!   [`HostShader::static_samplers`] (the same decoded into a D3D12 static
//!   sampler description). Comparison sampling (added 2026-11-23): an
//!   immediate sampler it uses has a comparison filter there (bit 0x80);
//!   a heap sampler it uses must be a comparison sampler on the host.
//! * **Root CBVs and root SRVs** (raw buffers) as root descriptors
//!   `b<i>` / `t<i>` in [`ROOT_CBV_SPACE`], `i` the title's root parameter
//!   index ([`HostShader::root_cbvs`], [`HostShader::root_srvs`]); the
//!   address the title sets is the buffer's.
//! * **Vertex fetch** (added 2026-11-23). Grand Theft Auto VI's mesh and
//!   vertex programs fetch vertices themselves with typed-format loads
//!   (`tbuffer_load_format_*`): the format (8_8_8_8 unorm, 16_16 snorm,
//!   2_10_10_10 unorm, 8_8_8_8 and 16_16_16_16 uint...) is in the
//!   instruction, so the translation reads raw bytes and converts them
//!   itself. Their descriptors are built around a root SRV (stride 1, no
//!   record limit): the host binds the root SRV `t<slot>` above with the
//!   address the title sets, nothing more. The vertex index, the stride
//!   and the attribute offset are the shader's own arithmetic on data in
//!   that buffer (per-stream headers), not descriptor fields.
//! * **Buffer views in the heap** are declared by how the shader reads
//!   them: raw (ByteAddressBuffer) for dword loads and stores, atomics and
//!   typed-format loads without an index; structured with the used-resource
//!   entry's stride when indexed; typed with four 32-bit float channels for
//!   `buffer_load_format_*` and `buffer_store_format_*`. The host's view at
//!   that index must be of that kind and start at the console descriptor's
//!   base address (raw: `R32_TYPELESS` with the raw flag), whatever kind the
//!   title's view description says. Typed-format loads from heap
//!   descriptors (1,024 world compute programs, ray-tracing material
//!   resolves) read raw views this way.
//!
//! * **Texture atomics** (added 2026-11-23): a texture UAV the shader uses
//!   atomically is declared a 32-bit unsigned single-channel view; the
//!   host's view at that index must be an R32 integer view (64-bit atomics
//!   stop the translation). Global loads from a root SRV read it raw.
//!
//! Entry points: [`translate`] (container bytes as the title passes them
//! at pipeline creation → [`HostShader`]), [`emit`] (the same with the
//! earlier stages already run). The root constants of each stage must
//! cover [`HostShader::user_data_dwords`]. Stability: these types and the
//! binding scheme are what pipeline creation builds on; changes are
//! announced.
//! * **Signatures** as the console container has them (same semantics,
//!   registers and masks), so the input layout and stage linking are the
//!   title's; pixel outputs are plain 32-bit values.
//! * **Mesh shaders** ([`HwStage::Ms`], `ms_6_6`): user data and
//!   descriptors as above (the root constants visible to the mesh stage);
//!   the pipeline is a mesh pipeline (pipeline-state stream, no input
//!   layout) with the title's pixel shader. The thread group is the
//!   console's in lanes (`[numthreads]` in the DXIL), the vertex and
//!   primitive maxima its own, triangles only, no wave size required. The
//!   console numbers groups in x only: the draw path passes the title's
//!   count as `DispatchMesh(x, 1, 1)`. Per-primitive outputs (`PSG1`) are
//!   the console's, without `SV_PrimitiveID` and `SV_CullPrimitive` (the
//!   console never exports them).

mod bitcode;
mod container;
#[cfg(all(test, windows))]
mod diff_tests;
#[cfg(all(windows, any(test, feature = "host")))]
pub mod host;
mod lower;
mod sampler;
mod shader;
#[cfg(windows)]
pub mod validator;
#[cfg(all(test, windows))]
mod valu_tests;
#[cfg(all(test, windows))]
mod waterfall_tests;
mod wave;

use crate::dxbc::{Container, FourCC};
use crate::native::{HwStage, NativeProgram, RootType};
use crate::{ShaderError, io, ir, resource, structure};

pub use sampler::StaticSampler;
pub use shader::{ROOT_CBV_SPACE, SAMPLER_SPACE, USER_DATA_SPACE};

/// Whether a `VICTORY_REQUIRE_*` switch with the value `var` is on: only
/// `1` is. The tests read these switches (`crate::testing`): on, a test
/// that would be skipped for want of a device, of `dxil.dll` or of a
/// compiler fails instead.
#[cfg(test)]
pub(crate) fn require(var: Option<&str>) -> bool {
    var.is_some_and(|v| v.trim() == "1")
}

/// The loop limit without an override: 2^20.
pub const DEFAULT_LOOP_LIMIT: u32 = lower::LOOP_LIMIT;

/// The iteration count after which a translated loop is left, per lane and
/// per entry into the loop: [`DEFAULT_LOOP_LIMIT`].
///
/// With the `diagnostics` feature, `VICTORY_SHADER_LOOP_LIMIT` (decimal or
/// `0x` hex, at least 1) sets it instead. A loop that hits the limit in
/// every lane of a full-screen pass still runs for seconds (2^20 times a
/// 4K screen is about 10^13 iterations); a much lower limit shows whether
/// such loops are what makes a frame slow. The value is part of the
/// translation, so the shader cache must key on it (`victory-gpu`'s does).
pub fn loop_limit() -> u32 {
    static LIMIT: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| {
        if cfg!(feature = "diagnostics") {
            parse_loop_limit(std::env::var("VICTORY_SHADER_LOOP_LIMIT").ok().as_deref())
        } else {
            lower::LOOP_LIMIT
        }
    })
}

/// `VICTORY_SHADER_LOOP_LIMIT`'s value; the default when it is unset,
/// unreadable or 0. Pure, for testing.
fn parse_loop_limit(value: Option<&str>) -> u32 {
    let parsed = value
        .map(str::trim)
        .and_then(|v| match v.strip_prefix("0x") {
            Some(hex) => u32::from_str_radix(hex, 16).ok(),
            None => v.parse().ok(),
        });
    match parsed {
        Some(n) if n > 0 => n,
        _ => lower::LOOP_LIMIT,
    }
}

/// A translated shader and what the host binds for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostShader {
    pub stage: HwStage,
    /// The DXIL container, hashed as D3D12 expects.
    pub dxil: Vec<u8>,
    /// The user-data dwords the shader reads.
    pub user_data: Vec<UserData>,
    /// Immediate samplers: the four sampler words of `s<n>`.
    pub samplers: Vec<[u32; 4]>,
    /// The same samplers decoded, `s<n>` at position n: the static sampler
    /// descriptions the host's root signature declares.
    pub static_samplers: Vec<StaticSampler>,
    /// Root CBVs the shader reads: root parameter indices, each bound as
    /// `b<index>` in [`ROOT_CBV_SPACE`].
    pub root_cbvs: Vec<u8>,
    /// Root SRVs (raw buffers) the shader reads: `t<index>` in
    /// [`ROOT_CBV_SPACE`].
    pub root_srvs: Vec<u8>,
}

impl HostShader {
    /// Size of the user-data buffer the shader declares, in dwords (a
    /// multiple of 4; 0 when it reads none). Root constants must cover it.
    pub fn user_data_dwords(&self) -> u32 {
        let n = self
            .user_data
            .iter()
            .map(|u| u.index + 1)
            .max()
            .unwrap_or(0);
        n.next_multiple_of(4)
    }
}

/// One dword of user data and the root parameter that fills it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserData {
    /// Dword in the user-data buffer (the console's user-data index).
    pub index: u32,
    /// Root parameter index in the title's root signature.
    pub slot: u8,
    /// `Constants` or `Table` (see the module notes for the value).
    pub root: RootType,
    /// Dword within the parameter.
    pub dword: u32,
}

/// Translates the vertex, pixel, compute or mesh program of a console
/// shader container.
pub fn translate(container: &[u8]) -> Result<HostShader, ShaderError> {
    let c = Container::parse(container)?;
    let dxil = c
        .part(FourCC::DXIL)
        .ok_or(ShaderError::Malformed("no DXIL part"))?;
    let native = crate::native_programs(dxil.data)?
        .into_iter()
        .find(|p| {
            matches!(
                p.stage,
                HwStage::Vs | HwStage::Ps | HwStage::Cs | HwStage::Ms
            )
        })
        .ok_or(ShaderError::Unsupported {
            offset: 0,
            what: "a container without a vertex, pixel, compute or mesh program",
        })?;
    let p = ir::build(&native.patched_code()?, native.wave64, native.vgprs)?;
    // Structurisation first: tracing per-lane descriptors needs it (its
    // failure is reported after the earlier stages').
    let st = structure::structure(&p);
    let res = resource::track(&p, &native.layout, native.stage, st.as_ref().ok())?;
    let io = io::raise(&p, &native)?;
    let st = st?;
    emit(&c, &native, &p, &st, &io, &res)
}

/// Emits a program whose earlier stages already ran.
pub fn emit(
    c: &Container,
    native: &NativeProgram,
    p: &ir::Program,
    st: &structure::Structured,
    io: &io::Io,
    res: &resource::Resources,
) -> Result<HostShader, ShaderError> {
    let sig = |fourcc| -> Result<Vec<container::SigElement>, ShaderError> {
        match c.part(fourcc) {
            Some(part) => signature(part.data),
            None => Ok(Vec::new()),
        }
    };
    let pixel = native.stage == HwStage::Ps;
    let mut inputs = sig(FourCC::ISG1)?;
    let mut outputs = sig(FourCC::OSG1)?;
    // Interpolation as DXC declares it by default; pixel inputs that are
    // read get the mode their barycentrics say.
    let interpolated = if pixel { &mut inputs } else { &mut outputs };
    for e in interpolated {
        e.interp = match (e.kind, e.comp) {
            (3, _) => 4,
            // Constant, as DXC declares it (the AMD compiler crashed on a
            // front face without a mode).
            (13, _) => 1,
            (0 | 6 | 7, 9) => 2,
            (0, _) => 1,
            _ => 0,
        };
    }
    // With a conservative depth output (SV_DepthLessEqual, GreaterEqual) DXIL
    // wants the position centroid (or per sample), as DXC declares it then.
    if pixel && outputs.iter().any(|e| matches!(e.kind, 18 | 19)) {
        for e in inputs.iter_mut().filter(|e| e.kind == 3) {
            e.interp = 5;
        }
    }
    // Per-primitive outputs are constant. The console never exports a
    // primitive id or cull flag a mesh shader writes (it culls with the
    // primitive export), so those rows are left out.
    let mut prims = sig(FourCC::PSG1)?;
    prims.retain(|e| !matches!(e.kind, 10 | 30));
    for e in &mut prims {
        e.interp = 1;
    }
    // A mesh group's id arrives multiplied by max(threads, vertices,
    // primitives) (the code divides by that); the thread count is in PSV0
    // from runtime info version 2 on.
    // shortcut: older containers fall back to the group's lane count, right
    // when the HLSL group fills its waves (Grand Theft Auto VI's do).
    let mesh_stride = match &native.io {
        crate::native::StageIo::Ms(ms) => {
            let psv = c.part(FourCC::PSV0).map(|p| p.data);
            let threads = psv
                .filter(|d| crate::dxbc::u32_at(d, 0).is_ok_and(|n| n >= 48))
                .and_then(|d| crate::dxbc::u32_at(d, 4 + 36).ok());
            threads.map_or(ms.threads, |t| {
                t.max(ms.max_vertices).max(ms.max_primitives)
            })
        }
        _ => 0,
    };
    let l = lower::lower(&lower::Parts {
        native,
        p,
        st,
        io,
        res,
        inputs,
        outputs,
        prims,
        mesh_stride,
    })?;
    Ok(HostShader {
        stage: native.stage,
        dxil: l.dxil,
        user_data: l.user_data,
        static_samplers: l
            .samplers
            .iter()
            .enumerate()
            .map(|(n, &w)| {
                let mut s = StaticSampler::from_words(w);
                if l.cmp_samplers >> n & 1 != 0 {
                    s.filter |= 0x80;
                }
                s
            })
            .collect(),
        samplers: l.samplers,
        root_cbvs: l.root_cbvs,
        root_srvs: l.root_srvs,
    })
}

/// Rows (registers) of a stage's input or output signature: the public
/// D3D12 limit (`D3D12_VS_INPUT_REGISTER_COUNT`,
/// `D3D12_VS_OUTPUT_REGISTER_COUNT` and `D3D12_PS_INPUT_REGISTER_COUNT` are
/// all 32), which the DXIL we emit must keep. This is the host's limit, not
/// a console rule: a signature beyond it is refused, not emulated.
const SIGNATURE_ROWS: u32 = 32;

/// The rows of an `ISG1`/`OSG1` part. Each element takes one row.
fn signature(part: &[u8]) -> Result<Vec<container::SigElement>, ShaderError> {
    use crate::dxbc::u32_at;
    let count = u32_at(part, 0)? as usize;
    let base = u32_at(part, 4)? as usize;
    // Each row is 32 bytes: a count past the part's size fails below, after
    // reserving no more than the part can hold.
    let mut out = Vec::with_capacity(count.min(part.len() / 32));
    for i in 0..count {
        let at = base + 32 * i;
        let name_at = u32_at(part, at + 4)? as usize;
        let name = part
            .get(name_at..)
            .and_then(|s| s.split(|&b| b == 0).next())
            .ok_or(ShaderError::Malformed("signature name out of bounds"))?;
        let mask = *part
            .get(at + 24)
            .ok_or(ShaderError::Malformed("truncated"))?;
        if u32_at(part, at + 28)? != 0 {
            return Err(ShaderError::Unsupported {
                offset: 0,
                what: "a minimum-precision signature element",
            });
        }
        if u32_at(part, at)? != 0 {
            return Err(ShaderError::Unsupported {
                offset: 0,
                what: "a signature element of another stream",
            });
        }
        let kind = match u32_at(part, at + 12)? {
            0 => 0,
            1 => 3,
            2 => 6,
            3 => 7,
            4 => 4,
            5 => 5,
            6 => 1,
            7 => 10,
            8 => 2,
            9 => 13,
            10 => 12,
            64 => 16,
            65 => 17,
            66 => 14,
            67 => 19,
            68 => 18,
            69 => 20,
            70 => 15,
            25 => 30, // cull primitive
            _ => {
                return Err(ShaderError::Unsupported {
                    offset: 0,
                    what: "a system value in the signature",
                });
            }
        };
        // 16-bit elements are declared 32-bit; reading or writing one
        // converts (16-bit values travel packed on the console).
        let raw = u32_at(part, at + 16)?;
        let comp = match (kind, raw) {
            // DXIL declares the front face a uint (the validator insists).
            (13, _) => 5,
            (_, 1 | 4) => 5,
            (_, 2 | 5) => 4,
            (_, 3 | 6) => 9,
            (_, c) => {
                return Err(ShaderError::Unsupported {
                    offset: 0,
                    what: match c {
                        0 => "a signature element of unknown type",
                        _ => "a 64-bit signature element",
                    },
                });
            }
        };
        // A register past the signature's rows (`u32::MAX` is none) would
        // size the tables built per row from a title's number.
        let register = u32_at(part, at + 20)?;
        if register != u32::MAX && register >= SIGNATURE_ROWS {
            return Err(ShaderError::Malformed(
                "signature register past the 32 rows of a D3D12 signature",
            ));
        }
        out.push(container::SigElement {
            name: String::from_utf8_lossy(name).into_owned(),
            index: u32_at(part, at + 8)?,
            kind,
            comp,
            interp: 0,
            row: (register != u32::MAX).then_some(register),
            col: mask.trailing_zeros().min(3) as u8,
            cols: mask.count_ones().max(1) as u8,
            usage: 0,
            half: (4..=6).contains(&raw),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::bitcode::Attr;
    use super::lower::LOOP_LIMIT;
    use super::parse_loop_limit;

    /// `VICTORY_REQUIRE_GPU`, `_WARP`, `_DXIL` and `_DIFF`: `1` requires,
    /// anything else (unset, empty, `0`, `true`) does not.
    #[test]
    fn the_require_switch_reads_one_as_required() {
        assert!(super::require(Some("1")));
        assert!(super::require(Some(" 1\n")));
        for other in [
            None,
            Some(""),
            Some("0"),
            Some("true"),
            Some("yes"),
            Some("11"),
        ] {
            assert!(!super::require(other), "{other:?}");
        }
    }

    #[test]
    fn the_loop_limit_switch_parses_or_falls_back() {
        assert_eq!(parse_loop_limit(None), LOOP_LIMIT);
        assert_eq!(parse_loop_limit(Some("4096")), 4096);
        assert_eq!(parse_loop_limit(Some(" 0x1000 ")), 4096);
        assert_eq!(parse_loop_limit(Some("1")), 1);
        for bad in ["", "0", "-5", "lots", "0x", "4294967296"] {
            assert_eq!(parse_loop_limit(Some(bad)), LOOP_LIMIT, "{bad:?}");
        }
    }
    /// A signature part that claims billions of rows is malformed, not a
    /// request to reserve gigabytes up front.
    #[test]
    fn a_signature_with_a_huge_row_count_is_malformed() {
        let mut part = Vec::new();
        part.extend(u32::MAX.to_le_bytes());
        part.extend(8u32.to_le_bytes());
        assert!(super::signature(&part).is_err());
    }

    /// A signature row's register comes from the title's container and
    /// sizes what is built per row (the PSV0 vectors and dependency table,
    /// the view-id state). D3D12 has 32 rows per signature; a register
    /// past them is malformed, at once: 0xFFFF_FFFE overflowed `row + 1`,
    /// and 0x1000_0000 asked for a dependency table of billions of words.
    #[test]
    fn a_signature_register_past_the_limit_is_malformed() {
        use crate::testkit::{self, Element};
        let words = [0xBF81_0000u32]; // s_endpgm
        let translate = |register: u32| {
            let isg1 = [Element::float("TEXCOORD", 0, register, 0xF)];
            let osg1 = [Element::float("SV_Target", 64, register, 0xF)];
            let both = testkit::console_container(HwStage::Ps, &words, &isg1, &osg1);
            let input = testkit::console_container(HwStage::Ps, &words, &isg1, &[]);
            let output = testkit::console_container(HwStage::Ps, &words, &[], &osg1);
            [both, input, output].map(|c| super::translate(&c).map(|s| s.stage))
        };
        let malformed = Err(ShaderError::Malformed(
            "signature register past the 32 rows of a D3D12 signature",
        ));
        let started = std::time::Instant::now();
        for register in [32, 0x1000_0000, 0xFFFF_FFFE] {
            for result in translate(register) {
                assert_eq!(result, malformed, "register {register:#x}");
            }
        }
        // The last row and "no register" are not refused for their row.
        for register in [31, u32::MAX] {
            for result in translate(register) {
                assert_ne!(result, malformed, "register {register:#x}");
            }
        }
        // Loose on purpose (a slow, loaded machine): the table the old code
        // built for 0x1000_0000 took minutes and gigabytes.
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
    }

    use super::container::SigElement;
    use super::shader::Shader;
    use super::*;
    use crate::native::{ComputeIo, StageIo};

    /// A pixel shader writing SV_StencilRef (channel y of `exp mrtz`).
    /// DXC sets ShaderFlags.StencilRef (bit 11) for a pixel shader whose
    /// output signature has SV_StencilRef, and the feature bit
    /// D3D_SHADER_FEATURE_STENCIL_REF (0x200) in SFI0; the validator checks
    /// both against the module.
    #[test]
    fn a_stencil_export_declares_the_stencil_ref_feature() {
        let words = [
            0x7E00_02F2u32, // v_mov_b32 v0, 1.0
            0x7E02_0285,    // v_mov_b32 v1, 5
            0xF800_1883,
            0x0000_0100, // exp mrtz v0, v1, off, off done vm
            0xBF81_0000,
        ];
        let native = NativeProgram {
            stage: HwStage::Ps,
            wave64: false,
            vgprs: 2,
            layout: vec![],
            used: vec![],
            io: StageIo::Ps {
                inputs: vec![],
                input_addr: 0,
                db_shader_control: 0,
                patches: vec![],
            },
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let el = |name: &str, kind, comp| SigElement {
            name: name.into(),
            index: 0,
            kind,
            comp,
            interp: 0,
            row: None,
            col: 0,
            cols: 1,
            usage: 1,
            half: false,
        };
        let osg1 =
            container::signature(&[el("SV_Depth", 17, 9), el("SV_StencilRef", 20, 5)], false);
        let bytes = crate::dxbc::build(&[(FourCC::OSG1, &osg1)]);
        let c = Container::parse(&bytes).unwrap();
        let p = ir::build(&native.patched_code().unwrap(), native.wave64, native.vgprs).unwrap();
        let st = structure::structure(&p).unwrap();
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = io::raise(&p, &native).unwrap();
        let out = emit(&c, &native, &p, &st, &io, &res).unwrap();
        // dxil.dll checks the flags against the module (a visible skip
        // where it cannot be loaded).
        crate::testing::dxil(&out.dxil);
        let host = Container::parse(&out.dxil).unwrap();
        let sfi0 = host.part(FourCC::SFI0).unwrap().data;
        let features = u64::from_le_bytes(sfi0.try_into().unwrap());
        assert_ne!(features & 0x200, 0, "SFI0 {features:#x}: no StencilRef");
    }

    /// A wave64 compute program that writes only EXEC's high dword
    /// (`s_mov_b32 exec_hi, 0`) before a vector instruction. The new mask is
    /// opaque and keyed by the unchanged low dword, so building its lane bit
    /// needed the mask's own value: lowering recursed until the stack
    /// overflowed, which takes the whole process down (found by
    /// random-input testing). Now the instruction fails like any other that
    /// cannot be expressed: its result is missing, and only a side effect
    /// that uses it would stop the translation.
    #[test]
    fn an_exec_high_write_does_not_overflow_the_stack() {
        // s_mov_b32 exec_hi, 0; v_mov_b32 v1, v0; s_endpgm
        let words = [0xBEFF_0380u32, 0x7E02_0300, 0xBF81_0000];
        let native = NativeProgram {
            stage: HwStage::Cs,
            wave64: true,
            vgprs: 2,
            layout: vec![],
            used: vec![],
            io: StageIo::Cs(ComputeIo {
                rsrc2: 0,
                threads: [64, 1, 1],
            }),
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let p = ir::build(&native.patched_code().unwrap(), native.wave64, native.vgprs).unwrap();
        let st = structure::structure(&p).unwrap();
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = io::raise(&p, &native).unwrap();
        let container = crate::dxbc::build(&[]);
        let c = Container::parse(&container).unwrap();
        // The v_mov's result is missing and nothing uses it: the program
        // translates, to a module the validator takes.
        let out = emit(&c, &native, &p, &st, &io, &res);
        assert_eq!(out.as_ref().map(|s| s.stage), Ok(HwStage::Cs));
        crate::testing::dxil(&out.unwrap().dxil);
    }

    /// A pixel shader writing a constant colour.
    #[cfg(windows)]
    fn constant_color() -> Vec<u8> {
        let mut s = Shader::new(true);
        s.outputs.push(SigElement {
            name: "SV_Target".into(),
            index: 0,
            kind: 16,
            comp: 9,
            interp: 0,
            row: Some(0),
            col: 0,
            cols: 4,
            usage: 15,
            half: false,
        });
        let b = s.entry;
        for (c, v) in [1.0f32, 0.5, 0.25, 1.0].into_iter().enumerate() {
            let args = [
                s.m.i32(0),
                s.m.i32(0),
                s.m.i8(c as u8),
                s.m.f32(v.to_bits()),
            ];
            s.op(b, "storeOutput.f32", Attr::NoUnwind, s.t.void, 5, &args);
        }
        s.m.terminate(b, super::bitcode::Term::Ret);
        s.finish()
    }

    #[test]
    #[cfg(windows)]
    fn stub_pipeline_is_created() {
        let Some(host) = crate::testing::host(false) else {
            return;
        };
        let ps = super::HostShader {
            stage: crate::HwStage::Ps,
            dxil: constant_color(),
            user_data: Vec::new(),
            samplers: Vec::new(),
            static_samplers: Vec::new(),
            root_cbvs: Vec::new(),
            root_srvs: Vec::new(),
        };
        let file = |var: &str, stage| {
            std::env::var(var).ok().map(|p| super::HostShader {
                stage,
                dxil: std::fs::read(p).unwrap(),
                user_data: Vec::new(),
                samplers: Vec::new(),
                static_samplers: Vec::new(),
                root_cbvs: Vec::new(),
                root_srvs: Vec::new(),
            })
        };
        let ps = file("VICTORY_REF_PS", crate::HwStage::Ps).unwrap_or(ps);
        let vs = file("VICTORY_REF_VS", crate::HwStage::Vs)
            .unwrap_or_else(|| super::host::stub_vs(&ps).unwrap());
        crate::testing::dxil(&vs.dxil);
        host.pipeline(&vs, &ps).unwrap();
        // The runtime checks the container hash: no hash or a wrong one is
        // refused. (Recent runtimes also take sixteen 0x01 bytes, a bypass
        // value; ours works with older ones too.)
        // D3DMetal under Wine does not check the hash, so it accepts both.
        // SAFETY: ntdll is always loaded; both names end with a NUL.
        let wine = unsafe {
            use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
            GetProcAddress(
                GetModuleHandleA(c"ntdll.dll".as_ptr().cast()),
                c"wine_get_version".as_ptr().cast(),
            )
        }
        .is_some();
        let mut wrong = ps.dxil[4..20].to_vec();
        wrong[0] ^= 1;
        let digests: &[&[u8]] = if wine { &[] } else { &[&[0u8; 16], &wrong] };
        for &digest in digests {
            let mut other = ps.clone();
            other.dxil[4..20].copy_from_slice(digest);
            assert!(
                host.pipeline(&vs, &other).is_err(),
                "digest {digest:02x?} accepted"
            );
        }
    }

    #[test]
    #[cfg(windows)]
    fn constant_color_validates() {
        // Validates, and compares our hash with the validator's.
        crate::testing::dxil(&constant_color());
    }

    /// A pixel shader interpolating at sample positions (PERSP_SAMPLE
    /// barycentrics, `SPI_PS_INPUT_ADDR` bit 0) runs at sample frequency:
    /// DXC's PSV0 says so in PSInfo.SampleFrequency (runtime info byte 1)
    /// whenever an input is interpolated per sample.
    #[test]
    fn sample_interpolated_inputs_set_psv_sample_frequency() {
        let words = [
            0xC808_0000u32, // v_interp_p1_f32 v2, v0, attr0.x
            0xC809_0001,    // v_interp_p2_f32 v2, v1, attr0.x
            0xF800_180F,
            0x0202_0202, // exp mrt0, v2, v2, v2, v2 done vm
            0xBF81_0000,
        ];
        let psv_of = |input_addr: u32| {
            let native = super::NativeProgram {
                stage: super::HwStage::Ps,
                wave64: false,
                vgprs: 4,
                layout: vec![],
                used: vec![],
                io: crate::native::StageIo::Ps {
                    inputs: vec![crate::native::PsInput {
                        usage: crate::native::Usage::Generic,
                        row: 0,
                        mask: 0b1111,
                        attr: 0,
                        flat: false,
                        fp16: false,
                        pack2: false,
                        row2: 0,
                    }],
                    input_addr,
                    db_shader_control: 0,
                    patches: vec![],
                },
                code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
            };
            let el = |name: &str, kind, usage| SigElement {
                name: name.into(),
                index: 0,
                kind,
                comp: 9,
                interp: 0,
                row: Some(0),
                col: 0,
                cols: 4,
                usage,
                half: false,
            };
            let isg1 = super::container::signature(&[el("TEXCOORD", 0, 0)], true);
            let osg1 = super::container::signature(&[el("SV_Target", 16, 15)], false);
            let bytes =
                crate::dxbc::build(&[(super::FourCC::ISG1, &isg1), (super::FourCC::OSG1, &osg1)]);
            let c = super::Container::parse(&bytes).unwrap();
            let p = crate::ir::build(&native.patched_code().unwrap(), native.wave64, native.vgprs)
                .unwrap();
            let st = crate::structure::structure(&p).unwrap();
            let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
            let io = crate::io::raise(&p, &native).unwrap();
            let out = super::emit(&c, &native, &p, &st, &io, &res).unwrap();
            // dxil.dll checks PSV0 against the module (a visible skip where
            // it cannot be loaded).
            crate::testing::dxil(&out.dxil);
            let host = super::Container::parse(&out.dxil).unwrap();
            host.part(super::FourCC::PSV0).unwrap().data.to_vec()
        };
        // Runtime info starts after its size; PSInfo is {DepthOutput, SampleFrequency}.
        let center = psv_of(1 << 1);
        assert_eq!(center[4 + 1], 0, "center interpolation is per pixel");
        let sample = psv_of(1 << 0);
        assert_eq!(sample[4 + 1], 1, "PSInfo.SampleFrequency");
    }

    /// A pixel shader reading channel z of an input whose element starts at
    /// w. Looking the channel up subtracted the element's first channel
    /// before checking the channel lies in it, so in a build with overflow
    /// checks (tests, debug) any read below an element that starts at z or
    /// w panicked, whichever row it was in (found by random-input testing).
    #[test]
    fn reading_a_channel_below_an_input_element_does_not_underflow() {
        // v_interp_mov_f32 v3, p0, attr0.z; s_endpgm
        let words = [0xC80E_0202u32, 0xBF81_0000];
        let native = super::NativeProgram {
            stage: super::HwStage::Ps,
            wave64: false,
            vgprs: 4,
            layout: vec![],
            used: vec![],
            io: crate::native::StageIo::Ps {
                inputs: vec![crate::native::PsInput {
                    usage: crate::native::Usage::Generic,
                    row: 0,
                    mask: 0b1000,
                    attr: 0,
                    flat: false,
                    fp16: false,
                    pack2: false,
                    row2: 0,
                }],
                input_addr: 0,
                db_shader_control: 0,
                patches: vec![],
            },
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let w_only = SigElement {
            name: "TEXCOORD".into(),
            index: 0,
            kind: 0,
            comp: 9,
            interp: 0,
            row: Some(0),
            col: 3,
            cols: 1,
            usage: 0,
            half: false,
        };
        let isg1 = super::container::signature(&[w_only], true);
        let bytes = crate::dxbc::build(&[(super::FourCC::ISG1, &isg1)]);
        let c = super::Container::parse(&bytes).unwrap();
        let p =
            crate::ir::build(&native.patched_code().unwrap(), native.wave64, native.vgprs).unwrap();
        let st = crate::structure::structure(&p).unwrap();
        let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = crate::io::raise(&p, &native).unwrap();
        // The read lies in no element and its value goes nowhere: the
        // program translates, to a module the validator takes.
        let out = super::emit(&c, &native, &p, &st, &io, &res);
        assert_eq!(out.as_ref().map(|s| s.stage), Ok(super::HwStage::Ps));
        crate::testing::dxil(&out.unwrap().dxil);
    }

    use crate::gfx10::asm;
    use crate::testkit;

    /// The testkit writes signature parts with its own writer
    /// (`testkit::signature_part`); the emitter's is `container::signature`.
    /// For the same elements the two give the same bytes: the testkit's
    /// part, read by the emitter's reader and written back by its writer.
    /// As an input signature, that is: the reader keeps no usage, so the
    /// writer's always-reads byte is 0 like the testkit's, where an output
    /// signature's never-writes byte would be the whole mask.
    #[test]
    fn the_testkit_and_the_emitter_write_the_same_signature() {
        let elements = [
            testkit::Element::float("POSITION", 0, 0, 0xF),
            testkit::Element {
                name: "TEXCOORD",
                index: 1,
                system_value: 0,
                component: 2,
                register: 1,
                mask: 0b0110,
            },
            testkit::Element {
                name: "SV_InstanceID",
                index: 0,
                system_value: 8,
                component: 1,
                register: 2,
                mask: 0b0001,
            },
        ];
        let part = testkit::signature_part(&elements);
        let read = super::signature(&part).unwrap();
        assert_eq!(read.len(), elements.len());
        for (r, e) in read.iter().zip(&elements) {
            assert_eq!((r.name.as_str(), r.index), (e.name, e.index));
            assert_eq!(r.row, Some(e.register), "{}", e.name);
            assert_eq!(((1u8 << r.cols) - 1) << r.col, e.mask, "{}", e.name);
        }
        assert_eq!(container::signature(&read, true), part);
    }

    /// Translates a synthetic console container and has dxil.dll check
    /// the result. Where the validator cannot be loaded that check is not
    /// made (`crate::testing::dxil` prints the skip), and the tests using
    /// this check structure only.
    fn translated(container: &[u8]) -> HostShader {
        let out = translate(container).unwrap();
        crate::testing::dxil(&out.dxil);
        out
    }

    /// The compute programs of the feature tests: `body`, then a store of v0
    /// to the UAV.
    fn compute(body: &[Vec<u32>]) -> Vec<u8> {
        let mut words = body.concat();
        words.extend(testkit::store_dword(0, 0));
        testkit::container(&testkit::compute_with_uav(&words), &[], &[])
    }

    /// The whole path a title's shader takes, on containers assembled here:
    /// container, `DXIL` part, XDAT index, packed blob, stage record, code,
    /// signatures, and out comes a host shader that binds what the program
    /// reads.
    #[test]
    fn a_synthetic_console_container_translates() {
        let user = |index, slot, root, dword| UserData {
            index,
            slot,
            root,
            dword,
        };
        let constants: Vec<UserData> = (0..4).map(|i| user(i, 0, RootType::Constants, i)).collect();
        let table = user(4, 1, RootType::Table, 0);
        let cases: [(&str, Vec<u8>, HwStage, Vec<UserData>); 6] = [
            (
                "vs_fullscreen_triangle",
                testkit::vs_fullscreen_triangle(),
                HwStage::Vs,
                vec![],
            ),
            (
                "vs_position_buffer",
                testkit::vs_position_buffer(),
                HwStage::Vs,
                vec![],
            ),
            (
                "ps_constant",
                testkit::ps_constant([1.0, 0.5, 0.25, 1.0]),
                HwStage::Ps,
                vec![],
            ),
            (
                "ps_root_constants",
                testkit::ps_root_constants(),
                HwStage::Ps,
                constants.clone(),
            ),
            (
                "cs_store_root_constant",
                testkit::cs_store_root_constant(),
                HwStage::Cs,
                vec![constants[0], table],
            ),
            (
                "cs_store_words",
                testkit::cs_store_words(&[1, 2, 3]),
                HwStage::Cs,
                vec![table],
            ),
        ];
        for (name, container, stage, user_data) in cases {
            let out = translated(&container);
            assert_eq!(out.stage, stage, "{name}");
            assert_eq!(out.user_data, user_data, "{name}");
            assert!(out.samplers.is_empty() && out.static_samplers.is_empty());
            assert!(out.root_cbvs.is_empty() && out.root_srvs.is_empty());
            let host = Container::parse(&out.dxil).unwrap();
            assert!(host.part(FourCC::DXIL).is_some(), "{name}");
            // The input signature is the title's, plus the vertex id a
            // program reads (the console signature leaves it out).
            let isg1 = host.part(FourCC::ISG1).unwrap().data;
            let names = |needle: &[u8]| isg1.windows(needle.len()).any(|w| w == needle);
            assert_eq!(names(b"POSITION\0"), name == "vs_position_buffer", "{name}");
            assert_eq!(
                names(b"SV_VertexID\0"),
                name == "vs_fullscreen_triangle",
                "{name}"
            );
        }
        // A container whose only program is a hull, domain or geometry one
        // is refused by name, not translated as something else.
        for stage in [HwStage::Ls, HwStage::Hs, HwStage::Es, HwStage::Gs] {
            let c = testkit::console_container(stage, &[asm::S_ENDPGM], &[], &[]);
            let Err(ShaderError::Unsupported { what, .. }) = translate(&c) else {
                panic!("{stage:?} translated");
            };
            assert!(what.contains("without a vertex, pixel, compute"), "{what}");
        }
    }

    /// What the host runtime reads before the bitcode: the parts in the
    /// order validator 1.6 writes them, the stage in `PSV0` and in the
    /// program header, and a hash over everything after it.
    #[test]
    fn emitted_containers_carry_the_parts_each_stage_needs() {
        // A mesh program whose every lane exports a vertex at the origin
        // (all four channels of the position its signature declares) and
        // a primitive with all indices 0: enough to be a mesh shader.
        let mesh = {
            let words = [
                asm::vop1("v_mov_b32", 0, asm::imm(0)),
                asm::exp(asm::target::POS0, 0xF, [0; 4], asm::ExpFlags::default()),
                asm::exp(
                    asm::target::PRIM,
                    1,
                    [0; 4],
                    asm::ExpFlags {
                        done: true,
                        ..asm::ExpFlags::default()
                    },
                ),
                vec![asm::S_ENDPGM],
            ]
            .concat();
            let program = NativeProgram {
                wave64: false,
                vgprs: 8,
                io: StageIo::Ms(crate::native::MeshIo {
                    outputs: vec![],
                    clip_cntl: 0,
                    threads: 32,
                    max_vertices: 24,
                    max_primitives: 16,
                    topology: 2,
                    lds_bytes: 0,
                }),
                ..testkit::bare_program(HwStage::Ms, &asm::bytes(&words))
            };
            testkit::container(&program, &[], &[testkit::SV_POSITION])
        };
        // (container, the kind PSV0 and the program header name)
        let cases = [
            (testkit::ps_constant([0.0; 4]), 0u8),
            (testkit::vs_fullscreen_triangle(), 1),
            (testkit::cs_store_root_constant(), 5),
            (mesh, 13),
        ];
        for (container, kind) in cases {
            let out = translated(&container);
            let host = Container::parse(&out.dxil).unwrap();
            let parts: Vec<String> = host.parts.iter().map(|p| p.fourcc.to_string()).collect();
            let mut want = vec!["SFI0", "ISG1", "OSG1", "PSV0", "HASH", "DXIL"];
            if kind == 13 {
                want.insert(3, "PSG1");
            }
            assert_eq!(parts, want, "kind {kind}");
            assert_eq!(host.digest, container::hash(&out.dxil[20..]), "kind {kind}");
            assert_eq!(host.part(FourCC::SFI0).unwrap().data.len(), 8);

            // PSV0: the runtime info's size, then the info (version 2).
            let psv = host.part(FourCC::PSV0).unwrap().data;
            let info = &psv[4..];
            let u32_at =
                |d: &[u8], at: usize| u32::from_le_bytes(d[at..at + 4].try_into().unwrap());
            assert_eq!(u32_at(psv, 0), 48, "kind {kind}");
            assert_eq!(info[24], kind, "PSV0 shader stage");
            match kind {
                // Vertex: OutputPositionPresent.
                1 => assert_eq!(info[0], 1),
                // Compute: the thread-group size.
                5 => assert_eq!([36, 40, 44].map(|at| u32_at(info, at)), [1, 1, 1]),
                // Mesh: maximum vertices and primitives, the topology.
                13 => {
                    assert_eq!(info[12..16], [24, 0, 16, 0]);
                    assert_eq!(info[27], 2);
                    assert_eq!(u32_at(info, 36), 32);
                }
                _ => assert_eq!(info[..2], [0, 0], "no depth output, per pixel"),
            }
            // Signature counts in PSV0 match the signature parts.
            let rows = |fourcc| u32_at(host.part(fourcc).unwrap().data, 0) as u8;
            assert_eq!(
                [info[28], info[29]],
                [rows(FourCC::ISG1), rows(FourCC::OSG1)],
                "kind {kind}"
            );

            // The program header: kind and shader model 6.6, the size in
            // dwords, then the bitcode header.
            let dxil = host.part(FourCC::DXIL).unwrap().data;
            assert_eq!(u32_at(dxil, 0), u32::from(kind) << 16 | 0x66);
            assert_eq!(u32_at(dxil, 4) as usize * 4, dxil.len());
            assert_eq!(&dxil[8..12], b"DXIL");
            assert_eq!(u32_at(dxil, 20) as usize, dxil.len() - 24);
            assert_eq!(&dxil[24..28], b"BC\xC0\xDE");
        }
    }

    /// The containers of the two tests below: a vertex, pixel or mesh
    /// program that moves 0 into v0 and exports it to each of `exports`
    /// (target, channel enables), the last one `done`.
    fn exporting(stage: HwStage, exports: &[(u8, u8)], osg1: &[testkit::Element]) -> Vec<u8> {
        let mut words = asm::vop1("v_mov_b32", 0, asm::imm(0));
        for (n, &(target, en)) in exports.iter().enumerate() {
            let flags = asm::ExpFlags {
                done: n + 1 == exports.len(),
                vm: stage == HwStage::Ps,
                ..asm::ExpFlags::default()
            };
            words.extend(asm::exp(target, en, [0; 4], flags));
        }
        words.push(asm::S_ENDPGM);
        if stage != HwStage::Ms {
            return testkit::console_container(stage, &words, &[], osg1);
        }
        let program = NativeProgram {
            wave64: false,
            vgprs: 8,
            io: StageIo::Ms(crate::native::MeshIo {
                outputs: vec![],
                clip_cntl: 0,
                threads: 32,
                max_vertices: 24,
                max_primitives: 16,
                topology: 2,
                lds_bytes: 0,
            }),
            ..testkit::bare_program(HwStage::Ms, &asm::bytes(&words))
        };
        testkit::container(&program, &[], osg1)
    }

    /// A signature row for the two tests below: `system_value` is the
    /// `D3D_NAME`, `component` 1 for uint and 3 for float.
    fn row(
        name: &'static str,
        system_value: u32,
        component: u32,
        register: u32,
        mask: u8,
    ) -> testkit::Element {
        testkit::Element {
            name,
            index: 0,
            system_value,
            component,
            register,
            mask,
        }
    }

    /// dxil.dll 1.9.2609 rejects a module that declares a system-value
    /// output and has no store for each of its channels ("Not all elements
    /// of output SV_Position were written."), and D3D12 does not say what
    /// such an output holds. A console program that leaves one unwritten,
    /// or writes part of it, is refused with an error instead of becoming
    /// a module the validator would reject.
    #[test]
    fn a_system_value_output_not_written_in_full_is_refused() {
        use asm::target::{MRT0, POS0, PRIM};
        use testkit::{SV_POSITION, SV_TARGET};
        const NONE: u32 = u32::MAX;
        let cases: [(&str, Vec<u8>); 9] = [
            (
                "a position without w",
                exporting(HwStage::Vs, &[(POS0, 0x7)], &[SV_POSITION]),
            ),
            (
                "clip distances never exported",
                exporting(
                    HwStage::Vs,
                    &[(POS0, 0xF)],
                    &[SV_POSITION, row("SV_ClipDistance", 2, 3, 1, 0x3)],
                ),
            ),
            (
                "one of two clip distances",
                exporting(
                    HwStage::Vs,
                    &[(POS0 + 1, 0x1), (POS0, 0xF)],
                    &[SV_POSITION, row("SV_ClipDistance", 2, 3, 1, 0x3)],
                ),
            ),
            (
                "cull distances never exported",
                exporting(
                    HwStage::Vs,
                    &[(POS0, 0xF)],
                    &[SV_POSITION, row("SV_CullDistance", 3, 3, 1, 0x3)],
                ),
            ),
            // No export of the translator writes these two.
            (
                "a render-target index",
                exporting(
                    HwStage::Vs,
                    &[(POS0, 0xF)],
                    &[SV_POSITION, row("SV_RenderTargetArrayIndex", 4, 1, 1, 0x1)],
                ),
            ),
            (
                "a viewport index",
                exporting(
                    HwStage::Vs,
                    &[(POS0, 0xF)],
                    &[SV_POSITION, row("SV_ViewportArrayIndex", 5, 1, 1, 0x1)],
                ),
            ),
            (
                "a depth never exported",
                exporting(
                    HwStage::Ps,
                    &[(MRT0, 0xF)],
                    &[SV_TARGET, row("SV_Depth", 65, 3, NONE, 0x1)],
                ),
            ),
            (
                "a coverage never exported",
                exporting(
                    HwStage::Ps,
                    &[(MRT0, 0xF)],
                    &[SV_TARGET, row("SV_Coverage", 66, 1, NONE, 0x1)],
                ),
            ),
            (
                "a mesh program without vertex exports",
                exporting(HwStage::Ms, &[(PRIM, 0x1)], &[SV_POSITION]),
            ),
        ];
        for (name, container) in cases {
            match translate(&container) {
                Err(ShaderError::Unsupported { what, .. }) => {
                    assert_eq!(what, lower::UNWRITTEN_OUTPUT, "{name}")
                }
                Err(other) => panic!("{name}: {other:?}"),
                Ok(_) => panic!("{name}: translated"),
            }
        }
    }

    /// What the refusal above leaves alone: user semantics and render
    /// targets the program writes in part or not at all still translate,
    /// and the validator accepts the modules (the signature says which
    /// channels are never written).
    #[test]
    fn user_and_target_outputs_may_stay_unwritten() {
        use asm::target::{MRT0, POS0, PRIM};
        use testkit::{SV_POSITION, SV_TARGET};
        let texcoord = row("TEXCOORD", 0, 3, 1, 0xF);
        let target1 = testkit::Element {
            index: 1,
            register: 1,
            ..SV_TARGET
        };
        let cases: [(&str, Vec<u8>); 4] = [
            (
                "a parameter never exported",
                exporting(HwStage::Vs, &[(POS0, 0xF)], &[SV_POSITION, texcoord]),
            ),
            (
                "a target without alpha",
                exporting(HwStage::Ps, &[(MRT0, 0x7)], &[SV_TARGET]),
            ),
            (
                "a second target never exported",
                exporting(HwStage::Ps, &[(MRT0, 0xF)], &[SV_TARGET, target1]),
            ),
            (
                "a mesh parameter never exported",
                exporting(
                    HwStage::Ms,
                    &[(POS0, 0xF), (PRIM, 0x1)],
                    &[SV_POSITION, texcoord],
                ),
            ),
        ];
        for (name, container) in cases {
            let out = translated(&container);
            let host = Container::parse(&out.dxil).unwrap();
            // Each 32-byte row: the mask at 24, the never-writes mask
            // after it. Some declared channel is marked never written.
            let osg1 = host.part(FourCC::OSG1).unwrap().data;
            let rows = u32::from_le_bytes(osg1[0..4].try_into().unwrap()) as usize;
            let unwritten = (0..rows).any(|r| {
                let at = 8 + 32 * r;
                osg1[at + 24] & osg1[at + 25] != 0
            });
            assert!(unwritten, "{name}: no channel marked never written");
        }
    }

    /// `SFI0` names the optional features the module uses, and only those:
    /// the runtime refuses a shader that uses one it does not declare, and
    /// a device without a declared one refuses the shader.
    #[test]
    fn feature_bits_follow_what_the_program_uses() {
        const TYPED_UAV_LOADS: u64 = 0x800;
        const WAVE_OPS: u64 = 0x4000;
        const INT64: u64 = 0x8000;
        const RESOURCE_HEAP: u64 = 0x200_0000;
        const SAMPLER_HEAP: u64 = 0x400_0000;
        let none = asm::Vop3Mods::default();
        let (v, s) = (asm::vgpr, asm::sgpr);
        let cases: [(&str, Vec<u8>, u64); 7] = [
            ("no resources", testkit::ps_constant([0.0; 4]), 0),
            ("root constants only", testkit::ps_root_constants(), 0),
            ("vertex id", testkit::vs_fullscreen_triangle(), 0),
            // The UAV comes from the descriptor heap.
            (
                "a raw store",
                testkit::cs_store_root_constant(),
                RESOURCE_HEAP,
            ),
            (
                "a 32 x 32 high product",
                compute(&[
                    asm::vop1("v_mov_b32", 1, s(0)),
                    asm::vop1("v_mov_b32", 2, s(1)),
                    asm::vop3("v_mul_hi_u32", 0, &[v(1), v(2)], none),
                ]),
                RESOURCE_HEAP | INT64,
            ),
            (
                "another lane's value",
                compute(&[
                    asm::vop1("v_mov_b32", 1, s(0)),
                    asm::vop3("v_readlane_b32", 6, &[v(1), asm::imm(0)], none),
                    asm::vop1("v_mov_b32", 0, s(6)),
                ]),
                RESOURCE_HEAP | WAVE_OPS,
            ),
            (
                "a typed load from a UAV",
                testkit::container(
                    &testkit::compute_with_uav(
                        &[
                            asm::vop1("v_mov_b32", 1, asm::imm(0)),
                            asm::mubuf(
                                "buffer_load_format_x",
                                asm::Buffer {
                                    vaddr: 1,
                                    srsrc: 8,
                                    idxen: true,
                                    ..asm::Buffer::default()
                                },
                            ),
                        ]
                        .concat(),
                    ),
                    &[],
                    &[],
                ),
                RESOURCE_HEAP | TYPED_UAV_LOADS,
            ),
        ];
        for (name, container, want) in cases {
            let out = translated(&container);
            let host = Container::parse(&out.dxil).unwrap();
            let sfi0 = host.part(FourCC::SFI0).unwrap().data;
            let features = u64::from_le_bytes(sfi0.try_into().unwrap());
            assert_eq!(features, want, "{name}: {features:#x}");
            assert_eq!(features & SAMPLER_HEAP, 0, "{name}: no program samples");
        }
    }
}
