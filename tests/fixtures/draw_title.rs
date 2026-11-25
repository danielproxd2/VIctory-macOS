//! A stand-in title that draws one triangle and reads it back, written by
//! us. It knows nothing of VIctory: it gets the console runtime's entry
//! points (`D3D12XboxCreateDevice` and what it imports beside it) and from
//! there on calls the objects it is handed through their method tables, at
//! the offsets a title's compiled code calls them at.
//!
//! One source for every way the tests run it, so that each runs the same
//! calls: `title_draw.dll` (tests/fixtures/title-draw) has it bind the
//! runtime through its import table and is mapped by victory-loader, and
//! the tests of victory-gpu and victory-hle include this file and hand it
//! the entry points themselves.
//!
//! What it does, as a title does it: creates the device, a queue, an
//! allocator and a list; a render target of [`SIDE`] by [`SIDE`] texels
//! (`R8G8B8A8_UNORM`) with a view in a descriptor heap, and a buffer to
//! read it back into; a root signature, serialized by the runtime; and a
//! pipeline from a creation stream holding the two shader containers it
//! was given. Then, frame after frame: begins the frame, resets the list,
//! sets the target, viewport, scissor and topology, clears, draws three
//! vertices, copies the target into the buffer, submits, and reads the
//! buffer. A pipeline is not there the moment it is created (a title's
//! first frames come out without what is still being made), so it goes on
//! until a frame shows something drawn, for at most [`FRAMES`] frames.
//!
//! Every layout and offset here is one the runtime's own code and tests
//! establish (`crates/victory-gpu`); nothing is guessed. What it leaves
//! out because nothing establishes it: a swap chain and presentation.
#![allow(dead_code)]

use std::ffi::c_void;

/// A console graphics object, or a host one (the root signature's blob):
/// a pointer to a pointer to its method table.
pub type Object = *const c_void;
pub type Iid = [u8; 16];
pub type Hresult = i32;

/// `d3d12_xs!D3D12XboxCreateDevice(adapter, parameters, iid, out)`.
pub type CreateDevice =
    unsafe extern "system" fn(*const c_void, *const u32, *const Iid, *mut Object) -> Hresult;

/// `d3d12_xs!D3D12SerializeRootSignature(description, version, blob,
/// errors)`.
pub type SerializeRootSignature =
    unsafe extern "system" fn(*const RootDesc, u32, *mut Object, *mut Object) -> Hresult;

/// What the title imports from the console runtime.
#[derive(Clone, Copy)]
pub struct Runtime {
    pub create_device: CreateDevice,
    pub serialize_root_signature: SerializeRootSignature,
    /// `d3d12_xs!IID_ID3D12Device`, a data import: the address of the id.
    pub device_iid: *const Iid,
}

/// A root signature description, laid out as on PC.
#[repr(C)]
pub struct RootDesc {
    parameter_count: u32,
    parameters: *const c_void,
    sampler_count: u32,
    samplers: *const c_void,
    flags: u32,
}

/// The render target's width and height in texels.
pub const SIDE: usize = 8;
/// The most frames the title draws while nothing shows.
pub const FRAMES: u32 = 240;

/// The steps of a run, by the number [`Report::failed`] gives them.
pub const STEPS: [&str; 16] = [
    "nothing failed",
    "D3D12XboxCreateDevice",
    "CreateCommandQueue (device 0x40)",
    "CreateCommandAllocator (device 0x48)",
    "CreateCommandList (device 0x60)",
    "Close (list 0x48)",
    "CreateCommittedResource for the render target (device 0xd8)",
    "CreateCommittedResource for the read back buffer (device 0xd8)",
    "Map of the read back buffer (resource 0x40)",
    "CreateDescriptorHeap (device 0x70)",
    "D3D12SerializeRootSignature",
    "CreateRootSignature (device 0x80)",
    "CreatePipelineState (device 0x2b0)",
    "Reset (list 0x50)",
    "the frame's token (device 0x280)",
    "no frame showed anything drawn",
];
const NOTHING_DRAWN: u32 = 15;

/// What a run came to.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Report {
    /// 0, or the step of [`STEPS`] the run ended at.
    pub failed: u32,
    /// What that step returned.
    pub hresult: Hresult,
    /// Frames drawn and read back, the one that showed the triangle
    /// included.
    pub frames: u32,
    /// The last frame's texels as read back, row after row from the top:
    /// red, green, blue and alpha of each.
    pub pixels: [[u8; 4]; SIDE * SIDE],
}

impl Report {
    pub const fn new() -> Report {
        Report {
            failed: 0,
            hresult: 0,
            frames: 0,
            pixels: [[0; 4]; SIDE * SIDE],
        }
    }

    /// The texel in column `x` of row `y`.
    pub fn pixel(&self, x: usize, y: usize) -> [u8; 4] {
        self.pixels[y * SIDE + x]
    }
}

/// The method of `object` at `offset` in its table.
///
/// # Safety
/// `object` is a live object whose table has a method of type `F` there.
unsafe fn method<F: Copy>(object: Object, offset: usize) -> F {
    // SAFETY: the caller's object starts with its table's address, and the
    // table holds a function of this type at the offset.
    unsafe {
        let table = *object.cast::<*const u8>();
        *table.add(offset).cast::<F>()
    }
}

/// `IUnknown::Release`.
///
/// # Safety
/// `object` is a live object, or null.
unsafe fn release(object: Object) {
    if !object.is_null() {
        // SAFETY: every object's table starts with IUnknown's three methods.
        unsafe { method::<unsafe extern "system" fn(Object) -> u32>(object, 0x10)(object) };
    }
}

/// The interface ids the title asks for, the same as on PC.
const fn iid(a: u32, b: u16, c: u16, d: [u8; 8]) -> Iid {
    let (a, b, c) = (a.to_le_bytes(), b.to_le_bytes(), c.to_le_bytes());
    [
        a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5], d[6],
        d[7],
    ]
}
const QUEUE: Iid = iid(
    0x0ec870a6,
    0x5d7e,
    0x4c22,
    [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed],
);
const ALLOCATOR: Iid = iid(
    0x6102dee4,
    0xaf59,
    0x4b09,
    [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24],
);
const LIST: Iid = iid(
    0x5b160d0f,
    0xac1b,
    0x4185,
    [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55],
);
const RESOURCE: Iid = iid(
    0x696442be,
    0xa72e,
    0x4059,
    [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
);
const DESCRIPTOR_HEAP: Iid = iid(
    0x8efb471d,
    0x616c,
    0x4f49,
    [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51],
);
const ROOT_SIGNATURE: Iid = iid(
    0xc54a6b66,
    0x72df,
    0x4ee8,
    [0x8b, 0xe5, 0xa9, 0x46, 0xa1, 0x42, 0x92, 0x14],
);
const PIPELINE: Iid = iid(
    0x765a30f3,
    0xf624,
    0x4c6f,
    [0xa8, 0x28, 0xac, 0xe9, 0x48, 0x62, 0x24, 0x45],
);

/// The interface version the runtime's layouts were checked against (2.4).
const INTERFACE_VERSION: u32 = 0x0002_0004;
const R8G8B8A8_UNORM: u32 = 28;
/// Resource states, as on PC.
const RENDER_TARGET: u32 = 0x4;
const COPY_DEST: u32 = 0x400;
const COPY_SOURCE: u32 = 0x800;
/// A read back row's bytes: footprints have pitches of 256 bytes.
const PITCH: usize = 256;

#[repr(C)]
struct HeapProperties {
    kind: u32,
    cpu_pages: u32,
    pool: u32,
    creation_nodes: u32,
    visible_nodes: u32,
}

#[repr(C)]
struct ResourceDesc {
    dimension: u32,
    alignment: u64,
    width: u64,
    height: u32,
    depth_or_array_size: u16,
    mip_levels: u16,
    format: u32,
    sample_count: u32,
    sample_quality: u32,
    layout: u32,
    flags: u32,
}

/// A descriptor heap's description, and after it the two words a newer
/// title passes there for descriptor memory of its own. The runtime reads
/// them wherever the description lies, so they are part of what is passed:
/// zero, for a heap the runtime allocates.
#[repr(C)]
struct DescriptorHeapDesc {
    kind: u32,
    count: u32,
    flags: u32,
    nodes: u32,
    placement: [u64; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Footprint {
    offset: u64,
    format: u32,
    width: u32,
    height: u32,
    depth: u32,
    row_pitch: u32,
}

/// One end of a copy: a texture's subresource (`kind` 0; the index shares
/// the footprint's first word) or a footprint in a buffer (`kind` 1).
#[repr(C)]
struct CopyLocation {
    resource: Object,
    kind: u32,
    footprint: Footprint,
}

/// A transition barrier.
#[repr(C)]
struct Barrier {
    kind: u32,
    flags: u32,
    resource: Object,
    subresource: u32,
    before: u32,
    after: u32,
}

/// A pipeline creation stream: subobjects, each its type and then its
/// description, in the order and at the sizes Grand Theft Auto VI's stream
/// has them (0x2b0 bytes; `crates/victory-gpu/src/pipeline.rs`). Kept in
/// 64-bit words so that the pointers in it are aligned.
struct Stream(Vec<u64>);

impl Stream {
    /// Adds a subobject of type `kind` that is `size` bytes with its type;
    /// where it starts.
    fn add(&mut self, kind: u32, size: usize) -> usize {
        let at = self.0.len() * 8;
        self.0.resize(self.0.len() + size / 8, 0);
        self.put32(at, kind);
        at
    }

    fn put32(&mut self, at: usize, value: u32) {
        let word = &mut self.0[at / 8];
        let shift = 8 * (at % 8);
        *word = *word & !(0xffff_ffff << shift) | u64::from(value) << shift;
    }

    fn put64(&mut self, at: usize, value: u64) {
        self.0[at / 8] = value;
    }

    /// A shader subobject of type `kind` holding `code` (or nothing).
    fn shader(&mut self, kind: u32, code: &[u8]) {
        let at = self.add(kind, 24);
        if !code.is_empty() {
            self.put64(at + 8, code.as_ptr() as u64);
            self.put64(at + 16, code.len() as u64);
        }
    }
}

/// The stream of a pipeline that draws triangles from `vs` and `ps` into
/// one `R8G8B8A8_UNORM` target: no blending, no culling, no depth, no
/// vertex input.
fn pipeline_stream(root: Object, vs: &[u8], ps: &[u8]) -> Stream {
    let mut s = Stream(Vec::new());
    let at = s.add(0, 16); // root signature
    s.put64(at + 8, root as u64);
    s.shader(1, vs);
    s.shader(2, ps);
    s.shader(3, &[]); // domain
    s.shader(4, &[]); // hull
    s.shader(5, &[]); // geometry
    // Blend: per target, blending and logic operations off, the factors
    // and operations of "source replaces", every channel written.
    let at = s.add(8, 336) + 4;
    for target in 0..8 {
        let at = at + 8 + 40 * target;
        for (i, v) in [0, 0, 2, 1, 1, 2, 1, 1, 4, 0xf].into_iter().enumerate() {
            s.put32(at + 4 * i, v);
        }
    }
    let at = s.add(9, 8); // sample mask
    s.put32(at + 4, u32::MAX);
    // Rasterizer: solid, no culling, depth clipping.
    let at = s.add(10, 48) + 4;
    s.put32(at, 3);
    s.put32(at + 4, 1);
    s.put32(at + 24, 1);
    // Depth and stencil (the newer description): both off, with valid
    // functions (always) and operations (keep) all the same.
    let at = s.add(21, 64) + 4;
    s.put32(at + 8, 8);
    for face in [20, 36] {
        for (i, v) in [1, 1, 1, 8].into_iter().enumerate() {
            s.put32(at + face + 4 * i, v);
        }
    }
    let at = s.add(17, 16); // sample description: one sample
    s.put32(at + 4, 1);
    s.add(12, 24); // input layout: no elements
    let at = s.add(14, 8); // topology type: triangles
    s.put32(at + 4, 3);
    let at = s.add(15, 40); // render target formats: one
    s.put32(at + 4, R8G8B8A8_UNORM);
    s.put32(at + 36, 1);
    s.add(16, 8); // depth format: none
    s
}

/// What the title holds between its steps; everything is released when it
/// goes.
struct Title {
    device: Object,
    queue: Object,
    allocator: Object,
    list: Object,
    target: Object,
    buffer: Object,
    heap: Object,
    root: Object,
    pipeline: Object,
}

impl Drop for Title {
    fn drop(&mut self) {
        let Title {
            device,
            queue,
            allocator,
            list,
            target,
            buffer,
            heap,
            root,
            pipeline,
        } = *self;
        for object in [
            list, allocator, pipeline, root, heap, buffer, target, queue, device,
        ] {
            // SAFETY: each is an object the title was handed, or null.
            unsafe { release(object) };
        }
    }
}

/// A step that failed, and what it returned.
type Failed = (u32, Hresult);

fn ok(step: u32, hresult: Hresult) -> Result<(), Failed> {
    if hresult < 0 {
        Err((step, hresult))
    } else {
        Ok(())
    }
}

/// Runs the title: `vs` and `ps` are its shader containers, as it would
/// have loaded them from its files, and `clear` the colour it clears its
/// target to.
///
/// # Safety
/// `runtime` holds the console runtime's entry points and the address of
/// its device interface id.
pub unsafe fn run(runtime: &Runtime, vs: &[u8], ps: &[u8], clear: [f32; 4]) -> Report {
    let mut report = Report::new();
    // SAFETY: the caller's.
    if let Err((step, hresult)) = unsafe { frames(runtime, vs, ps, clear, &mut report) } {
        (report.failed, report.hresult) = (step, hresult);
    }
    report
}

/// # Safety
/// As [`run`].
unsafe fn frames(
    runtime: &Runtime,
    vs: &[u8],
    ps: &[u8],
    clear: [f32; 4],
    report: &mut Report,
) -> Result<(), Failed> {
    type Create = unsafe extern "system" fn(Object, *const u32, *const Iid, *mut Object) -> Hresult;
    type CreateAllocator =
        unsafe extern "system" fn(Object, u32, *const Iid, *mut Object) -> Hresult;
    type CreateList = unsafe extern "system" fn(
        Object,
        u32,
        u32,
        Object,
        Object,
        *const Iid,
        *mut Object,
    ) -> Hresult;
    type CreateResource = unsafe extern "system" fn(
        Object,
        *const HeapProperties,
        u32,
        *const ResourceDesc,
        u32,
        *const c_void,
        *const Iid,
        *mut Object,
    ) -> Hresult;
    type CreateHeap = unsafe extern "system" fn(
        Object,
        *const DescriptorHeapDesc,
        *const Iid,
        *mut Object,
    ) -> Hresult;
    type CreateRoot = unsafe extern "system" fn(
        Object,
        u32,
        *const u8,
        usize,
        *const Iid,
        *mut Object,
    ) -> Hresult;
    type CreateView = unsafe extern "system" fn(Object, Object, *const c_void, u64);
    type CreatePipeline =
        unsafe extern "system" fn(Object, *const [usize; 2], *const Iid, *mut Object) -> Hresult;
    type FrameWait =
        unsafe extern "system" fn(Object, u32, u32, *const c_void, u32, *mut u64) -> Hresult;
    type Plain = unsafe extern "system" fn(Object) -> Hresult;
    type Word = unsafe extern "system" fn(Object) -> u64;
    type Map = unsafe extern "system" fn(Object, u32, *const c_void, *mut *mut u8) -> Hresult;
    type Reset = unsafe extern "system" fn(Object, Object, Object) -> Hresult;
    type SetObject = unsafe extern "system" fn(Object, Object);
    type SetWord = unsafe extern "system" fn(Object, u32);
    type SetMany = unsafe extern "system" fn(Object, u32, *const c_void);
    type Barriers = unsafe extern "system" fn(Object, u32, *const Barrier) -> u32;
    type Targets = unsafe extern "system" fn(Object, u32, *const u64, i32, *const u64);
    type Clear = unsafe extern "system" fn(Object, u64, *const [f32; 4], u32, *const c_void);
    type Draw = unsafe extern "system" fn(Object, u32, u32, u32, u32);
    type Copy = unsafe extern "system" fn(
        Object,
        *const CopyLocation,
        u32,
        u32,
        u32,
        *const CopyLocation,
        *const c_void,
    );
    type Execute = unsafe extern "system" fn(Object, u32, *const Object);

    let null = std::ptr::null();
    let mut t = Title {
        device: null,
        queue: null,
        allocator: null,
        list: null,
        target: null,
        buffer: null,
        heap: null,
        root: null,
        pipeline: null,
    };
    // SAFETY: the runtime's entry points and its objects' methods, each
    // called with the arguments its signature in the runtime has; every
    // pointer passed is to a live local or to memory the runtime handed
    // out.
    unsafe {
        // The device: the interface version starts the parameters.
        let parameters = [INTERFACE_VERSION, 0, 0, 0, 0, 0, 0, 0];
        let hr =
            (runtime.create_device)(null, parameters.as_ptr(), runtime.device_iid, &mut t.device);
        ok(1, hr)?;
        let d = t.device;

        // A direct queue, an allocator and a list, which starts open.
        let queue_desc = [0u32; 4];
        ok(
            2,
            method::<Create>(d, 0x40)(d, queue_desc.as_ptr(), &QUEUE, &mut t.queue),
        )?;
        ok(
            3,
            method::<CreateAllocator>(d, 0x48)(d, 0, &ALLOCATOR, &mut t.allocator),
        )?;
        ok(
            4,
            method::<CreateList>(d, 0x60)(d, 0, 0, t.allocator, null, &LIST, &mut t.list),
        )?;
        ok(5, method::<Plain>(t.list, 0x48)(t.list))?;

        // The render target and the buffer its texels are read back into.
        let heap = |kind| HeapProperties {
            kind,
            cpu_pages: 0,
            pool: 0,
            creation_nodes: 1,
            visible_nodes: 1,
        };
        let texture = ResourceDesc {
            dimension: 3, // a 2D texture
            alignment: 0,
            width: SIDE as u64,
            height: SIDE as u32,
            depth_or_array_size: 1,
            mip_levels: 1,
            format: R8G8B8A8_UNORM,
            sample_count: 1,
            sample_quality: 0,
            layout: 0,
            flags: 1, // a render target
        };
        let bytes = ResourceDesc {
            dimension: 1, // a buffer
            width: (SIDE * PITCH) as u64,
            height: 1,
            format: 0,
            layout: 1, // row major
            flags: 0,
            ..texture
        };
        let create = method::<CreateResource>(d, 0xd8);
        let (default, read_back) = (heap(1), heap(3));
        ok(
            6,
            create(
                d,
                &default,
                0,
                &texture,
                RENDER_TARGET,
                null,
                &RESOURCE,
                &mut t.target,
            ),
        )?;
        ok(
            7,
            create(
                d,
                &read_back,
                0,
                &bytes,
                COPY_DEST,
                null,
                &RESOURCE,
                &mut t.buffer,
            ),
        )?;
        let mut mapped = std::ptr::null_mut();
        ok(
            8,
            method::<Map>(t.buffer, 0x40)(t.buffer, 0, null, &mut mapped),
        )?;
        if mapped.is_null() {
            return Err((8, 0));
        }

        // One render target view, in a heap of its own.
        let heap_desc = DescriptorHeapDesc {
            kind: 2, // render target views
            count: 1,
            flags: 0,
            nodes: 0,
            placement: [0; 2],
        };
        ok(
            9,
            method::<CreateHeap>(d, 0x70)(d, &heap_desc, &DESCRIPTOR_HEAP, &mut t.heap),
        )?;
        let view = method::<Word>(t.heap, 0x48)(t.heap);
        method::<CreateView>(d, 0xa0)(d, t.target, null, view);

        // An empty root signature that allows vertex input, serialized by
        // the runtime (version 1.0); the blob is the host's, with its bytes
        // and their count behind its fourth and fifth methods.
        let root_desc = RootDesc {
            parameter_count: 0,
            parameters: null,
            sampler_count: 0,
            samplers: null,
            flags: 1,
        };
        let (mut blob, mut errors) = (null, null);
        let hr = (runtime.serialize_root_signature)(&root_desc, 1, &mut blob, &mut errors);
        release(errors);
        ok(10, hr)?;
        if blob.is_null() {
            return Err((10, 0));
        }
        let data = method::<unsafe extern "system" fn(Object) -> *const u8>(blob, 0x18)(blob);
        let size = method::<unsafe extern "system" fn(Object) -> usize>(blob, 0x20)(blob);
        let hr = method::<CreateRoot>(d, 0x80)(d, 0, data, size, &ROOT_SIGNATURE, &mut t.root);
        release(blob);
        ok(11, hr)?;

        // The pipeline, from a stream that lives for the call.
        let stream = pipeline_stream(t.root, vs, ps);
        let stream_desc = [stream.0.len() * 8, stream.0.as_ptr() as usize];
        ok(
            12,
            method::<CreatePipeline>(d, 0x2b0)(d, &stream_desc, &PIPELINE, &mut t.pipeline),
        )?;
        drop(stream);

        let side = SIDE as u32;
        let viewport = [0.0f32, 0.0, SIDE as f32, SIDE as f32, 0.0, 1.0];
        let scissor = [0i32, 0, SIDE as i32, SIDE as i32];
        let rows = Footprint {
            offset: 0,
            format: R8G8B8A8_UNORM,
            width: side,
            height: side,
            depth: 1,
            row_pitch: PITCH as u32,
        };
        let from_target = CopyLocation {
            resource: t.target,
            kind: 0,
            footprint: Footprint { offset: 0, ..rows },
        };
        let into_buffer = CopyLocation {
            resource: t.buffer,
            kind: 1,
            footprint: rows,
        };
        let transition = |before, after| Barrier {
            kind: 0,
            flags: 0,
            resource: t.target,
            subresource: u32::MAX,
            before,
            after,
        };
        let (l, q) = (t.list, t.queue);
        for frame in 1..=FRAMES {
            // The frame begins: its origin is polled without waiting for
            // it, and the token is made all the same (flag 2), as Grand
            // Theft Auto VI begins its frames.
            let mut token = 0u64;
            ok(
                14,
                method::<FrameWait>(d, 0x280)(d, 0, 0, null, 2, &mut token),
            )?;
            ok(13, method::<Plain>(t.allocator, 0x40)(t.allocator))?;
            ok(13, method::<Reset>(l, 0x50)(l, t.allocator, t.pipeline))?;
            method::<SetObject>(l, 0x110)(l, t.root); // graphics root signature
            method::<SetObject>(l, 0xc8)(l, t.pipeline);
            method::<Targets>(l, 0x190)(l, 1, &view, 0, null.cast());
            method::<SetMany>(l, 0xa8)(l, 1, viewport.as_ptr().cast());
            method::<SetMany>(l, 0xb0)(l, 1, scissor.as_ptr().cast());
            method::<SetWord>(l, 0xa0)(l, 4); // a triangle list
            method::<Clear>(l, 0x1a0)(l, view, &clear, 0, null);
            method::<Draw>(l, 0x60)(l, 3, 1, 0, 0);
            method::<Barriers>(l, 0xd0)(l, 1, &transition(RENDER_TARGET, COPY_SOURCE));
            method::<Copy>(l, 0x80)(l, &into_buffer, 0, 0, 0, &from_target, null);
            method::<Barriers>(l, 0xd0)(l, 1, &transition(COPY_SOURCE, RENDER_TARGET));
            ok(5, method::<Plain>(l, 0x48)(l))?;
            method::<Execute>(q, 0x50)(q, 1, &l);

            report.frames = frame;
            for y in 0..SIDE {
                let row = mapped.add(y * PITCH).cast::<[u8; 4]>();
                for x in 0..SIDE {
                    report.pixels[y * SIDE + x] = row.add(x).read_volatile();
                }
            }
            // Drawn: a texel below the diagonal differs from one above it.
            if report.pixel(1, SIDE - 2) != report.pixel(SIDE - 2, 1) {
                return Ok(());
            }
        }
    }
    Err((NOTHING_DRAWN, 0))
}
