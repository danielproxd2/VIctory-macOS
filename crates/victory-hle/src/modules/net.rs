//! A console without a network cable: the title reaches loopback only.
//!
//! What is vetted, and nothing else is claimed: the functions below, by
//! the names a title imports from the DLLs [`HostPolicy`] passes through
//! (`ws2_32.dll`, `winhttp.dll`, `iphlpapi.dll`, `crypt32.dll` and the
//! `api-ms-win-core-winrt-l1` API set). The rest of those DLLs is the
//! host's own.
//!
//! * Connects (`connect`, `WSAConnect`, by name, by list, `WSAJoinLeaf`,
//!   `ConnectEx` from `WSAIoctl`) and datagram sends (`sendto`,
//!   `WSASendTo`, `WSASendMsg`, also from `WSAIoctl`) to anything but
//!   127.0.0.0/8 and ::1 fail with `WSAENETUNREACH`. The multi-pointer
//!   `WSAIoctl` query, which would hand out Registered I/O, is refused.
//! * Forward lookups other than `localhost` and IP literals fail with
//!   `WSAHOST_NOT_FOUND`; reverse lookups of other addresses come back
//!   numeric, or fail if a name is required, as with no network.
//!   Name-space queries (`WSALookupServiceBegin`) find nothing.
//! * `WinHttpConnect` to a host that is not loopback fails with
//!   `ERROR_WINHTTP_CANNOT_CONNECT`, so no request (or WebSocket) handle can
//!   exist for one. Proxies are off and proxy auto-detection (WPAD, PAC
//!   downloads) fails with `ERROR_WINHTTP_AUTODETECTION_FAILED`.
//! * A bind to all interfaces is narrowed to loopback, so nothing on the
//!   network can connect in.
//! * Echo requests, Windows Runtime network classes and what certificate
//!   checks would fetch: see [`elsewhere`], which also names what is not
//!   vetted.
//!
//! Wine's `ws2_32` lacks `WSAConnectByList` and `GetAddrInfoExA`; both are
//! done here from functions it has, as their public references describe
//! them.
//!
//! [`HostPolicy`]: crate::HostPolicy
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::too_many_arguments)]

pub(crate) mod addr;
pub(crate) mod elsewhere;
#[cfg(test)]
mod tests;

use std::ffi::{CStr, c_char, c_void};
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::SetLastError;
use windows_sys::Win32::Globalization::{CP_ACP, MultiByteToWideChar, WideCharToMultiByte};

use self::addr::{loopback, loopback_name, raw_ip, socket_addr};
use crate::modules::files::host;
use crate::{HleExport, HleModule, hle_export};

const SOCKET_ERROR: i32 = -1;
const INVALID_SOCKET: Socket = !0;
const WSAEFAULT: i32 = 10014;
const WSAEINVAL: i32 = 10022;
const WSAEADDRNOTAVAIL: i32 = 10049;
const WSAENETUNREACH: i32 = 10051;
const WSAETIMEDOUT: i32 = 10060;
const WSASERVICE_NOT_FOUND: i32 = 10108;
const WSAHOST_NOT_FOUND: i32 = 11001;
const ERROR_WINHTTP_CANNOT_CONNECT: u32 = 12029;
const ERROR_WINHTTP_AUTODETECTION_FAILED: u32 = 12180;
const NI_NUMERICHOST: i32 = 0x02;
const NI_NAMEREQD: i32 = 0x04;
const SIO_GET_EXTENSION_FUNCTION_POINTER: u32 = 0xC800_0006;
const SIO_GET_MULTIPLE_EXTENSION_FUNCTION_POINTER: u32 = 0xC800_0024;
const WINHTTP_ACCESS_TYPE_NO_PROXY: u32 = 1;
const WINHTTP_OPTION_PROXY: u32 = 38;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct Guid(u32, u16, u16, [u8; 8]);

const WSAID_CONNECTEX: Guid = Guid(
    0x25a2_07b9,
    0xddf3,
    0x4660,
    [0x8e, 0xe9, 0x76, 0xe5, 0x8c, 0x74, 0x06, 0x3e],
);
const WSAID_WSASENDMSG: Guid = Guid(
    0xa441_e712,
    0x754f,
    0x43ca,
    [0x84, 0xa7, 0x0d, 0xee, 0x44, 0xcf, 0x60, 0x6d],
);

type Socket = usize;
type Handle = *mut c_void;

/// Every host function this file and [`elsewhere`] bind with `host!`,
/// sorted: checked against the host at start-up
/// (`crate::host::missing_exports`) and against both files by a test.
pub(crate) const HOST_IMPORTS: &[(&str, &str)] = &[
    ("combase.dll", "RoActivateInstance"),
    ("combase.dll", "RoGetActivationFactory"),
    ("combase.dll", "WindowsGetStringRawBuffer"),
    ("crypt32.dll", "CertGetCertificateChain"),
    ("crypt32.dll", "CertVerifyRevocation"),
    ("iphlpapi.dll", "Icmp6SendEcho2"),
    ("iphlpapi.dll", "IcmpSendEcho"),
    ("iphlpapi.dll", "IcmpSendEcho2"),
    ("iphlpapi.dll", "IcmpSendEcho2Ex"),
    ("winhttp.dll", "WinHttpConnect"),
    ("winhttp.dll", "WinHttpOpen"),
    ("winhttp.dll", "WinHttpSetOption"),
    ("ws2_32.dll", "FreeAddrInfoEx"),
    ("ws2_32.dll", "FreeAddrInfoExW"),
    ("ws2_32.dll", "GetAddrInfoExA"),
    ("ws2_32.dll", "GetAddrInfoExW"),
    ("ws2_32.dll", "GetAddrInfoW"),
    ("ws2_32.dll", "GetNameInfoW"),
    ("ws2_32.dll", "WSAAsyncGetHostByAddr"),
    ("ws2_32.dll", "WSAAsyncGetHostByName"),
    ("ws2_32.dll", "WSAConnect"),
    ("ws2_32.dll", "WSAConnectByList"),
    ("ws2_32.dll", "WSAConnectByNameA"),
    ("ws2_32.dll", "WSAConnectByNameW"),
    ("ws2_32.dll", "WSAIoctl"),
    ("ws2_32.dll", "WSAJoinLeaf"),
    ("ws2_32.dll", "WSASendMsg"),
    ("ws2_32.dll", "WSASendTo"),
    ("ws2_32.dll", "WSASetLastError"),
    ("ws2_32.dll", "bind"),
    ("ws2_32.dll", "connect"),
    ("ws2_32.dll", "getaddrinfo"),
    ("ws2_32.dll", "gethostbyaddr"),
    ("ws2_32.dll", "gethostbyname"),
    ("ws2_32.dll", "getnameinfo"),
    ("ws2_32.dll", "getpeername"),
    ("ws2_32.dll", "getsockname"),
    ("ws2_32.dll", "getsockopt"),
    ("ws2_32.dll", "sendto"),
];

/// Logs `what` for `destination` the first time (no payload, ever).
fn once(what: &'static str, destination: String) {
    static SEEN: Mutex<Vec<(&str, String)>> = Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if !seen.iter().any(|(w, d)| *w == what && *d == destination) {
        tracing::info!(%destination, "{what}");
        seen.push((what, destination));
    }
}

fn wsa_error(code: i32) {
    host!("ws2_32.dll", "WSASetLastError": extern "system" fn(i32))(code);
}

fn narrow(text: *const c_char) -> Option<String> {
    (!text.is_null()).then(|| {
        // SAFETY: the title passes a NUL-terminated string.
        unsafe { CStr::from_ptr(text) }
            .to_string_lossy()
            .into_owned()
    })
}

fn wide(text: *const u16) -> Option<String> {
    // SAFETY: the title passes a NUL-terminated string or null.
    (!text.is_null()).then(|| unsafe {
        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
    })
}

fn shown(addr: Option<impl ToString>) -> String {
    addr.map_or_else(|| "not an IP address".into(), |a| a.to_string())
}

/// True for a loopback `sockaddr`; otherwise logs it and sets
/// `WSAENETUNREACH`. Other address families count as off the machine.
fn reachable(what: &'static str, addr: *const u8, len: i32) -> bool {
    match socket_addr(addr, len) {
        Some(a) if loopback(a.ip()) => true,
        a => {
            once(what, shown(a));
            wsa_error(WSAENETUNREACH);
            false
        }
    }
}

/// Connecting by name: only to loopback. A name would need a lookup
/// (`WSAHOST_NOT_FOUND`), an address a route (`WSAENETUNREACH`).
fn reachable_name(name: Option<String>) -> bool {
    match name {
        Some(name) if !loopback_name(&name) => {
            let literal = name.parse::<IpAddr>().is_ok();
            once("network connect blocked", name);
            wsa_error(if literal {
                WSAENETUNREACH
            } else {
                WSAHOST_NOT_FOUND
            });
            false
        }
        // No name: the host rejects the call itself.
        _ => true,
    }
}

/// Names answered without asking the network: none, `localhost`, IP
/// literals (connecting to one is vetted separately).
fn resolvable(name: Option<String>) -> bool {
    match name {
        Some(name)
            if !(name.is_empty()
                || name.eq_ignore_ascii_case("localhost")
                || name.parse::<IpAddr>().is_ok()) =>
        {
            once("network name lookup blocked", name);
            wsa_error(WSAHOST_NOT_FOUND);
            false
        }
        _ => true,
    }
}

/// Reverse lookups ask DNS unless the address is loopback.
fn reverse_resolvable(ip: Option<IpAddr>) -> bool {
    match ip {
        Some(ip) if loopback(ip) => true,
        ip => {
            once("network reverse lookup blocked", shown(ip));
            false
        }
    }
}

/// `getnameinfo` flags that keep a host name lookup on the machine: the
/// numeric form, as without a network, or `None` if a name is required.
fn name_info_flags(sa: *const u8, len: i32, host_wanted: bool, flags: i32) -> Option<i32> {
    if !host_wanted
        || flags & NI_NUMERICHOST != 0
        || reverse_resolvable(socket_addr(sa, len).map(|a| a.ip()))
    {
        Some(flags)
    } else if flags & NI_NAMEREQD != 0 {
        wsa_error(WSAHOST_NOT_FOUND);
        None
    } else {
        Some(flags | NI_NUMERICHOST)
    }
}

// Connects.

pub extern "system" fn connect(s: Socket, name: *const u8, len: i32) -> i32 {
    if !reachable("network connect blocked", name, len) {
        return SOCKET_ERROR;
    }
    host!("ws2_32.dll", "connect": extern "system" fn(Socket, *const u8, i32) -> i32)(s, name, len)
}

pub extern "system" fn WSAConnect(
    s: Socket,
    name: *const u8,
    len: i32,
    caller_data: *const c_void,
    callee_data: *mut c_void,
    sqos: *const c_void,
    gqos: *const c_void,
) -> i32 {
    if !reachable("network connect blocked", name, len) {
        return SOCKET_ERROR;
    }
    host!("ws2_32.dll", "WSAConnect": extern "system" fn(Socket, *const u8, i32, *const c_void, *mut c_void, *const c_void, *const c_void) -> i32)(
        s,
        name,
        len,
        caller_data,
        callee_data,
        sqos,
        gqos,
    )
}

pub extern "system" fn WSAConnectByNameA(
    s: Socket,
    node: *const c_char,
    service: *const c_char,
    local_len: *mut u32,
    local: *mut c_void,
    remote_len: *mut u32,
    remote: *mut c_void,
    timeout: *const c_void,
    reserved: *mut c_void,
) -> i32 {
    if !reachable_name(narrow(node)) {
        return 0;
    }
    host!("ws2_32.dll", "WSAConnectByNameA": extern "system" fn(Socket, *const c_char, *const c_char, *mut u32, *mut c_void, *mut u32, *mut c_void, *const c_void, *mut c_void) -> i32)(
        s, node, service, local_len, local, remote_len, remote, timeout, reserved,
    )
}

pub extern "system" fn WSAConnectByNameW(
    s: Socket,
    node: *const u16,
    service: *const u16,
    local_len: *mut u32,
    local: *mut c_void,
    remote_len: *mut u32,
    remote: *mut c_void,
    timeout: *const c_void,
    reserved: *mut c_void,
) -> i32 {
    if !reachable_name(wide(node)) {
        return 0;
    }
    host!("ws2_32.dll", "WSAConnectByNameW": extern "system" fn(Socket, *const u16, *const u16, *mut u32, *mut c_void, *mut u32, *mut c_void, *const c_void, *mut c_void) -> i32)(
        s, node, service, local_len, local, remote_len, remote, timeout, reserved,
    )
}

/// `SOCKET_ADDRESS`; a `SOCKET_ADDRESS_LIST` is an `i32` count, then these
/// (pointer aligned).
#[repr(C)]
struct SocketAddress {
    addr: *const u8,
    len: i32,
}

/// The Winsock `timeval`.
#[repr(C)]
pub struct TimeVal {
    seconds: i32,
    microseconds: i32,
}

/// Every candidate must be loopback.
pub extern "system" fn WSAConnectByList(
    s: Socket,
    list: *const u8,
    local_len: *mut u32,
    local: *mut u8,
    remote_len: *mut u32,
    remote: *mut u8,
    timeout: *const TimeVal,
    reserved: *mut c_void,
) -> i32 {
    let entries: &[SocketAddress] = if list.is_null() {
        &[]
    } else {
        // SAFETY: the title passes a `SOCKET_ADDRESS_LIST`.
        unsafe {
            let count = usize::try_from(list.cast::<i32>().read()).unwrap_or(0);
            std::slice::from_raw_parts(list.add(8).cast::<SocketAddress>(), count)
        }
    };
    if !entries
        .iter()
        .all(|e| reachable("network connect blocked", e.addr, e.len))
    {
        return 0;
    }
    // Wine's ws2_32 has no WSAConnectByList.
    if crate::host::function("ws2_32.dll", "WSAConnectByList").is_none() {
        return connect_by_list(
            s, entries, local_len, local, remote_len, remote, timeout, reserved,
        );
    }
    host!("ws2_32.dll", "WSAConnectByList": extern "system" fn(Socket, *const u8, *mut u32, *mut u8, *mut u32, *mut u8, *const TimeVal, *mut c_void) -> i32)(
        s, list, local_len, local, remote_len, remote, timeout, reserved,
    )
}

/// `WSAConnectByList` on a host without it, as its public reference
/// (learn.microsoft.com, winsock2/nf-winsock2-wsaconnectbylist) describes
/// it: the candidates are tried one by one with `connect`; the time limit
/// is looked at between attempts, never during one (`WSAETIMEDOUT`); on
/// failure the error is the last attempt's; on success the two buffers
/// get what `getsockname` and `getpeername` say; `reserved` must be null
/// (`WSAEINVAL`). The candidates are loopback, so there are no pairs of
/// local and remote addresses to rank. An empty list is an invalid
/// parameter too: there was no attempt whose error could be reported.
// shortcut: a socket in non-blocking mode is not made to block for the
// call, as the reference says the real one does; its first attempt ends
// with WSAEWOULDBLOCK.
// Wine on macOS does not connect a socket again after a refused attempt,
// and the call has only the title's socket: there a candidate after a
// refused one fails too, with Wine's error for the second connect.
fn connect_by_list(
    s: Socket,
    entries: &[SocketAddress],
    local_len: *mut u32,
    local: *mut u8,
    remote_len: *mut u32,
    remote: *mut u8,
    timeout: *const TimeVal,
    reserved: *mut c_void,
) -> i32 {
    if entries.is_empty() || !reserved.is_null() {
        wsa_error(WSAEINVAL);
        return 0;
    }
    // SAFETY: the title passes a `timeval` or null.
    let limit = unsafe { timeout.as_ref() }.map(|t| {
        Duration::from_secs(u64::try_from(t.seconds).unwrap_or(0))
            + Duration::from_micros(u64::try_from(t.microseconds).unwrap_or(0))
    });
    let started = Instant::now();
    let host_connect =
        host!("ws2_32.dll", "connect": extern "system" fn(Socket, *const u8, i32) -> i32);
    for (tried, entry) in entries.iter().enumerate() {
        if tried > 0 && limit.is_some_and(|limit| started.elapsed() >= limit) {
            wsa_error(WSAETIMEDOUT);
            return 0;
        }
        if host_connect(s, entry.addr, entry.len) == 0 {
            let name = host!("ws2_32.dll", "getsockname": extern "system" fn(Socket, *mut u8, *mut i32) -> i32);
            let peer = host!("ws2_32.dll", "getpeername": extern "system" fn(Socket, *mut u8, *mut i32) -> i32);
            return (told(name, s, local, local_len) && told(peer, s, remote, remote_len)).into();
        }
    }
    0
}

/// Fills one of `WSAConnectByList`'s address buffers; a null buffer is
/// not wanted, and its length then is not looked at.
fn told(
    address_of: extern "system" fn(Socket, *mut u8, *mut i32) -> i32,
    s: Socket,
    buffer: *mut u8,
    len: *mut u32,
) -> bool {
    if buffer.is_null() {
        return true;
    }
    if len.is_null() {
        wsa_error(WSAEFAULT);
        return false;
    }
    address_of(s, buffer, len.cast()) == 0
}

pub extern "system" fn WSAJoinLeaf(
    s: Socket,
    name: *const u8,
    len: i32,
    caller_data: *const c_void,
    callee_data: *mut c_void,
    sqos: *const c_void,
    gqos: *const c_void,
    flags: u32,
) -> Socket {
    if !reachable("network connect blocked", name, len) {
        return INVALID_SOCKET;
    }
    host!("ws2_32.dll", "WSAJoinLeaf": extern "system" fn(Socket, *const u8, i32, *const c_void, *mut c_void, *const c_void, *const c_void, u32) -> Socket)(
        s,
        name,
        len,
        caller_data,
        callee_data,
        sqos,
        gqos,
        flags,
    )
}

/// `ConnectEx` as `WSAIoctl` hands it out: vetted, then the socket's own.
extern "system" fn connect_ex(
    s: Socket,
    name: *const u8,
    len: i32,
    send: *const c_void,
    send_len: u32,
    sent: *mut u32,
    overlapped: *mut c_void,
) -> i32 {
    if !reachable("network connect blocked", name, len) {
        return 0;
    }
    let (mut real, mut size) = (0usize, 0u32);
    let ioctl = host!("ws2_32.dll", "WSAIoctl": extern "system" fn(Socket, u32, *const c_void, u32, *mut c_void, u32, *mut u32, *mut c_void, *const c_void) -> i32);
    let guid = WSAID_CONNECTEX;
    let found = ioctl(
        s,
        SIO_GET_EXTENSION_FUNCTION_POINTER,
        (&raw const guid).cast(),
        16,
        (&raw mut real).cast(),
        8,
        &mut size,
        std::ptr::null_mut(),
        std::ptr::null(),
    );
    if found != 0 || real == 0 {
        return 0;
    }
    // SAFETY: the provider's `ConnectEx` for this socket.
    let real = unsafe {
        std::mem::transmute::<
            usize,
            extern "system" fn(
                Socket,
                *const u8,
                i32,
                *const c_void,
                u32,
                *mut u32,
                *mut c_void,
            ) -> i32,
        >(real)
    };
    real(s, name, len, send, send_len, sent, overlapped)
}

/// The `ConnectEx` and `WSASendMsg` pointers it hands out are ours; the
/// multi-pointer query (Registered I/O, whose `RIOSendEx` takes any
/// address) is refused as by an old system. Other controls pass through.
pub extern "system" fn WSAIoctl(
    s: Socket,
    code: u32,
    input: *const c_void,
    input_len: u32,
    output: *mut c_void,
    output_len: u32,
    returned: *mut u32,
    overlapped: *mut c_void,
    completion: *const c_void,
) -> i32 {
    if code == SIO_GET_MULTIPLE_EXTENSION_FUNCTION_POINTER {
        wsa_error(WSAEINVAL);
        return SOCKET_ERROR;
    }
    let result = host!("ws2_32.dll", "WSAIoctl": extern "system" fn(Socket, u32, *const c_void, u32, *mut c_void, u32, *mut u32, *mut c_void, *const c_void) -> i32)(
        s, code, input, input_len, output, output_len, returned, overlapped, completion,
    );
    if result == 0 && code == SIO_GET_EXTENSION_FUNCTION_POINTER && input_len >= 16 {
        // SAFETY: the host just read a GUID from `input` and wrote a pointer to `output`.
        unsafe {
            let ours = match input.cast::<Guid>().read_unaligned() {
                WSAID_CONNECTEX => connect_ex as *const () as usize,
                WSAID_WSASENDMSG => WSASendMsg as *const () as usize,
                _ => return result,
            };
            output.cast::<usize>().write_unaligned(ours);
        }
    }
    result
}

// Datagram sends.

/// Without an address it sends to the connected peer, which `connect` vetted;
/// so does a stream socket whatever `to` says (Winsock ignores it there).
pub extern "system" fn sendto(
    s: Socket,
    buf: *const u8,
    len: i32,
    flags: i32,
    to: *const u8,
    to_len: i32,
) -> i32 {
    if !to.is_null() && !stream(s) && !reachable("network datagram blocked", to, to_len) {
        return SOCKET_ERROR;
    }
    host!("ws2_32.dll", "sendto": extern "system" fn(Socket, *const u8, i32, i32, *const u8, i32) -> i32)(
        s, buf, len, flags, to, to_len,
    )
}

pub extern "system" fn WSASendTo(
    s: Socket,
    buffers: *const c_void,
    count: u32,
    sent: *mut u32,
    flags: u32,
    to: *const u8,
    to_len: i32,
    overlapped: *mut c_void,
    completion: *const c_void,
) -> i32 {
    if !to.is_null() && !stream(s) && !reachable("network datagram blocked", to, to_len) {
        return SOCKET_ERROR;
    }
    host!("ws2_32.dll", "WSASendTo": extern "system" fn(Socket, *const c_void, u32, *mut u32, u32, *const u8, i32, *mut c_void, *const c_void) -> i32)(
        s, buffers, count, sent, flags, to, to_len, overlapped, completion,
    )
}

/// A connection-oriented socket, whose sends go to the peer `connect` vetted.
fn stream(s: Socket) -> bool {
    const SOL_SOCKET: i32 = 0xffff;
    const SO_TYPE: i32 = 0x1008;
    const SOCK_STREAM: i32 = 1;
    let (mut kind, mut len) = (0i32, 4i32);
    let getsockopt = host!("ws2_32.dll", "getsockopt": extern "system" fn(Socket, i32, i32, *mut i32, *mut i32) -> i32);
    getsockopt(s, SOL_SOCKET, SO_TYPE, &mut kind, &mut len) == 0 && kind == SOCK_STREAM
}

/// The start of a `WSAMSG`: the destination.
#[repr(C)]
struct MessageName {
    name: *const u8,
    len: i32,
}

pub extern "system" fn WSASendMsg(
    s: Socket,
    message: *const c_void,
    flags: u32,
    sent: *mut u32,
    overlapped: *mut c_void,
    completion: *const c_void,
) -> i32 {
    // SAFETY: the title passes a `WSAMSG` or null.
    if let Some(m) = unsafe { message.cast::<MessageName>().as_ref() }
        && !m.name.is_null()
        && !reachable("network datagram blocked", m.name, m.len)
    {
        return SOCKET_ERROR;
    }
    host!("ws2_32.dll", "WSASendMsg": extern "system" fn(Socket, *const c_void, u32, *mut u32, *mut c_void, *const c_void) -> i32)(
        s, message, flags, sent, overlapped, completion,
    )
}

/// All interfaces become loopback, the only one an unplugged console has;
/// any other address is not ours (`WSAEADDRNOTAVAIL`).
// shortcut: `[::]` becomes `[::1]`, so a dual-stack listener no longer hears
// 127.0.0.1; bind both families if a title relies on that.
pub extern "system" fn bind(s: Socket, name: *const u8, len: i32) -> i32 {
    let host_bind = host!("ws2_32.dll", "bind": extern "system" fn(Socket, *const u8, i32) -> i32);
    match socket_addr(name, len) {
        Some(a) if loopback(a.ip()) => host_bind(s, name, len),
        Some(a) if a.ip().is_unspecified() => {
            once("network bind narrowed to loopback", a.to_string());
            // SAFETY: `socket_addr` checked `len` readable bytes.
            let mut local = unsafe { std::slice::from_raw_parts(name, len as usize) }.to_vec();
            match a {
                SocketAddr::V4(_) => local[4..8].copy_from_slice(&[127, 0, 0, 1]),
                SocketAddr::V6(_) => local[23] = 1,
            }
            host_bind(s, local.as_ptr(), len)
        }
        a => {
            once("network bind blocked", shown(a));
            wsa_error(WSAEADDRNOTAVAIL);
            SOCKET_ERROR
        }
    }
}

// Forward lookups.

fn blocked_lookup(result: *mut *mut c_void) -> i32 {
    if !result.is_null() {
        // SAFETY: a non-null result points to the caller's output slot.
        unsafe { result.write(std::ptr::null_mut()) };
    }
    WSAHOST_NOT_FOUND
}

pub extern "system" fn getaddrinfo(
    node: *const c_char,
    service: *const c_char,
    hints: *const c_void,
    result: *mut *mut c_void,
) -> i32 {
    if !resolvable(narrow(node)) {
        return blocked_lookup(result);
    }
    host!("ws2_32.dll", "getaddrinfo": extern "system" fn(*const c_char, *const c_char, *const c_void, *mut *mut c_void) -> i32)(
        node, service, hints, result,
    )
}

pub extern "system" fn GetAddrInfoW(
    node: *const u16,
    service: *const u16,
    hints: *const c_void,
    result: *mut *mut c_void,
) -> i32 {
    if !resolvable(wide(node)) {
        return blocked_lookup(result);
    }
    host!("ws2_32.dll", "GetAddrInfoW": extern "system" fn(*const u16, *const u16, *const c_void, *mut *mut c_void) -> i32)(
        node, service, hints, result,
    )
}

/// `ADDRINFOEXA` and `ADDRINFOEXW`: one layout, and only what
/// `canonical_name` points to differs (ANSI or UTF-16 text).
#[repr(C)]
struct AddrInfoEx {
    flags: i32,
    family: i32,
    socket_type: i32,
    protocol: i32,
    addr_len: usize,
    canonical_name: *mut c_void,
    addr: *mut u8,
    blob: *mut c_void,
    blob_len: usize,
    provider: *mut c_void,
    next: *mut AddrInfoEx,
}

/// A list `GetAddrInfoExW` made that a title holds as an `ADDRINFOEXA`
/// list: its head, and per entry with a canonical name the entry, the
/// host's UTF-16 name and our ANSI copy the entry points to meanwhile.
struct Converted {
    head: usize,
    names: Vec<(usize, usize, Vec<u8>)>,
}

static CONVERTED: Mutex<Vec<Converted>> = Mutex::new(Vec::new());

/// The UTF-16 form of an ANSI string, NUL included; none for null.
fn from_ansi(text: *const c_char) -> Option<Vec<u16>> {
    if text.is_null() {
        return None;
    }
    // SAFETY: a NUL-terminated string; a sizing call, then one with room.
    unsafe {
        let n = MultiByteToWideChar(CP_ACP, 0, text.cast(), -1, std::ptr::null_mut(), 0);
        let mut wide = vec![0u16; usize::try_from(n).unwrap_or(0).max(1)];
        MultiByteToWideChar(CP_ACP, 0, text.cast(), -1, wide.as_mut_ptr(), n);
        Some(wide)
    }
}

/// The ANSI form of a NUL-terminated UTF-16 string, NUL included.
fn to_ansi(text: *const u16) -> Vec<u8> {
    let (null, no) = (std::ptr::null(), std::ptr::null_mut());
    // SAFETY: a NUL-terminated string; a sizing call, then one with room.
    unsafe {
        let n = WideCharToMultiByte(CP_ACP, 0, text, -1, std::ptr::null_mut(), 0, null, no);
        let mut ansi = vec![0u8; usize::try_from(n).unwrap_or(0).max(1)];
        WideCharToMultiByte(CP_ACP, 0, text, -1, ansi.as_mut_ptr(), n, null, no);
        ansi
    }
}

/// `GetAddrInfoExA` done with `GetAddrInfoExW`, for a host without the
/// ANSI function. The two differ in the encoding of their strings only
/// (learn.microsoft.com, ws2tcpip/nf-ws2tcpip-getaddrinfoexa): the names
/// go in converted from the ANSI code page, and each canonical name in
/// the result is replaced by an ANSI copy until [`FreeAddrInfoEx`] puts
/// the host's back and frees the list.
fn addr_info_ex_a_through_w(
    name: *const c_char,
    service: *const c_char,
    namespace: u32,
    provider: *const c_void,
    hints: *const c_void,
    result: *mut *mut c_void,
    timeout: *const c_void,
    handle: *mut Handle,
) -> i32 {
    let get = host!("ws2_32.dll", "GetAddrInfoExW": extern "system" fn(*const u16, *const u16, u32, *const c_void, *const c_void, *mut *mut c_void, *const c_void, *mut c_void, *const c_void, *mut Handle) -> i32);
    let (name, service) = (from_ansi(name), from_ansi(service));
    let text = |wide: &Option<Vec<u16>>| wide.as_ref().map_or(std::ptr::null(), |w| w.as_ptr());
    let (no, null) = (std::ptr::null_mut(), std::ptr::null());
    if result.is_null() {
        // Nowhere to put a list: the host's answer to that.
        return get(
            text(&name),
            text(&service),
            namespace,
            provider,
            hints,
            result,
            timeout,
            no,
            null,
            handle,
        );
    }
    let mut list = std::ptr::null_mut::<c_void>();
    let code = get(
        text(&name),
        text(&service),
        namespace,
        provider,
        hints,
        &mut list,
        timeout,
        no,
        null,
        handle,
    );
    if code == 0 && !list.is_null() {
        let mut names = Vec::new();
        let mut entry = list.cast::<AddrInfoEx>();
        // SAFETY: the list the host just made, which no one else has yet.
        while let Some(e) = unsafe { entry.as_mut() } {
            if !e.canonical_name.is_null() {
                let mut ansi = to_ansi(e.canonical_name.cast());
                let wide = std::mem::replace(&mut e.canonical_name, ansi.as_mut_ptr().cast());
                names.push((entry as usize, wide as usize, ansi));
            }
            entry = e.next;
        }
        let mut converted = CONVERTED.lock().unwrap_or_else(|e| e.into_inner());
        // An address seen before was freed behind our back.
        converted.retain(|c| c.head != list as usize);
        converted.push(Converted {
            head: list as usize,
            names,
        });
    }
    // SAFETY: the title's place for the list.
    unsafe { result.write(list) };
    code
}

/// On a host without the ANSI function (Wine) it is done with the wide
/// one, for calls that complete at once. The ANSI function has no other
/// kind: its reference says the overlapped and completion parameters must
/// be null, so with either set the call fails as it always has here.
pub extern "system" fn GetAddrInfoExA(
    name: *const c_char,
    service: *const c_char,
    namespace: u32,
    provider: *const c_void,
    hints: *const c_void,
    result: *mut *mut c_void,
    timeout: *const c_void,
    overlapped: *mut c_void,
    completion: *const c_void,
    handle: *mut Handle,
) -> i32 {
    if !resolvable(narrow(name)) {
        return blocked_lookup(result);
    }
    if crate::host::function("ws2_32.dll", "GetAddrInfoExA").is_none() {
        if !overlapped.is_null() || !completion.is_null() {
            tracing::warn!("GetAddrInfoExA is missing from the host ws2_32");
            wsa_error(WSAHOST_NOT_FOUND);
            return WSAHOST_NOT_FOUND;
        }
        return addr_info_ex_a_through_w(
            name, service, namespace, provider, hints, result, timeout, handle,
        );
    }
    host!("ws2_32.dll", "GetAddrInfoExA": extern "system" fn(*const c_char, *const c_char, u32, *const c_void, *const c_void, *mut *mut c_void, *const c_void, *mut c_void, *const c_void, *mut Handle) -> i32)(
        name, service, namespace, provider, hints, result, timeout, overlapped, completion, handle,
    )
}

/// Frees an `ADDRINFOEXA` list: one [`addr_info_ex_a_through_w`] made gets
/// the host's canonical names back and goes to `FreeAddrInfoExW`, whose
/// list it is; any other is the host's own.
pub extern "system" fn FreeAddrInfoEx(list: *mut c_void) {
    let ours = {
        let mut converted = CONVERTED.lock().unwrap_or_else(|e| e.into_inner());
        converted
            .iter()
            .position(|c| c.head == list as usize && !list.is_null())
            .map(|at| converted.swap_remove(at))
    };
    match ours {
        Some(ours) => {
            for (entry, wide, _ansi) in &ours.names {
                // SAFETY: an entry of the list, which lives until the call below.
                unsafe { (*(*entry as *mut AddrInfoEx)).canonical_name = *wide as *mut c_void };
            }
            host!("ws2_32.dll", "FreeAddrInfoExW": extern "system" fn(*mut c_void))(list);
        }
        None => host!("ws2_32.dll", "FreeAddrInfoEx": extern "system" fn(*mut c_void))(list),
    }
}

pub extern "system" fn GetAddrInfoExW(
    name: *const u16,
    service: *const u16,
    namespace: u32,
    provider: *const c_void,
    hints: *const c_void,
    result: *mut *mut c_void,
    timeout: *const c_void,
    overlapped: *mut c_void,
    completion: *const c_void,
    handle: *mut Handle,
) -> i32 {
    if !resolvable(wide(name)) {
        return blocked_lookup(result);
    }
    host!("ws2_32.dll", "GetAddrInfoExW": extern "system" fn(*const u16, *const u16, u32, *const c_void, *const c_void, *mut *mut c_void, *const c_void, *mut c_void, *const c_void, *mut Handle) -> i32)(
        name, service, namespace, provider, hints, result, timeout, overlapped, completion, handle,
    )
}

pub extern "system" fn gethostbyname(name: *const c_char) -> *mut c_void {
    if !resolvable(narrow(name)) {
        return std::ptr::null_mut();
    }
    host!("ws2_32.dll", "gethostbyname": extern "system" fn(*const c_char) -> *mut c_void)(name)
}

pub extern "system" fn WSAAsyncGetHostByName(
    window: Handle,
    message: u32,
    name: *const c_char,
    buf: *mut c_char,
    buf_len: i32,
) -> Handle {
    if !resolvable(narrow(name)) {
        return std::ptr::null_mut();
    }
    host!("ws2_32.dll", "WSAAsyncGetHostByName": extern "system" fn(Handle, u32, *const c_char, *mut c_char, i32) -> Handle)(
        window, message, name, buf, buf_len,
    )
}

/// Name-space queries (DNS, also network lists and Bluetooth) are refused
/// as a console without a network answers them.
// shortcut: local name spaces are refused too; pass those through if a title
// needs one.
pub extern "system" fn WSALookupServiceBeginA(
    _restrictions: *const c_void,
    _flags: u32,
    _lookup: *mut Handle,
) -> i32 {
    once("network name-space query blocked", "any".into());
    wsa_error(WSASERVICE_NOT_FOUND);
    SOCKET_ERROR
}

/// See [`WSALookupServiceBeginA`].
pub extern "system" fn WSALookupServiceBeginW(
    restrictions: *const c_void,
    flags: u32,
    lookup: *mut Handle,
) -> i32 {
    WSALookupServiceBeginA(restrictions, flags, lookup)
}

// Reverse lookups.

pub extern "system" fn gethostbyaddr(addr: *const c_char, len: i32, kind: i32) -> *mut c_void {
    if !reverse_resolvable(raw_ip(addr, len, kind)) {
        wsa_error(WSAHOST_NOT_FOUND);
        return std::ptr::null_mut();
    }
    host!("ws2_32.dll", "gethostbyaddr": extern "system" fn(*const c_char, i32, i32) -> *mut c_void)(
        addr, len, kind,
    )
}

pub extern "system" fn WSAAsyncGetHostByAddr(
    window: Handle,
    message: u32,
    addr: *const c_char,
    len: i32,
    kind: i32,
    buf: *mut c_char,
    buf_len: i32,
) -> Handle {
    if !reverse_resolvable(raw_ip(addr, len, kind)) {
        wsa_error(WSAHOST_NOT_FOUND);
        return std::ptr::null_mut();
    }
    host!("ws2_32.dll", "WSAAsyncGetHostByAddr": extern "system" fn(Handle, u32, *const c_char, i32, i32, *mut c_char, i32) -> Handle)(
        window, message, addr, len, kind, buf, buf_len,
    )
}

pub extern "system" fn getnameinfo(
    sa: *const u8,
    sa_len: i32,
    host: *mut c_char,
    host_len: u32,
    serv: *mut c_char,
    serv_len: u32,
    flags: i32,
) -> i32 {
    let Some(flags) = name_info_flags(sa, sa_len, !host.is_null(), flags) else {
        return WSAHOST_NOT_FOUND;
    };
    host!("ws2_32.dll", "getnameinfo": extern "system" fn(*const u8, i32, *mut c_char, u32, *mut c_char, u32, i32) -> i32)(
        sa, sa_len, host, host_len, serv, serv_len, flags,
    )
}

pub extern "system" fn GetNameInfoW(
    sa: *const u8,
    sa_len: i32,
    host: *mut u16,
    host_len: u32,
    serv: *mut u16,
    serv_len: u32,
    flags: i32,
) -> i32 {
    let Some(flags) = name_info_flags(sa, sa_len, !host.is_null(), flags) else {
        return WSAHOST_NOT_FOUND;
    };
    host!("ws2_32.dll", "GetNameInfoW": extern "system" fn(*const u8, i32, *mut u16, u32, *mut u16, u32, i32) -> i32)(
        sa, sa_len, host, host_len, serv, serv_len, flags,
    )
}

// WinHTTP.

/// The one door to a server: requests, uploads and WebSockets all hang off
/// the handle this returns.
pub extern "system" fn WinHttpConnect(
    session: Handle,
    server: *const u16,
    port: u16,
    reserved: u32,
) -> Handle {
    if let Some(name) = wide(server)
        && !loopback_name(&name)
    {
        once("HTTP connection blocked", format!("{name}:{port}"));
        // SAFETY: no pointers.
        unsafe { SetLastError(ERROR_WINHTTP_CANNOT_CONNECT) };
        return std::ptr::null_mut();
    }
    host!("winhttp.dll", "WinHttpConnect": extern "system" fn(Handle, *const u16, u16, u32) -> Handle)(
        session, server, port, reserved,
    )
}

/// Sessions never use a proxy: only loopback is reachable, and a proxy
/// (named, or found by auto-detection) would carry it off the machine.
pub extern "system" fn WinHttpOpen(
    agent: *const u16,
    access: u32,
    _proxy: *const u16,
    _bypass: *const u16,
    flags: u32,
) -> Handle {
    let open = host!("winhttp.dll", "WinHttpOpen": extern "system" fn(*const u16, u32, *const u16, *const u16, u32) -> Handle);
    open_without_proxy(open, agent, access, flags)
}

type OpenSession = extern "system" fn(*const u16, u32, *const u16, *const u16, u32) -> Handle;

/// [`WinHttpOpen`] with the host's function as a parameter, so that a
/// test can see what the host is asked for.
fn open_without_proxy(open: OpenSession, agent: *const u16, access: u32, flags: u32) -> Handle {
    if access != WINHTTP_ACCESS_TYPE_NO_PROXY {
        once("HTTP proxy ignored", format!("access type {access}"));
    }
    open(
        agent,
        WINHTTP_ACCESS_TYPE_NO_PROXY,
        std::ptr::null(),
        std::ptr::null(),
        flags,
    )
}

/// See [`WinHttpOpen`]: a proxy set later is accepted and not used.
pub extern "system" fn WinHttpSetOption(
    handle: Handle,
    option: u32,
    buffer: *const c_void,
    len: u32,
) -> i32 {
    if option == WINHTTP_OPTION_PROXY {
        once("HTTP proxy ignored", "WINHTTP_OPTION_PROXY".into());
        return 1;
    }
    host!("winhttp.dll", "WinHttpSetOption": extern "system" fn(Handle, u32, *const c_void, u32) -> i32)(
        handle, option, buffer, len,
    )
}

/// Proxy auto-detection asks the network (DHCP, DNS, a PAC download).
fn no_auto_proxy() -> u32 {
    once("HTTP proxy auto-detection blocked", "WPAD/PAC".into());
    ERROR_WINHTTP_AUTODETECTION_FAILED
}

pub extern "system" fn WinHttpDetectAutoProxyConfigUrl(_flags: u32, _url: *mut *mut u16) -> i32 {
    // SAFETY: no pointers.
    unsafe { SetLastError(no_auto_proxy()) };
    0
}

pub extern "system" fn WinHttpGetProxyForUrl(
    _session: Handle,
    _url: *const u16,
    _options: *const c_void,
    _info: *mut c_void,
) -> i32 {
    // SAFETY: no pointers.
    unsafe { SetLastError(no_auto_proxy()) };
    0
}

pub extern "system" fn WinHttpGetProxyForUrlEx(
    _resolver: Handle,
    _url: *const u16,
    _options: *const c_void,
    _context: usize,
) -> u32 {
    no_auto_proxy()
}

pub extern "system" fn WinHttpGetProxyForUrlEx2(
    _resolver: Handle,
    _url: *const u16,
    _options: *const c_void,
    _selection_len: u32,
    _selection: *const u8,
    _context: usize,
) -> u32 {
    no_auto_proxy()
}

pub extern "system" fn WinHttpGetProxySettingsEx(
    _resolver: Handle,
    _kind: i32,
    _params: *const c_void,
    _context: usize,
) -> u32 {
    no_auto_proxy()
}

/// Winsock imports by ordinal; the Winsock 1.1 numbers are fixed.
fn ordinal(name: &'static str, f: usize) -> HleExport {
    HleExport { name, address: f }
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "net",
        dlls: &["ws2_32.dll", "winhttp.dll"],
        exports: vec![
            hle_export!(connect),
            hle_export!(WSAConnect),
            hle_export!(WSAConnectByNameA),
            hle_export!(WSAConnectByNameW),
            hle_export!(WSAConnectByList),
            hle_export!(WSAJoinLeaf),
            hle_export!(WSAIoctl),
            hle_export!(sendto),
            hle_export!(WSASendTo),
            hle_export!(WSASendMsg),
            hle_export!(bind),
            hle_export!(getaddrinfo),
            hle_export!(GetAddrInfoW),
            hle_export!(GetAddrInfoExA),
            hle_export!(GetAddrInfoExW),
            hle_export!(FreeAddrInfoEx),
            hle_export!(gethostbyname),
            hle_export!(WSAAsyncGetHostByName),
            hle_export!(WSALookupServiceBeginA),
            hle_export!(WSALookupServiceBeginW),
            hle_export!(gethostbyaddr),
            hle_export!(WSAAsyncGetHostByAddr),
            hle_export!(getnameinfo),
            hle_export!(GetNameInfoW),
            hle_export!(WinHttpConnect),
            hle_export!(WinHttpOpen),
            hle_export!(WinHttpSetOption),
            hle_export!(WinHttpDetectAutoProxyConfigUrl),
            hle_export!(WinHttpGetProxyForUrl),
            hle_export!(WinHttpGetProxyForUrlEx),
            hle_export!(WinHttpGetProxyForUrlEx2),
            hle_export!(WinHttpGetProxySettingsEx),
            ordinal("#2", bind as *const () as usize),
            ordinal("#4", connect as *const () as usize),
            ordinal("#20", sendto as *const () as usize),
            ordinal("#51", gethostbyaddr as *const () as usize),
            ordinal("#52", gethostbyname as *const () as usize),
            ordinal("#102", WSAAsyncGetHostByAddr as *const () as usize),
            ordinal("#103", WSAAsyncGetHostByName as *const () as usize),
        ],
    }
}
