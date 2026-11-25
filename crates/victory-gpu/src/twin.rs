//! Depth textures the title also writes through unordered access (decision
//! 0007, "One host texture per texture memory"). The console allows both
//! uses of one texture (Grand Theft Auto VI's shadow-map copies and its
//! material depth, written by compute and then tested as depth); the host
//! does not: a depth-stencil resource takes no unordered access, in any
//! format. Such a texture gets a writable twin, a colour texture per plane
//! (depth, stencil) that its unordered access views write, and the newer
//! contents move between the two by copies where the use changes: within
//! a list where it records the change (`HostList::transition`), and at
//! submission for each list's first use, from the side the lists before it
//! left newer. A 16-bit depth plane in the host's 32-bit float one (the
//! console's 16-bit depth with stencil) is converted on the way, so a
//! value written as 16-bit depth compares equal to the same depth
//! rasterised.
//!
//! shortcut: uses the lists do not name (shaders indexing the heap
//! themselves) move nothing; a whole twin is copied for any use of either
//! plane, also when the next pass overwrites all of it; console 24-bit
//! depth gets no twin. Upgrade when a frame trace shows such a use, or the
//! copies (counted on the frame-rate line) cost.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering::Relaxed};

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::s;

use crate::host::{HostList, host, transition};
use crate::object::This;
use crate::resource::HostTexture;

/// Which side holds the newer contents.
pub(crate) const BOTH: u8 = 0;
pub(crate) const DEPTH: u8 = 1;
pub(crate) const TWIN: u8 = 2;

const RENDER_TARGET: u32 = 0x4;
const UNORDERED_ACCESS: u32 = 0x8;
const DEPTH_WRITE: u32 = 0x10;
const COPY_DEST: u32 = 0x400;
const COPY_SOURCE: u32 = 0x800;
const RESOLVE_DEST: u32 = 0x1000;

/// Copies between a depth texture and its twin since the last frame-rate
/// line (either way, within lists and at submission).
pub(crate) static COPIES: AtomicU64 = AtomicU64::new(0);

pub(crate) struct Twin {
    /// The colour texture of each plane the depth texture has (depth, then
    /// stencil), shader-writable.
    pub(crate) planes: Vec<Arc<HostTexture>>,
    /// Which side is newer as of the lists submitted so far.
    pub(crate) newer: AtomicU8,
    /// Per plane and subresource: how its bytes go through `scratch`.
    routes: Vec<Route>,
    /// A shader-writable buffer the copies go through, in the common state
    /// between them.
    scratch: ID3D12Resource,
}

// SAFETY: D3D12 objects are free-threaded; the rest is atomics and data.
unsafe impl Send for Twin {}
// SAFETY: as above.
unsafe impl Sync for Twin {}

/// One subresource of one plane.
struct Route {
    plane: usize,
    /// Its index in the twin's plane texture and in the depth texture.
    twin_sub: u32,
    depth_sub: u32,
    /// Its place in the scratch buffer as the twin's texture has it.
    twin: D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    /// As the depth texture has it, where that differs: a 16-bit depth
    /// plane the host keeps as 32-bit floats, converted.
    depth: Option<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
}

/// A depth texture's use in one list: the side its first use needs (made
/// newer at submission, `at_submission`) and the side the list left newer
/// (`None`: unchanged from where it started).
pub(crate) struct Use {
    first: u8,
    last: Option<u8>,
    depth: Arc<HostTexture>,
}

/// The twin's plane formats for a console depth format (resource or
/// view format): 16- or 32-bit depth, then 8-bit stencil if it has one.
/// `None` for 24-bit depth, whose host plane takes no unordered view.
fn plane_formats(console: u32) -> Option<&'static [u32]> {
    const R16: u32 = 53;
    const R32: u32 = 39;
    const R8: u32 = 60;
    match console {
        53 | 55 => Some(&[R16]),
        39..=41 => Some(&[R32]),
        118..=120 => Some(&[R16, R8]),
        19..=22 => Some(&[R32, R8]),
        _ => None,
    }
}

/// The twin of the depth texture `depth`, whose title resource has format
/// `console`; `None` if the host makes none.
fn make(console: u32, depth: &ID3D12Resource) -> Option<Twin> {
    let host = host()?;
    let Some(formats) = plane_formats(console) else {
        once(console, 0, 0, "a depth texture of this format gets no writable twin: its unordered writes are dropped");
        return None;
    };
    // SAFETY: a live resource.
    let dd = unsafe { depth.GetDesc() };
    let subs = u32::from(dd.MipLevels) * u32::from(dd.DepthOrArraySize).max(1);
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let mut planes = Vec::new();
    for &f in formats {
        let desc = D3D12_RESOURCE_DESC {
            Format: DXGI_FORMAT(f as i32),
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Alignment: 0,
            ..dd
        };
        let mut texture: Option<ID3D12Resource> = None;
        // SAFETY: plain creation; our out pointer.
        let made = unsafe {
            host.device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut texture)
        };
        if let Err(e) = made {
            tracing::warn!(error = %e, ?desc, "no writable twin for a depth texture");
            return None;
        }
        let texture = texture?;
        crate::host::name(&texture, || {
            format!("depth twin {}x{} f{f}", dd.Width, dd.Height)
        });
        planes.push(Arc::new(HostTexture::new(texture, 0, Vec::new())));
    }
    // The depth texture's footprints, every plane, and the twin's.
    let n = subs * planes.len() as u32;
    let mut df = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); n as usize];
    let mut drows = vec![0u32; n as usize];
    // SAFETY: outputs sized for `n` subresources.
    unsafe { host.device.GetCopyableFootprints(&dd, 0, n, 0, Some(df.as_mut_ptr()), Some(drows.as_mut_ptr()), None, None) };
    let mut routes = Vec::new();
    let mut at = 0u64;
    let mut place = |mut f: D3D12_PLACED_SUBRESOURCE_FOOTPRINT, rows: u32| {
        f.Offset = at;
        let bytes = u64::from(f.Footprint.RowPitch) * u64::from(rows) * u64::from(f.Footprint.Depth);
        at = (at + bytes).next_multiple_of(512);
        f
    };
    for (p, plane) in planes.iter().enumerate() {
        // SAFETY: a live resource.
        let td = unsafe { plane.texture.GetDesc() };
        let mut tf = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); subs as usize];
        let mut trows = vec![0u32; subs as usize];
        // SAFETY: outputs sized for `subs` subresources.
        unsafe { host.device.GetCopyableFootprints(&td, 0, subs, 0, Some(tf.as_mut_ptr()), Some(trows.as_mut_ptr()), None, None) };
        for s in 0..subs {
            let d = (p as u32 * subs + s) as usize;
            let same = df[d].Footprint.Format == td.Format;
            // The same bytes: both sides copy through the depth texture's
            // footprint; else each its own, converted between.
            let route = if same {
                Route { plane: p, twin_sub: s, depth_sub: d as u32, twin: place(df[d], drows[d]), depth: None }
            } else {
                let twin = place(tf[s as usize], trows[s as usize]);
                Route { plane: p, twin_sub: s, depth_sub: d as u32, twin, depth: Some(place(df[d], drows[d])) }
            };
            if route.depth.is_some() && (td.Format != DXGI_FORMAT_R16_TYPELESS || df[d].Footprint.Format != DXGI_FORMAT_R32_TYPELESS) {
                once(console, td.Format.0 as u32, df[d].Footprint.Format.0 as u32, "a depth twin plane with no conversion to the host's: not made");
                return None;
            }
            routes.push(route);
        }
    }
    let scratch = scratch_buffer(at)?;
    crate::host::name(&scratch, || format!("depth twin copies {at:#x}"));
    tracing::debug!(console, width = dd.Width, height = dd.Height, planes = planes.len(), "a writable twin for a depth texture");
    Some(Twin { planes, newer: AtomicU8::new(DEPTH), routes, scratch })
}

/// A default-heap buffer of `size` bytes shaders may write, in the common
/// state.
fn scratch_buffer(size: u64) -> Option<ID3D12Resource> {
    let host = host()?;
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size.max(256),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: plain creation.
    let made = unsafe {
        host.device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut buffer)
    };
    made.ok().and(buffer)
}

/// Logs once per (console format, view format, plane) what happened.
fn once(console: u32, view: u32, plane: u32, what: &str) {
    static SEEN: std::sync::Mutex<Vec<(u32, u32, u32, String)>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let key = (console, view, plane, what.to_owned());
    if seen.len() < 64 && !seen.contains(&key) {
        tracing::info!(console, view, plane, "{what}");
        seen.push(key);
    }
}

/// An unordered access view at `at` of `resource`, a depth texture whose
/// host copy is `copy`, made on its twin's plane the title's view names
/// (`words`: the view description), in a format of that plane's family
/// (the title's own if it is one, else the plane's integer or normalised
/// one, as the title's format is). The twin's plane texture, which the
/// slot then views.
pub(crate) fn uav(
    resource: This,
    copy: &Arc<HostTexture>,
    words: Option<&[u32; 10]>,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
) -> Option<ID3D12Resource> {
    let console = crate::resource::title_format(resource)?;
    let twin = copy.twin.get_or_init(|| make(console, &copy.texture)).as_ref()?;
    let mut words = *words?;
    const TEXTURE_2D: u32 = 4;
    const TEXTURE_2D_ARRAY: u32 = 5;
    // The plane: last of the 2D view's fields (mip, plane; mip, first
    // slice, slices, plane).
    let plane_word = match words[1] {
        TEXTURE_2D => 3,
        TEXTURE_2D_ARRAY => 5,
        other => {
            once(console, words[0], other, "an unordered view of a depth texture of this dimension: not made");
            return None;
        }
    };
    let plane = words[plane_word] as usize;
    let Some(texture) = twin.planes.get(plane) else {
        once(console, words[0], plane as u32, "an unordered view of a depth plane the twin does not have: not made");
        return None;
    };
    // SAFETY: a live resource.
    let own = unsafe { texture.texture.GetDesc() }.Format.0 as u32;
    let view = crate::resource::pc_format(words[0]);
    let format = if crate::resource::family(view) == own {
        view
    } else {
        // R16_TYPELESS: unorm or uint; R32_TYPELESS: float or uint; R8: uint.
        match (own, integer(view)) {
            (53, false) => 56,
            (53, true) => 57,
            (39, false) => 41,
            (39, true) => 42,
            _ => 62,
        }
    };
    once(console, words[0], plane as u32, "an unordered view of a depth texture, made on its writable twin");
    words[0] = format;
    words[plane_word] = 0;
    // SAFETY: 40 bytes in the host's layout.
    let desc = unsafe { std::ptr::from_ref(&words).cast::<D3D12_UNORDERED_ACCESS_VIEW_DESC>().read_unaligned() };
    // SAFETY: a live texture and a slot of our heap.
    unsafe { host()?.device.CreateUnorderedAccessView(&texture.texture, None, Some(&desc), at) };
    Some(texture.texture.clone())
}

/// Whether a format holds integers (`_UINT`, `_SINT`).
fn integer(format: u32) -> bool {
    matches!(format, 3 | 4 | 7 | 8 | 12 | 14 | 17 | 18 | 25 | 30 | 32 | 36 | 38 | 42 | 43 | 50 | 52 | 57 | 59 | 62 | 64)
}

/// Before a use in `list` of the depth texture `depth` (with `twin`) in
/// host state `wanted`: the side the use needs is brought up to date (a
/// copy recorded now, or by the list's prelude for its first use). For
/// unordered access the twin's planes are moved there and true returned:
/// the depth texture itself is not used.
pub(crate) fn route(list: &HostList, depth: &Arc<HostTexture>, twin: &Twin, wanted: u32) -> bool {
    let side = if wanted & UNORDERED_ACCESS != 0 { TWIN } else { DEPTH };
    let writes = side == TWIN || wanted & (DEPTH_WRITE | RENDER_TARGET | COPY_DEST | RESOLVE_DEST) != 0;
    if let Some(to_depth) = list.twin_use(depth, side, writes)
        && twin.copy(&list.list, depth, to_depth, &mut |t, w| {
            list.move_texture(t, w)
        })
    {
        list.need_rebind();
    }
    if side == DEPTH {
        return false;
    }
    let barriers: Vec<_> = twin.planes.iter().filter_map(|p| list.move_texture(p, UNORDERED_ACCESS)).collect();
    if !barriers.is_empty() {
        // SAFETY: an open list and live textures.
        unsafe { list.list.ResourceBarrier(&barriers) };
    }
    true
}

impl Use {
    pub(crate) fn new(depth: &Arc<HostTexture>, side: u8, writes: bool) -> Self {
        Use { first: side, last: writes.then_some(side), depth: depth.clone() }
    }

    /// A later use in the list needing `side`: whether a copy must be
    /// recorded first (true: into the depth texture).
    pub(crate) fn again(&mut self, side: u8, writes: bool) -> Option<bool> {
        again(self.first, &mut self.last, side, writes)
    }
}

/// [`Use::again`] on its first side and the side it left newer.
fn again(first: u8, last: &mut Option<u8>, side: u8, writes: bool) -> Option<bool> {
    // Unchanged since the list's start: only the first use's side is
    // known to be current there.
    let current = match *last {
        None => first == side,
        Some(newer) => newer == BOTH || newer == side,
    };
    let copy = (!current).then_some(side == DEPTH);
    if copy.is_some() {
        *last = Some(BOTH);
    }
    if writes {
        *last = Some(side);
    }
    copy
}

/// Where a list's uses leave a twin's newer side, from `newer` before it:
/// the copy its prelude makes first (true: into the depth texture), and the
/// newer side after the list.
fn submitted(first: u8, last: Option<u8>, newer: u8) -> (Option<bool>, u8) {
    let copy = match (first, newer) {
        (DEPTH, TWIN) => Some(true),
        (TWIN, DEPTH) => Some(false),
        _ => None,
    };
    let start = if copy.is_some() { BOTH } else { newer };
    (copy, last.unwrap_or(start))
}

/// At submission, for a list's twin uses in order: the copies its prelude
/// records first (the depth texture and whether into it), the twins'
/// newer sides moved on to where the list leaves them.
pub(crate) fn at_submission<'a>(uses: impl Iterator<Item = &'a Use>) -> Vec<(Arc<HostTexture>, bool)> {
    let mut copies = Vec::new();
    for u in uses {
        let Some(Some(twin)) = u.depth.twin.get() else {
            continue;
        };
        let (to_depth, after) = submitted(u.first, u.last, twin.newer.load(Relaxed));
        twin.newer.store(after, Relaxed);
        if let Some(to_depth) = to_depth {
            copies.push((u.depth.clone(), to_depth));
        }
    }
    copies
}

/// Moves a texture to a state for a copy: the barrier, if any.
pub(crate) type Mover<'a> = dyn FnMut(&Arc<HostTexture>, u32) -> Option<D3D12_RESOURCE_BARRIER> + 'a;

impl Twin {
    /// Records into `l` the copy of the newer side into the other: from
    /// the twin into `depth` (`to_depth`) or back. `mv` moves textures to
    /// the copy states (tracked by the list, or from their real states in
    /// a prelude). True if it ran our conversion, which replaces the
    /// list's compute root signature and pipeline.
    pub(crate) fn copy(&self, l: &ID3D12GraphicsCommandList, depth: &Arc<HostTexture>, to_depth: bool, mv: &mut Mover) -> bool {
        let (src, dst) = if to_depth { (COPY_SOURCE, COPY_DEST) } else { (COPY_DEST, COPY_SOURCE) };
        let mut barriers: Vec<_> = self.planes.iter().filter_map(|p| mv(p, src)).collect();
        barriers.extend(mv(depth, dst));
        let at = |r: &ID3D12Resource, sub: u32| D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the copy does not release.
            pResource: unsafe { std::mem::transmute_copy(r) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: sub },
        };
        let placed = |f: D3D12_PLACED_SUBRESOURCE_FOOTPRINT| D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(&self.scratch) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: f },
        };
        let converts = self.routes.iter().any(|r| r.depth.is_some());
        let state = |from, to| {
            transition(
                &self.scratch,
                D3D12_RESOURCE_STATES(from),
                D3D12_RESOURCE_STATES(to),
            )
        };
        let mut converted = false;
        // SAFETY: an open list; live resources in the states named (the
        // scratch buffer starts common: its first copy promotes it).
        unsafe {
            if !barriers.is_empty() {
                l.ResourceBarrier(&barriers);
            }
            for r in &self.routes {
                let plane = &self.planes[r.plane].texture;
                if to_depth {
                    l.CopyTextureRegion(&placed(r.twin), 0, 0, 0, &at(plane, r.twin_sub), None);
                } else {
                    l.CopyTextureRegion(&placed(r.depth.unwrap_or(r.twin)), 0, 0, 0, &at(&depth.texture, r.depth_sub), None);
                }
            }
            if converts && let Some(c) = converter() {
                l.ResourceBarrier(&[state(COPY_DEST as i32, UNORDERED_ACCESS as i32)]);
                l.SetComputeRootSignature(&c.root);
                l.SetPipelineState(&c.pso);
                l.SetComputeRootUnorderedAccessView(1, self.scratch.GetGPUVirtualAddress());
                for r in &self.routes {
                    let Some(d) = r.depth else { continue };
                    let (t, d) = (r.twin.Footprint, d);
                    let constants = [t.Width, t.Height, t.RowPitch, r.twin.Offset as u32, d.Footprint.RowPitch, d.Offset as u32, u32::from(to_depth), 0];
                    l.SetComputeRoot32BitConstants(0, 8, constants.as_ptr().cast(), 0);
                    l.Dispatch(t.Width.div_ceil(2).div_ceil(64), t.Height, 1);
                }
                l.ResourceBarrier(&[uav_barrier(), state(UNORDERED_ACCESS as i32, COPY_SOURCE as i32)]);
                converted = true;
            } else {
                l.ResourceBarrier(&[state(COPY_DEST as i32, COPY_SOURCE as i32)]);
            }
            for r in &self.routes {
                let plane = &self.planes[r.plane].texture;
                if to_depth {
                    l.CopyTextureRegion(&at(&depth.texture, r.depth_sub), 0, 0, 0, &placed(r.depth.unwrap_or(r.twin)), None);
                } else {
                    l.CopyTextureRegion(&at(plane, r.twin_sub), 0, 0, 0, &placed(r.twin), None);
                }
            }
            l.ResourceBarrier(&[state(COPY_SOURCE as i32, 0)]);
        }
        COPIES.fetch_add(1, Relaxed);
        converted
    }
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

/// 16-bit depth (unorm, two per dword) to the host's 32-bit float depth
/// plane and back, in the scratch buffer: `u` becomes `u` times
/// `0x37800080` (the title's 1/65535; its tiled-lighting draws rasterise
/// that very float and test it EQUAL), and a float back the nearest `u`.
///
/// `depth_bytecode.rs` holds this shader compiled by Microsoft's compilers
/// on a Windows runner (`cargo xtask embed-depth`), because Wine's HLSL
/// compiler (vkd3d-shader) cannot build it; that bytecode is used only
/// while its `SOURCE_HASH` names this text, and the system's compiler
/// builds the shader otherwise, as it always did.
const SOURCE: &str = include_str!("depth.hlsl");

struct Converter {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Converter {}
// SAFETY: as above.
unsafe impl Sync for Converter {}

fn converter() -> Option<&'static Converter> {
    static MADE: std::sync::OnceLock<Option<Converter>> = std::sync::OnceLock::new();
    MADE.get_or_init(|| match compile() {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!(error = %e, "no depth conversion: 16-bit depth written through unordered views does not reach the depth texture");
            None
        }
    })
    .as_ref()
}

/// The hash of the shader source, which names the bytecode it was built
/// into (the hash `cargo xtask embed-depth` writes).
fn source_hash() -> u64 {
    crate::indirect::fnv(SOURCE.as_bytes())
}

/// Whether there is no prebuilt bytecode of the present `depth.hlsl`: none
/// was embedded yet, or it was built from another source. It is not used
/// then: the shader's constants and the scratch layout are this file's,
/// and the bytecode's may differ.
fn stale() -> bool {
    crate::depth_bytecode::SOURCE_HASH != source_hash()
}

fn compile() -> windows::core::Result<Converter> {
    let device = &host().ok_or_else(windows::core::Error::empty)?.device;
    // Prebuilt bytecode first, as long as it was built from this source.
    if stale() {
        tracing::warn!(
            "no prebuilt depth conversion of this depth.hlsl (build it on a Windows \
             runner and run `cargo xtask embed-depth`); compiling the shader here"
        );
    } else {
        for bytes in [crate::depth_bytecode::DXIL, crate::depth_bytecode::DXBC] {
            if bytes.is_empty() {
                continue;
            }
            match from_bytecode(device, bytes) {
                Ok(c) => return Ok(c),
                Err(e) => tracing::warn!(error = %e, "prebuilt depth conversion refused"),
            }
        }
    }
    match compiled_here() {
        Ok(bytes) => from_bytecode(device, &bytes),
        Err((e, text)) => {
            if !text.is_empty() {
                tracing::warn!("depth conversion: {text}");
            }
            Err(e)
        }
    }
}

/// The shader compiled by the system's HLSL compiler, or its error and
/// what the compiler said.
fn compiled_here() -> Result<Vec<u8>, (windows::core::Error, String)> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(
            SOURCE.as_ptr().cast(),
            SOURCE.len(),
            s!("depth"),
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
    // SAFETY: a blob's own bytes, copied while it lives.
    let bytes_of = |blob: &ID3DBlob| unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
            .to_vec()
    };
    if let Err(e) = compiled {
        let text = errors.as_ref().map(bytes_of).unwrap_or_default();
        return Err((e, String::from_utf8_lossy(&text).into_owned()));
    }
    match code {
        Some(code) => Ok(bytes_of(&code)),
        None => Err((windows::core::Error::empty(), String::new())),
    }
}

/// The root signature and pipeline of compiled conversion bytecode, whose
/// root signature is inside it.
fn from_bytecode(device: &ID3D12Device, bytes: &[u8]) -> windows::core::Result<Converter> {
    // SAFETY: the bytes live through both calls.
    unsafe {
        let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
        let pso = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE { pShaderBytecode: bytes.as_ptr().cast(), BytecodeLength: bytes.len() },
            ..Default::default()
        })?;
        Ok(Converter { root, pso })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The material-depth frame of Grand Theft Auto VI: written through
    /// unordered views, read as a shader resource in the same list, written
    /// again; read in the next list; tested as depth in a later one; the
    /// next frame writes it through unordered views again.
    #[test]
    fn copies_follow_the_side_each_use_needs() {
        // List 1: unordered write first (the prelude copies the depth side
        // over, newer since the last frame), a read (copy back now), a
        // write again.
        let mut last = Some(TWIN);
        assert_eq!(again(TWIN, &mut last, DEPTH, false), Some(true));
        assert_eq!(last, Some(BOTH));
        assert_eq!(again(TWIN, &mut last, TWIN, true), None);
        assert_eq!(submitted(TWIN, last, DEPTH), (Some(false), TWIN));
        // List 2 only reads: its prelude copies into the depth texture.
        assert_eq!(submitted(DEPTH, None, TWIN), (Some(true), BOTH));
        // List 3 renders to it: nothing to copy, the depth side is newer.
        assert_eq!(submitted(DEPTH, Some(DEPTH), BOTH), (None, DEPTH));
        // A list that starts reading the depth side, unchanged since, does
        // not know the twin is current: a use of the twin copies.
        let mut last = None;
        assert_eq!(again(DEPTH, &mut last, DEPTH, false), None);
        assert_eq!(again(DEPTH, &mut last, TWIN, true), Some(false));
        assert_eq!(last, Some(TWIN));
    }

    /// Whether there is a depth conversion for `test`, which needs one.
    /// Without one the test did not run: while no bytecode of the present
    /// `depth.hlsl` is embedded and Wine's compiler cannot build the
    /// shader, that is the line `skipped: no prebuilt depth conversion
    /// ... (<test>)`, or a failure with `VICTORY_REQUIRE_DEPTH=1`.
    ///
    /// # Panics
    ///
    /// Without a conversion for any other reason: the prebuilt bytecode
    /// names this source and the host refused it, or the system's compiler
    /// is Microsoft's (the process is not under Wine) and the shader did
    /// not build.
    fn converter_for(test: &str) -> Option<&'static Converter> {
        if let Some(c) = converter() {
            return Some(c);
        }
        assert!(
            stale(),
            "{test}: no depth conversion, though the prebuilt bytecode names this source"
        );
        assert!(
            crate::test_support::under_wine(),
            "{test}: no depth conversion, though the system's compiler is not Wine's"
        );
        crate::test_support::no_depth(test);
        None
    }

    /// The fallback, as upstream has it: the system's compiler builds the
    /// shader with its root signature inside, and the host takes it. Under
    /// Wine that compiler is vkd3d-shader's, which refuses the source
    /// (measured: "E5005: Method 'Load' is not defined on type
    /// 'RWByteAddressBuffer'"): there, and only there, the test says
    /// "skipped" with the compiler's text, or fails with
    /// `VICTORY_REQUIRE_HLSL=1`.
    #[test]
    fn the_systems_compiler_builds_the_depth_conversion() {
        let test = "twin::the_systems_compiler_builds_the_depth_conversion";
        let Some((_gpu, host)) = crate::testing::gpu() else {
            return;
        };
        match compiled_here() {
            Ok(bytes) => {
                assert!(crate::root::holds_a_root_signature(&bytes), "no RTS0 part");
                let made = from_bytecode(&host.device, &bytes).map(|_| ());
                assert_eq!(made, Ok(()));
            }
            Err((e, text)) => {
                let text = format!("{e}: {}", text.trim_end_matches(['\0', '\n', '\r']));
                assert!(crate::test_support::under_wine(), "{test}: {text}");
                crate::test_support::no_hlsl(test, &text);
            }
        }
    }

    /// The model of the shader's `depth`: `u` times the title's 1/65535,
    /// as a float's bits.
    fn depth_bits(u: u32) -> u32 {
        (u as f32 * f32::from_bits(0x3780_0080)).to_bits()
    }

    /// The conversion on the device, recorded as `Twin::copy` records it:
    /// a 5 by 2 plane of 16-bit depths becomes the 32-bit floats of the
    /// same depths (upstream's conversion since 2026-11-25; before it the
    /// host plane was 24-bit and this test asserted `u * (2^24 - 1) /
    /// 65535`), the odd last column leaves the dword after its row alone,
    /// and the way back gives the 16-bit depth nearest to each float,
    /// floats outside 0..1 saturated.
    #[test]
    fn depth_converts_both_ways_on_the_device() {
        let test = "twin::depth_converts_both_ways_on_the_device";
        let Some((_gpu, host)) = crate::testing::gpu() else {
            return;
        };
        let Some(c) = converter_for(test) else {
            return;
        };
        // The title's own constant: 1 becomes it, and the far plane a float
        // within two steps of 1.0 that goes back to 65535.
        assert_eq!((depth_bits(0), depth_bits(1)), (0, 0x3780_0080));
        assert!(1.0f32.to_bits().abs_diff(depth_bits(65535)) <= 2);
        const WIDTH: usize = 5;
        const HEIGHT: usize = 2;
        // Bytes: the 16-bit plane, the float plane it becomes, a float
        // plane of our own and the 16-bit plane that one becomes.
        const PITCH16: usize = 16;
        const PITCH32: usize = 32;
        let (in16, out32, in32, out16) = (0usize, 64usize, 128usize, 192usize);
        const UNTOUCHED: u32 = 0xdead_beef;
        let depths: [u16; WIDTH * HEIGHT] =
            [0, 1, 2, 0x7fff, 0x8000, 0x8001, 0xfffe, 0xffff, 100, 257];
        // Floats on and between the 16-bit depths' own (none near a half,
        // where the rounding of the product would decide), and outside
        // 0..1: the nearest depth is wanted.
        let f = |depth: f32| (depth / 65535.0).to_bits();
        let near: [(u32, u16); WIDTH * HEIGHT] = [
            (0.0f32.to_bits(), 0),
            (1.0f32.to_bits(), 0xffff),
            (f(100.25), 100),
            (f(100.75), 101),
            (depth_bits(0x8000), 0x8000),
            (depth_bits(77), 77),
            (f(76.75), 77),
            (f(65534.25), 0xfffe),
            ((-1.0f32).to_bits(), 0),
            (2.0f32.to_bits(), 0xffff),
        ];
        let words = 256 / 4;
        let (buffer, data) = host.shared_buffer(256).expect("a shared buffer");
        let data = data.cast::<u32>();
        let list = host.new_list().expect("a list");
        // SAFETY: our mapped buffer of 64 dwords, written before the list
        // runs and read after it has; an open list, our pipeline and a
        // live buffer.
        let after: Vec<u32> = unsafe {
            for at in 0..words {
                data.add(at).write_volatile(UNTOUCHED);
            }
            for (i, &depth) in depths.iter().enumerate() {
                let at = in16 + i / WIDTH * PITCH16 + i % WIDTH * 2;
                data.cast::<u16>().add(at / 2).write_volatile(depth);
            }
            for (i, &(value, _)) in near.iter().enumerate() {
                let at = in32 + i / WIDTH * PITCH32 + i % WIDTH * 4;
                data.add(at / 4).write_volatile(value);
            }
            let l = &list.list;
            l.SetComputeRootSignature(&c.root);
            l.SetPipelineState(&c.pso);
            l.SetComputeRootUnorderedAccessView(1, buffer.GetGPUVirtualAddress());
            for (at16, at32, to_depth) in [(in16, out32, 1), (out16, in32, 0)] {
                let constants = [
                    WIDTH as u32,
                    HEIGHT as u32,
                    PITCH16 as u32,
                    at16 as u32,
                    PITCH32 as u32,
                    at32 as u32,
                    to_depth,
                    0,
                ];
                l.SetComputeRoot32BitConstants(0, 8, constants.as_ptr().cast(), 0);
                l.Dispatch((WIDTH as u32).div_ceil(2).div_ceil(64), HEIGHT as u32, 1);
                l.ResourceBarrier(&[uav_barrier()]);
            }
            list.close();
            host.execute(&[&list]);
            (0..words).map(|at| data.add(at).read_volatile()).collect()
        };
        let half = |at: usize| (after[at / 4] >> (at % 4 * 8)) as u16;
        for (i, &depth) in depths.iter().enumerate() {
            let (x, y) = (i % WIDTH, i / WIDTH);
            // The source stays as written.
            assert_eq!(half(in16 + y * PITCH16 + x * 2), depth, "source {i}");
            let got = after[(out32 + y * PITCH32 + x * 4) / 4];
            assert_eq!(got, depth_bits(u32::from(depth)), "depth {depth:#x} at {x}, {y}");
        }
        for (i, &(value, want)) in near.iter().enumerate() {
            let (x, y) = (i % WIDTH, i / WIDTH);
            let got = half(out16 + y * PITCH16 + x * 2);
            assert_eq!(got, want, "{value:#x} at {x}, {y}");
        }
        for y in 0..HEIGHT {
            // Past the odd width: no sixth float, and a zero in the other
            // half of the last pair of 16-bit depths.
            let past = after[(out32 + y * PITCH32 + WIDTH * 4) / 4];
            assert_eq!(past, UNTOUCHED, "row {y}");
            assert_eq!(half(out16 + y * PITCH16 + WIDTH * 2), 0, "row {y}");
        }
    }

    /// A list that writes a depth texture through its twin and then reads
    /// its depth side: its prelude owes a copy into the twin first. Every
    /// barrier the prelude and the list issue starts from the state the
    /// texture is in, as the debug layer checks (run light21: the prelude's
    /// copy started from the states the list leaves).
    #[test]
    fn prelude_copies_start_from_the_real_states() {
        use crate::host::tests::{issued, replay};
        use windows::core::Interface;
        let Some((_gpu, host)) = crate::testing::gpu() else {
            return;
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 64,
            Height: 64,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R16_TYPELESS,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Flags: D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
            ..Default::default()
        };
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
        let mut texture: Option<ID3D12Resource> = None;
        // SAFETY: plain creation; our out pointer.
        unsafe { host.device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_DEPTH_WRITE, None, &mut texture) }.unwrap();
        let depth = Arc::new(HostTexture::new(texture.unwrap(), DEPTH_WRITE, Vec::new()));
        let twin = depth.twin.get_or_init(|| make(53, &depth.texture)).as_ref().unwrap();
        let list = host.new_list().unwrap();
        issued();
        assert!(route(&list, &depth, twin, UNORDERED_ACCESS));
        const PIXEL_SHADER_RESOURCE: u32 = 0x80;
        assert!(!route(&list, &depth, twin, PIXEL_SHADER_RESOURCE));
        if let Some(b) = list.move_texture(&depth, PIXEL_SHADER_RESOURCE) {
            // SAFETY: an open list and a live texture.
            unsafe { list.list.ResourceBarrier(&[b]) };
        }
        list.close();
        let recorded = issued();
        host.execute(&[&list]);
        // The GPU runs the prelude, then the list.
        let mut run = issued();
        run.extend(recorded);
        let key = |t: &HostTexture| t.texture.as_raw() as usize;
        let mut states = std::collections::HashMap::from([(key(&depth), DEPTH_WRITE), (key(&twin.planes[0]), 0)]);
        replay(&run, &mut states).unwrap();
        // What the list left is what the next prelude starts from.
        assert_eq!(states[&key(&depth)], depth.state.load(Relaxed));
        assert_eq!(states[&key(&twin.planes[0])], twin.planes[0].state.load(Relaxed));
    }
}
