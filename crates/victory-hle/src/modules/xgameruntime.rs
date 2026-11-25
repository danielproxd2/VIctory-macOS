//! `xgameruntime.dll`: the console game runtime (XGame*, XUser*, XTaskQueue*...).
//!
//! Titles rarely import it. A stub linked into the title loads the DLL with
//! `LoadLibraryExA` (answered with a pseudo handle, see `kernel`), looks up
//! the `*ApiImpl` functions and `XErrorReport` with `GetProcAddress`, and
//! initialises. Each runtime API call then asks `QueryApiImpl` for a COM-style
//! object of its area (provider and interface GUIDs), calls one method and
//! releases it. Until an area is implemented its query fails, and the call
//! returns that error without touching the object. An area's interface id
//! changes when its method table does, so each object answers only the ids
//! whose slots we know. Implemented, as an offline retail console with one
//! signed-in user: task queues and async blocks (`xasync`), the user
//! (`xuser`), system information (`xsystem`), title id, feature checks,
//! invites and events (`xgame`), persistent local storage (`storage`), game
//! saves (`xgamesave`), networking (`xnetworking`), the package
//! (`xpackage`), the store (`xstore`), system dialogs (`xgameui`), speech
//! (`speech`), HDR, URIs, display deferrals and screenshots (`display`).
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::BTreeMap;
use std::ffi::{CStr, c_char, c_void};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{
    display, speech, storage, xasync, xgame, xgamesave, xgameui, xnetworking, xpackage, xstore,
    xsystem, xuser,
};
use crate::{HleModule, hle_export};

pub type HRESULT = i32;
pub const S_OK: HRESULT = 0;
/// `HRESULT_FROM_WIN32(ERROR_NOT_SUPPORTED)`.
pub const NOT_SUPPORTED: HRESULT = 0x8007_0032_u32 as i32;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl Guid {
    pub const fn new(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Guid {
        Guid {
            data1,
            data2,
            data3,
            data4,
        }
    }
}

impl std::fmt::Display for Guid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let d = self.data4;
        write!(
            f,
            "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            self.data1, self.data2, self.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
        )
    }
}

/// A runtime object as title code calls it: its first word points at the
/// method table. Ours are static, so reference counting is a no-op.
#[repr(C)]
pub struct Object {
    vtable: *const *const (),
    /// Named in the log when a title calls a method we lack.
    area: &'static str,
}

// SAFETY: the table is a static array of function addresses.
unsafe impl Sync for Object {}

impl Object {
    pub const fn new<const N: usize>(area: &'static str, methods: &'static Methods<N>) -> Object {
        Object {
            vtable: methods.0.as_ptr(),
            area,
        }
    }
}

/// An object's method table, by slot (`QueryInterface`, `AddRef`,
/// `Release`, then the area's methods from offset 0x18).
pub struct Methods<const N: usize>(pub [*const (); N]);

// SAFETY: function addresses only.
unsafe impl<const N: usize> Sync for Methods<N> {}

/// A (provider, interface id) pair a title may ask for, and our object.
pub type Interface = (Guid, Guid, &'static Object);

const AREAS: &[&[Interface]] = &[
    xasync::INTERFACES,
    xuser::INTERFACES,
    xsystem::INTERFACES,
    xgame::INTERFACES,
    storage::INTERFACES,
    xnetworking::INTERFACES,
    xpackage::INTERFACES,
    display::INTERFACES,
    xgamesave::INTERFACES,
    xstore::INTERFACES,
    xgameui::INTERFACES,
    speech::INTERFACES,
];

/// `AddRef` and `Release`.
pub extern "system" fn add_ref(_: *const Object) -> u32 {
    1
}

/// Calls of [`missing`] slots, by method table and slot.
static MISSING: Mutex<BTreeMap<(usize, usize), u64>> = Mutex::new(BTreeMap::new());

/// A slot nobody has needed yet. Logged with its area the first time it is
/// called (a title may call it every frame) and counted every time. Wrong
/// for a slot that returns `bool` or a value: which slots do is not
/// established for any of them, so all answer `E_NOTIMPL`.
pub extern "system" fn missing<const SLOT: usize>(this: *const Object) -> HRESULT {
    // SAFETY: title code calls methods on objects we handed out.
    let (table, area) = unsafe { ((*this).vtable as usize, (*this).area) };
    let first = {
        let mut calls = MISSING.lock().unwrap_or_else(|e| e.into_inner());
        let count = calls.entry((table, SLOT)).or_insert(0);
        *count = count.saturating_add(1);
        *count == 1
    };
    if first {
        tracing::warn!(
            area,
            slot = format_args!("{:#x}", SLOT * 8),
            "runtime method not implemented (logged once)"
        );
    }
    xasync::E_NOTIMPL
}

/// How often the unimplemented method at index `slot` of `object`'s table
/// has been called.
pub fn missing_calls(object: &Object, slot: usize) -> u64 {
    let calls = MISSING.lock().unwrap_or_else(|e| e.into_inner());
    calls
        .get(&(object.vtable as usize, slot))
        .copied()
        .unwrap_or(0)
}

/// Writes `value` through `out` if it is non-null.
pub fn put<T>(out: *mut T, value: T) -> HRESULT {
    if out.is_null() {
        return xasync::E_INVALIDARG;
    }
    // SAFETY: checked non-null; the title passes room for a `T`.
    unsafe { out.write(value) };
    S_OK
}

/// Copies `text` and a NUL into the title's `size`-byte buffer and reports
/// the bytes used, as the runtime's string getters do.
pub fn put_string(text: &str, size: usize, buffer: *mut u8, used: *mut usize) -> HRESULT {
    let len = text.len() + 1;
    if buffer.is_null() || size < len {
        return xasync::E_NOT_SUFFICIENT_BUFFER;
    }
    // SAFETY: the buffer holds `size` bytes, checked above.
    unsafe {
        buffer.copy_from_nonoverlapping(text.as_ptr(), text.len());
        buffer.add(text.len()).write(0);
    }
    if !used.is_null() {
        // SAFETY: checked non-null.
        unsafe { used.write(len) };
    }
    S_OK
}

/// The title's NUL-terminated string, if any.
pub fn text(s: *const c_char) -> Option<String> {
    // SAFETY: the title passes a NUL-terminated string or null.
    (!s.is_null()).then(|| unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned())
}

/// A new registration token for callbacks we never call (events nobody
/// raises offline).
pub fn token() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Calls method `slot` (byte offset) of `object` as title code would.
#[cfg(test)]
pub(crate) fn method<F: Copy>(object: &Object, slot: usize) -> F {
    // SAFETY: the caller names the slot's signature; tables cover the slots
    // tests use.
    unsafe { std::mem::transmute_copy(&*object.vtable.add(slot / 8)) }
}

pub extern "system" fn XGameRuntimeInitialize() -> HRESULT {
    tracing::info!("XGameRuntimeInitialize");
    S_OK
}

pub extern "system" fn XGameRuntimeUninitialize() {
    tracing::info!("XGameRuntimeUninitialize");
}

/// The arguments identify the title's build of the stub; not checked.
pub extern "system" fn InitializeApiImpl(version: u64, build: u64) -> HRESULT {
    tracing::info!(version, build, "InitializeApiImpl");
    S_OK
}

pub extern "system" fn InitializeApiImplEx(version: u64, build: u64, flags: u64) -> HRESULT {
    tracing::info!(version, build, flags, "InitializeApiImplEx");
    S_OK
}

/// The stub prefers this one when present. It passes the same three values
/// and a pointer to the title's initialisation options, null unless the
/// title initialises with options (Grand Theft Auto VI does not).
pub extern "system" fn InitializeApiImplEx2(
    version: u64,
    build: u64,
    flags: u64,
    options: *const c_void,
) -> HRESULT {
    tracing::info!(version, build, flags, ?options, "InitializeApiImplEx2");
    S_OK
}

pub extern "system" fn UninitializeApiImpl() {
    tracing::info!("UninitializeApiImpl");
}

/// The object for an implemented (provider, interface id); others fail,
/// each pair logged once.
pub extern "system" fn QueryApiImpl(
    provider: *const Guid,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    static QUERIED: Mutex<Vec<(Guid, Guid)>> = Mutex::new(Vec::new());
    // SAFETY: the title passes pointers to GUIDs (null is tolerated).
    let (Some(&provider), Some(&iid)) = (unsafe { provider.as_ref() }, unsafe { iid.as_ref() })
    else {
        return NOT_SUPPORTED;
    };
    #[cfg(feature = "diagnostics")]
    if tracing::enabled!(tracing::Level::DEBUG) {
        // Each runtime call site once: which title code uses which area, when.
        static CALLS: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let from = super::kernel::callers();
        let mut calls = CALLS.lock().unwrap_or_else(|e| e.into_inner());
        if !calls.contains(&from) {
            tracing::debug!(%provider, %iid, %from, "runtime call");
            calls.push(from);
        }
    }
    let found = AREAS
        .iter()
        .flat_map(|area| area.iter())
        .find(|(p, i, _)| (*p, *i) == (provider, iid));
    if let Some(&(_, _, object)) = found
        && !out.is_null()
    {
        // SAFETY: checked non-null; the title passes room for a pointer.
        unsafe { out.write((object as *const Object).cast_mut().cast()) };
        return S_OK;
    }
    let mut queried = QUERIED.lock().unwrap_or_else(|e| e.into_inner());
    if !queried.contains(&(provider, iid)) {
        queried.push((provider, iid));
        tracing::warn!(%provider, %iid, "runtime API area not implemented");
    }
    NOT_SUPPORTED
}

pub extern "system" fn XErrorReport(hr: HRESULT, message: *const c_char) {
    let message = if message.is_null() {
        "".into()
    } else {
        // SAFETY: a non-null message is a NUL-terminated string.
        unsafe { CStr::from_ptr(message) }.to_string_lossy()
    };
    tracing::warn!(hr = format_args!("{:#010x}", hr as u32), %message, "XErrorReport");
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "xgameruntime",
        dlls: &["xgameruntime.dll"],
        exports: vec![
            hle_export!(XGameRuntimeInitialize),
            hle_export!(XGameRuntimeUninitialize),
            hle_export!(InitializeApiImpl),
            hle_export!(InitializeApiImplEx),
            hle_export!(InitializeApiImplEx2),
            hle_export!(UninitializeApiImpl),
            hle_export!(QueryApiImpl),
            hle_export!(XErrorReport),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::kernel::{FreeLibrary, GetProcAddress, LoadLibraryExA};

    /// Counts the events this module logs on the current thread.
    struct Events(std::sync::Arc<AtomicU64>);

    impl tracing::Subscriber for Events {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            if event.metadata().target().ends_with("xgameruntime") {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// Runs `f` and counts what this module logs meanwhile on this thread.
    fn logged_by<R>(f: impl FnOnce() -> R) -> (u64, R) {
        let count = || std::sync::Arc::new(AtomicU64::new(0));
        // A second subscriber, kept alive: while only one exists, tracing
        // decides whether a callsite is of interest by the subscriber of
        // the thread that reaches it first, and the threads of other tests
        // have none.
        let _other = tracing::Dispatch::new(Events(count()));
        let logged = count();
        let result = tracing::subscriber::with_default(Events(logged.clone()), f);
        (logged.load(Ordering::SeqCst), result)
    }

    /// A title that calls a method we lack every frame gets one log line,
    /// not one per call; the calls are still counted.
    #[test]
    fn missing_slots_log_once_and_count_every_call() {
        static TABLE: Methods<5> = Methods([
            missing::<0> as *const (),
            add_ref as *const (),
            add_ref as *const (),
            missing::<3> as *const (),
            missing::<4> as *const (),
        ]);
        static OBJECT: Object = Object::new("test area", &TABLE);
        type Method = extern "system" fn(*const Object) -> HRESULT;
        let (logged, results) = logged_by(|| {
            let call = method::<Method>(&OBJECT, 0x18);
            (0..1000).map(|_| call(&OBJECT)).collect::<Vec<_>>()
        });
        assert!(results.iter().all(|hr| *hr == xasync::E_NOTIMPL));
        assert_eq!(logged, 1);
        assert_eq!(missing_calls(&OBJECT, 3), 1000);
        // Another slot of the same table is logged and counted for itself.
        assert_eq!(missing_calls(&OBJECT, 4), 0);
        let (logged, other) = logged_by(|| method::<Method>(&OBJECT, 0x20)(&OBJECT));
        assert_eq!((logged, other), (1, xasync::E_NOTIMPL));
        assert_eq!(
            (missing_calls(&OBJECT, 3), missing_calls(&OBJECT, 4)),
            (1000, 1)
        );
    }

    /// Which unimplemented slots return `bool`, a value or nothing is not
    /// established, so every one of them answers `E_NOTIMPL` as an HRESULT.
    /// Pinned here; a slot whose signature becomes known gets a method.
    #[test]
    fn unimplemented_slots_answer_not_implemented() {
        type Method = extern "system" fn(*const Object) -> HRESULT;
        for slot in [0x40, 0xa0, 0xa8, 0xd8, 0xe0, 0xf0] {
            let call = method::<Method>(&xasync::OBJECT, slot);
            assert_eq!(call(&xasync::OBJECT), xasync::E_NOTIMPL, "{slot:#x}");
        }
    }

    #[test]
    fn title_stub_loads_the_runtime_dynamically() {
        let load = |name: &CStr| LoadLibraryExA(name.as_ptr().cast(), std::ptr::null_mut(), 0x800);
        let runtime = load(c"xgameruntime.dll");
        assert!(!runtime.is_null());
        let proc = |name: &CStr| GetProcAddress(runtime, name.as_ptr().cast());
        for name in [
            c"InitializeApiImpl",
            c"InitializeApiImplEx",
            c"InitializeApiImplEx2",
            c"QueryApiImpl",
            c"UninitializeApiImpl",
            c"XErrorReport",
        ] {
            assert!(!proc(name).is_null(), "{name:?}");
        }
        assert!(proc(c"NotAnExport").is_null());
        let ordinal = std::ptr::without_provenance::<u8>(1);
        assert!(GetProcAddress(runtime, ordinal).is_null());

        type Init = extern "system" fn(u64, u64, u64) -> HRESULT;
        type Query = extern "system" fn(*const Guid, *const Guid, *mut *mut c_void) -> HRESULT;
        // SAFETY: the addresses are our functions with these signatures.
        let (init, query) = unsafe {
            (
                std::mem::transmute::<*mut c_void, Init>(proc(c"InitializeApiImplEx")),
                std::mem::transmute::<*mut c_void, Query>(proc(c"QueryApiImpl")),
            )
        };
        assert_eq!(init(1, 2, 2), S_OK);
        let guid = Guid {
            data1: 1,
            data2: 2,
            data3: 3,
            data4: [4; 8],
        };
        let untouched = 0x1234 as *mut c_void;
        let mut out = untouched;
        assert!(query(&guid, &guid, &mut out) < 0);
        assert_eq!(out, untouched);
        assert_eq!(FreeLibrary(runtime), 1);

        #[cfg(windows)]
        {
            let kernel32 = load(c"kernel32.dll");
            assert!(!kernel32.is_null(), "host DLLs still load from the host");
            assert!(!GetProcAddress(kernel32, c"GetTickCount".as_ptr().cast()).is_null());
        }
    }
}
