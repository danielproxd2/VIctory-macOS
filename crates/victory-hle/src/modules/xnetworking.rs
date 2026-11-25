//! XNetworking as an offline console answers it: the network stack is up
//! (titles wait for that before anything else) but reaches nothing, the
//! connectivity hint and the preferred UDP port never change, and the
//! title's security allow list, which comes from the service, cannot be
//! fetched.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::HashMap;
use std::ffi::{c_char, c_void};
use std::sync::{Condvar, Mutex, MutexGuard};

use super::xasync::{self, AsyncBlock, E_INVALIDARG, Queue};
use super::xgameruntime::{
    Guid, HRESULT, Interface, Methods, Object, add_ref, put, query_interface, text, token,
};

pub const PROVIDER: Guid = Guid::new(
    0x37e5_6907,
    0x2f10,
    0x41e8,
    [0xb7, 0x2f, 0x36, 0xed, 0xb1, 0x85, 0x33, 0x1a],
);

/// As Grand Theft Auto VI (2026) asks for it; the slots it calls (0x18-0x88)
/// take the same arguments as under the provider's own id (older titles).
pub const IID_2026: Guid = Guid::new(
    0xbf23_46b2,
    0x39af,
    0x4658,
    [0xb5, 0xea, 0x44, 0x71, 0x3c, 0x7e, 0x83, 0xb3],
);

pub static OBJECT: Object = Object::new("networking", &VTABLE);
pub const INTERFACES: &[Interface] =
    &[(PROVIDER, PROVIDER, &OBJECT), (PROVIDER, IID_2026, &OBJECT)];

/// `HRESULT_FROM_WIN32(ERROR_NETWORK_UNREACHABLE)`: what needs the service.
pub const UNREACHABLE: HRESULT = 0x8007_04CF_u32 as i32;

/// The documented default for the preferred local UDP multiplayer port.
const UDP_PORT: u16 = 3074;

/// The connectivity hint: level, cost, interface type, then four flags.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectivityHint {
    pub level: u32,
    pub cost: u32,
    pub interface_type: u32,
    pub initialized: bool,
    pub approaching_data_limit: bool,
    pub over_data_limit: bool,
    pub roaming: bool,
}

/// Initialized, no connectivity (level 1), cost unknown (0).
const OFFLINE: ConnectivityHint = ConnectivityHint {
    level: 1,
    cost: 0,
    interface_type: 0,
    initialized: true,
    approaching_data_limit: false,
    over_data_limit: false,
    roaming: false,
};

type This = *const Object;

extern "system" fn udp_port(_: This, out: *mut u16) -> HRESULT {
    put(out, UDP_PORT)
}

/// Tells our port queries apart from other calls on the same block.
static PORT: u8 = 0;

extern "system" fn udp_port_async(_: This, block: *mut AsyncBlock) -> HRESULT {
    xasync::finished_with(block, (&raw const PORT).cast(), Ok(xasync::bytes(UDP_PORT)))
}
extern "system" fn udp_port_result(_: This, block: *mut AsyncBlock, out: *mut u16) -> HRESULT {
    xasync::result(block, (&raw const PORT).cast(), out)
}

type PortCallback = extern "system" fn(context: *mut c_void, port: u16);

/// A registration for changes of the preferred port, until its one
/// notification has run.
struct PortNotice {
    context: usize,
    callback: usize,
    /// The thread its callback is running on.
    running: Option<std::thread::ThreadId>,
}

/// The registrations whose notification is still to run, by token.
static NOTICES: Mutex<Option<HashMap<u64, PortNotice>>> = Mutex::new(None);
/// Signalled when a notification has run.
static NOTIFIED: Condvar = Condvar::new();

fn notices() -> MutexGuard<'static, Option<HashMap<u64, PortNotice>>> {
    NOTICES.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the queue runs for a registration: its callback with the port,
/// unless it was unregistered meanwhile or the queue is going.
extern "system" fn notify_port(token: *mut c_void, canceled: bool) {
    let token = token as u64;
    let call = {
        let mut notices = notices();
        let notices = notices.get_or_insert_default();
        if canceled {
            notices.remove(&token);
        }
        notices.get_mut(&token).map(|notice| {
            notice.running = Some(std::thread::current().id());
            (notice.context, notice.callback)
        })
    };
    if let Some((context, callback)) = call {
        // SAFETY: stored from a `PortCallback` in `register_port`.
        let callback: PortCallback = unsafe { std::mem::transmute(callback) };
        callback(context as *mut c_void, UDP_PORT);
        notices().get_or_insert_default().remove(&token);
        NOTIFIED.notify_all();
    }
}

/// `XNetworkingRegisterPreferredLocalUdpMultiplayerPortChanged`. As
/// documented, registering "always fires an initial notification
/// callback": the callback is submitted once, with the port, to the
/// completion side of the queue (null: the process queue). The port never
/// changes after that, so the callback never runs again.
extern "system" fn register_port(
    _: This,
    queue: *const Queue,
    context: *mut c_void,
    callback: Option<PortCallback>,
    out: *mut u64,
) -> HRESULT {
    let Some(callback) = callback else {
        return E_INVALIDARG;
    };
    if out.is_null() {
        return E_INVALIDARG;
    }
    let id = token();
    let notice = PortNotice {
        context: context as usize,
        callback: callback as usize,
        running: None,
    };
    notices().get_or_insert_default().insert(id, notice);
    let submitted = xasync::XTaskQueueSubmitDelayedCallback(
        queue,
        xasync::COMPLETION,
        0,
        id as *mut c_void,
        Some(notify_port),
    );
    if submitted < 0 {
        notices().get_or_insert_default().remove(&id);
        return submitted;
    }
    put(out, id)
}

/// `XNetworkingUnregisterPreferredLocalUdpMultiplayerPortChanged`. As
/// documented: the callback is removed; true unless it is executing, in
/// which case this waits for it (`wait`) or returns false, and the token
/// is unregistered when the callback completes. Calling it with `wait`
/// from inside the callback is documented to deadlock; here it returns
/// false instead.
extern "system" fn unregister_port(_: This, token: u64, wait: bool) -> bool {
    let me = std::thread::current().id();
    let mut notices = notices();
    loop {
        let table = notices.get_or_insert_default();
        match table.get(&token).map(|notice| notice.running) {
            None => return true,
            Some(None) => {
                table.remove(&token);
                return true;
            }
            Some(Some(thread)) if !wait || thread == me => return false,
            Some(Some(_)) => notices = NOTIFIED.wait(notices).unwrap_or_else(|e| e.into_inner()),
        }
    }
}

static SECURITY: u8 = 0;

/// The allow list comes from the service: unreachable offline.
extern "system" fn security_info_async(
    _: This,
    url: *const c_char,
    block: *mut AsyncBlock,
) -> HRESULT {
    tracing::debug!(
        url = text(url).unwrap_or_default(),
        "security information: offline"
    );
    xasync::finished(block, (&raw const SECURITY).cast(), UNREACHABLE)
}
/// `XNetworkingQuerySecurityInformationForUrlUtf16Async`: the same query
/// for a UTF-16 URL, with the same answer. Its result functions take what
/// the UTF-8 ones take.
extern "system" fn security_info_wide_async(
    _: This,
    url: *const u16,
    block: *mut AsyncBlock,
) -> HRESULT {
    tracing::debug!(url = wide(url), "security information: offline");
    xasync::finished(block, (&raw const SECURITY).cast(), UNREACHABLE)
}

/// The NUL-terminated UTF-16 text at `text`; empty for null.
fn wide(text: *const u16) -> String {
    if text.is_null() {
        return String::new();
    }
    // SAFETY: the title passes a NUL-terminated string.
    unsafe {
        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
    }
}

extern "system" fn security_info_size(_: This, block: *mut AsyncBlock, out: *mut usize) -> HRESULT {
    xasync::XAsyncGetResultSize(block, out)
}
extern "system" fn security_info_result(
    _: This,
    block: *mut AsyncBlock,
    size: usize,
    used: *mut usize,
    buffer: *mut u8,
    _info: *mut *const c_void,
) -> HRESULT {
    xasync::XAsyncGetResult(
        block,
        (&raw const SECURITY).cast(),
        size,
        buffer.cast(),
        used,
    )
}

/// No security information can exist offline, so no certificate passes.
extern "system" fn verify_certificate(
    _: This,
    _request: *mut c_void,
    _info: *const c_void,
) -> HRESULT {
    UNREACHABLE
}

extern "system" fn connectivity_hint(_: This, out: *mut ConnectivityHint) -> HRESULT {
    put(out, OFFLINE)
}

/// The hint never changes offline, so the callback never runs.
extern "system" fn register(
    _: This,
    _queue: *const c_void,
    _context: *mut c_void,
    _callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    put(out, token())
}
extern "system" fn unregister(_: This, _token: u64, _wait: bool) -> bool {
    true
}

static VTABLE: Methods<18> = Methods([
    query_interface as *const (),
    add_ref as *const (),
    add_ref as *const (),
    udp_port as *const (),                 // 0x18
    udp_port_async as *const (),           // 0x20
    udp_port_result as *const (),          // 0x28
    register_port as *const (),            // 0x30 port changed (one notification)
    unregister_port as *const (),          // 0x38
    security_info_async as *const (),      // 0x40
    security_info_size as *const (),       // 0x48
    security_info_result as *const (),     // 0x50
    security_info_wide_async as *const (), // 0x58 the same for UTF-16 URLs
    security_info_size as *const (),       // 0x60
    security_info_result as *const (),     // 0x68
    verify_certificate as *const (),       // 0x70
    connectivity_hint as *const (),        // 0x78
    register as *const (),                 // 0x80 hint changed (never)
    unregister as *const (),               // 0x88
]);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::xgameruntime::{S_OK, method};

    fn block() -> AsyncBlock {
        AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        }
    }

    #[test]
    fn offline_but_initialized() {
        let this: This = &OBJECT;
        type Hint = extern "system" fn(This, *mut ConnectivityHint) -> HRESULT;
        let mut hint = OFFLINE;
        hint.initialized = false;
        assert_eq!(method::<Hint>(&OBJECT, 0x78)(this, &mut hint), S_OK);
        assert!(hint.initialized && hint.level == 1);
        assert_eq!(size_of::<ConnectivityHint>(), 16);

        type Begin = extern "system" fn(This, *mut AsyncBlock) -> HRESULT;
        type Port = extern "system" fn(This, *mut AsyncBlock, *mut u16) -> HRESULT;
        let mut b = block();
        assert_eq!(method::<Begin>(&OBJECT, 0x20)(this, &mut b), S_OK);
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), S_OK);
        let mut port = 0;
        assert_eq!(method::<Port>(&OBJECT, 0x28)(this, &mut b, &mut port), S_OK);
        assert_eq!(port, 3074);

        type Url = extern "system" fn(This, *const c_char, *mut AsyncBlock) -> HRESULT;
        type Size = extern "system" fn(This, *mut AsyncBlock, *mut usize) -> HRESULT;
        let mut b = block();
        let url = c"https://example.invalid/";
        assert_eq!(
            method::<Url>(&OBJECT, 0x40)(this, url.as_ptr(), &mut b),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), UNREACHABLE);
        let mut size = 0;
        assert_eq!(
            method::<Size>(&OBJECT, 0x48)(this, &mut b, &mut size),
            UNREACHABLE
        );
    }

    use std::sync::atomic::{AtomicU32, Ordering};

    type Register = extern "system" fn(
        This,
        *const Queue,
        *mut c_void,
        Option<PortCallback>,
        *mut u64,
    ) -> HRESULT;
    type Unregister = extern "system" fn(This, u64, bool) -> bool;

    /// What a port callback saw: its calls, the port of the last one, and
    /// what unregistering its own token from inside it answered.
    #[derive(Default)]
    struct Seen {
        calls: AtomicU32,
        port: AtomicU32,
        token: std::sync::atomic::AtomicU64,
        unregistered_inside: Mutex<Option<bool>>,
    }

    extern "system" fn port_changed(context: *mut c_void, port: u16) {
        // SAFETY: the test's `Seen`, which outlives the registration.
        let seen = unsafe { &*context.cast::<Seen>() };
        seen.port.store(port.into(), Ordering::SeqCst);
        seen.calls.fetch_add(1, Ordering::SeqCst);
        let token = seen.token.load(Ordering::SeqCst);
        if token != 0 {
            let answer = method::<Unregister>(&OBJECT, 0x38)(&OBJECT, token, true);
            *seen.unregistered_inside.lock().unwrap() = Some(answer);
        }
    }

    /// A queue whose ports the test dispatches itself.
    fn manual_queue() -> *const Queue {
        let mut queue = std::ptr::null();
        assert_eq!(xasync::XTaskQueueCreate(0, 0, &mut queue), S_OK);
        queue
    }

    fn register(queue: *const Queue, seen: &Seen) -> u64 {
        let mut token = 0;
        let context = std::ptr::from_ref(seen).cast_mut().cast();
        let callback: PortCallback = port_changed;
        let register = method::<Register>(&OBJECT, 0x30);
        assert_eq!(
            register(&OBJECT, queue, context, Some(callback), &mut token),
            S_OK
        );
        assert_ne!(token, 0);
        token
    }

    /// Registering fires the documented initial notification, with the
    /// port, on the completion side of the queue; nothing follows it.
    #[test]
    fn port_registration_notifies_once_with_the_port() {
        let this: This = &OBJECT;
        let unregister = method::<Unregister>(&OBJECT, 0x38);
        let (queue, seen) = (manual_queue(), Seen::default());
        let token = register(queue, &seen);
        assert_eq!(
            seen.calls.load(Ordering::SeqCst),
            0,
            "submitted, not called"
        );
        assert!(
            !xasync::XTaskQueueDispatch(queue, 0, 0),
            "not the work side"
        );
        assert!(xasync::XTaskQueueDispatch(queue, xasync::COMPLETION, 0));
        assert_eq!(
            (
                seen.calls.load(Ordering::SeqCst),
                seen.port.load(Ordering::SeqCst)
            ),
            (1, 3074)
        );
        assert!(!xasync::XTaskQueueDispatch(queue, xasync::COMPLETION, 50));
        assert!(!xasync::XTaskQueueDispatch(queue, 0, 0));
        assert_eq!(seen.calls.load(Ordering::SeqCst), 1);
        // Nothing of it is executing: unregistering succeeds, any number
        // of times, as for a token that names nothing.
        assert!(unregister(this, token, false));
        assert!(unregister(this, token, true));
        assert!(unregister(this, u64::MAX, true));

        // The same port the direct query gives.
        let mut port = 0;
        type Port = extern "system" fn(This, *mut u16) -> HRESULT;
        assert_eq!(method::<Port>(&OBJECT, 0x18)(this, &mut port), S_OK);
        assert_eq!(u32::from(port), seen.port.load(Ordering::SeqCst));
        xasync::XTaskQueueCloseHandle(queue);
    }

    #[test]
    fn an_unregistered_port_callback_does_not_run() {
        let this: This = &OBJECT;
        let unregister = method::<Unregister>(&OBJECT, 0x38);
        let (queue, seen) = (manual_queue(), Seen::default());
        let token = register(queue, &seen);
        assert!(unregister(this, token, false));
        // What was submitted for it runs without calling the title.
        xasync::XTaskQueueDispatch(queue, xasync::COMPLETION, 0);
        assert_eq!(seen.calls.load(Ordering::SeqCst), 0);

        // While its callback executes, unregistering answers false (also
        // with `wait`, which the console is documented to deadlock on)
        // and the token is gone once the callback has completed.
        let inside = Seen::default();
        let token = register(queue, &inside);
        inside.token.store(token, Ordering::SeqCst);
        assert!(xasync::XTaskQueueDispatch(queue, xasync::COMPLETION, 0));
        assert_eq!(inside.calls.load(Ordering::SeqCst), 1);
        assert_eq!(*inside.unregistered_inside.lock().unwrap(), Some(false));
        assert!(unregister(this, token, true));

        // No callback, no place for the token, a queue that takes nothing.
        let register_slot = method::<Register>(&OBJECT, 0x30);
        let context = std::ptr::from_ref(&seen).cast_mut().cast();
        let callback: PortCallback = port_changed;
        let mut refused = 0;
        assert_eq!(
            register_slot(this, queue, context, None, &mut refused),
            E_INVALIDARG
        );
        let nowhere = std::ptr::null_mut();
        assert_eq!(
            register_slot(this, queue, context, Some(callback), nowhere),
            E_INVALIDARG
        );
        assert_eq!(
            xasync::XTaskQueueTerminate(queue, false, nowhere.cast(), None),
            S_OK
        );
        assert_eq!(
            register_slot(this, queue, context, Some(callback), &mut refused),
            xasync::E_ABORT
        );
        assert_eq!((refused, seen.calls.load(Ordering::SeqCst)), (0, 0));
        xasync::XTaskQueueCloseHandle(queue);
    }

    /// The UTF-16 query and its result functions answer as the UTF-8 ones.
    #[test]
    fn wide_security_info_matches_narrow() {
        let this: This = &OBJECT;
        type Narrow = extern "system" fn(This, *const c_char, *mut AsyncBlock) -> HRESULT;
        type Wide = extern "system" fn(This, *const u16, *mut AsyncBlock) -> HRESULT;
        type Size = extern "system" fn(This, *mut AsyncBlock, *mut usize) -> HRESULT;
        type Result = extern "system" fn(
            This,
            *mut AsyncBlock,
            usize,
            *mut usize,
            *mut u8,
            *mut *const c_void,
        ) -> HRESULT;
        let answers = |begun: HRESULT, b: &mut AsyncBlock, size_slot: usize, result_slot: usize| {
            let status = xasync::XAsyncGetStatus(b, true);
            let (mut size, mut used, mut buffer) = (7usize, 7usize, [0u8; 8]);
            let mut info = std::ptr::null();
            let sized = method::<Size>(&OBJECT, size_slot)(this, b, &mut size);
            let result = method::<Result>(&OBJECT, result_slot);
            let got = result(
                this,
                b,
                buffer.len(),
                &mut used,
                buffer.as_mut_ptr(),
                &mut info,
            );
            (begun, status, sized, got, info.is_null())
        };
        let mut b = block();
        let begun =
            method::<Narrow>(&OBJECT, 0x40)(this, c"https://example.invalid/".as_ptr(), &mut b);
        let narrow = answers(begun, &mut b, 0x48, 0x50);
        let url: Vec<u16> = "https://example.invalid/\0".encode_utf16().collect();
        let mut b = block();
        let begun = method::<Wide>(&OBJECT, 0x58)(this, url.as_ptr(), &mut b);
        let wide = answers(begun, &mut b, 0x60, 0x68);
        assert_eq!(wide, narrow);
        assert_eq!(wide, (S_OK, UNREACHABLE, UNREACHABLE, UNREACHABLE, true));
        // A null URL is logged as empty and answered the same.
        let mut b = block();
        assert_eq!(
            method::<Wide>(&OBJECT, 0x58)(this, std::ptr::null(), &mut b),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), UNREACHABLE);
    }
}
