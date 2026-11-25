//! Every override is called here, against loopback or with an address
//! that is refused before the host hears of it. Nothing in this file
//! opens a connection to another machine.

use std::net::{Ipv4Addr, Ipv6Addr, TcpListener, UdpSocket};
use std::time::Duration;

use victory_loader::Provenance;
use victory_pe::ImportSymbol;
use windows_sys::Win32::Foundation::GetLastError;

use super::addr::{AF_INET, AF_INET6};
use super::*;

const WSAECONNREFUSED: i32 = 10061;
const ERROR_INVALID_HANDLE: u32 = 6;

// TEST-NET-1 (192.0.2.0/24): never routed, and never tried here.
const AWAY: [u8; 4] = [192, 0, 2, 1];
const HOME: [u8; 4] = [127, 0, 0, 1];
/// The documentation prefix 2001:db8::/32: never routed, never tried.
const AWAY6: [u8; 16] = [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];

const STREAM: i32 = 1;
const DATAGRAM: i32 = 2;

fn v4(ip: [u8; 4], port: u16) -> [u8; 16] {
    let mut a = [0u8; 16];
    a[..2].copy_from_slice(&AF_INET.to_le_bytes());
    a[2..4].copy_from_slice(&port.to_be_bytes());
    a[4..8].copy_from_slice(&ip);
    a
}

fn v6(ip: [u8; 16], port: u16) -> [u8; 28] {
    let mut a = [0u8; 28];
    a[..2].copy_from_slice(&AF_INET6.to_le_bytes());
    a[2..4].copy_from_slice(&port.to_be_bytes());
    a[8..24].copy_from_slice(&ip);
    a
}

fn utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

fn named(name: &str) -> ImportSymbol {
    ImportSymbol::Name {
        hint: 0,
        name: name.into(),
    }
}

/// A loopback listener. Making it also starts Winsock for the raw calls
/// (the standard library does that on first use).
fn listener() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

/// True if a connection is waiting on `listener`.
fn connected(listener: &TcpListener) -> bool {
    listener.set_nonblocking(true).unwrap();
    // A loopback connect that returned has reached the listener's queue.
    let waiting = listener.accept().is_ok();
    listener.set_nonblocking(false).unwrap();
    waiting
}

/// A host socket, closed when dropped.
struct Sock(Socket);

impl Sock {
    fn new(family: u16, kind: i32) -> Sock {
        let s = Sock::try_new(family, kind);
        assert_ne!(s.0, INVALID_SOCKET, "socket: {}", last_error());
        s
    }

    fn try_new(family: u16, kind: i32) -> Sock {
        let socket = host!("ws2_32.dll", "socket": extern "system" fn(i32, i32, i32) -> Socket);
        Sock(socket(family.into(), kind, 0))
    }

    /// What `getsockname` says.
    fn local(&self) -> Option<SocketAddr> {
        let name = host!("ws2_32.dll", "getsockname": extern "system" fn(Socket, *mut u8, *mut i32) -> i32);
        let (mut buffer, mut len) = ([0u8; 28], 28);
        (name(self.0, buffer.as_mut_ptr(), &mut len) == 0)
            .then(|| socket_addr(buffer.as_ptr(), len))
            .flatten()
    }
}

impl Drop for Sock {
    fn drop(&mut self) {
        if self.0 != INVALID_SOCKET {
            host!("ws2_32.dll", "closesocket": extern "system" fn(Socket) -> i32)(self.0);
        }
    }
}

fn last_error() -> i32 {
    host!("ws2_32.dll", "WSAGetLastError": extern "system" fn() -> i32)()
}

/// A loopback TCP port that refuses connections: one below 1024 that
/// nothing listens on. No test of this run can come to listen there, as
/// the ports the system hands out are never that low.
fn refusing() -> u16 {
    (1..1024)
        .find(|&port| {
            std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                .is_err_and(|e| e.kind() == std::io::ErrorKind::ConnectionRefused)
        })
        .expect("a loopback port that refuses connections")
}

/// The last Win32 error.
fn win32_error() -> u32 {
    // SAFETY: no arguments.
    unsafe { GetLastError() }
}

fn clear_win32_error() {
    // SAFETY: no pointers.
    unsafe { SetLastError(0) };
}

/// The DLL a title imports `export` of `module` from.
fn home_of(module: &HleModule, export: &str) -> &'static str {
    match (module.name, export) {
        ("net", e) if e.starts_with("WinHttp") => "winhttp.dll",
        ("net", _) => "ws2_32.dll",
        (module, export) => panic!("{module} has {export}, which this test knows no DLL for"),
    }
}

/// The table lists what this module binds. Wine's ws2_32 lacks two of
/// the functions; `WSAConnectByList` and `GetAddrInfoExA` here answer
/// for themselves before they would reach the host's.
#[test]
fn every_host_import_exists_or_has_a_fallback() {
    const WINE_LACKS: &[(&str, &str)] = &[
        ("ws2_32.dll", "GetAddrInfoExA"),
        ("ws2_32.dll", "WSAConnectByList"),
    ];
    crate::host::check_bindings(include_str!("../net.rs"), HOST_IMPORTS, WINE_LACKS);
}

#[test]
fn only_loopback_is_reachable() {
    let (listener, port) = listener();
    let null = std::ptr::null_mut::<c_void>();

    // Connects.
    let s = Sock::new(AF_INET, STREAM);
    assert_eq!(connect(s.0, v4(AWAY, 80).as_ptr(), 16), SOCKET_ERROR);
    assert_eq!(last_error(), WSAENETUNREACH);
    let n = null.cast();
    assert_eq!(
        WSAConnectByNameA(
            s.0,
            c"example.com".as_ptr(),
            c"80".as_ptr(),
            n,
            null,
            n,
            null,
            null,
            null
        ),
        0
    );
    assert_eq!(last_error(), WSAHOST_NOT_FOUND);
    assert_eq!(connect(s.0, v4(HOME, port).as_ptr(), 16), 0);
    listener.accept().unwrap();
    // A stream socket sends to its vetted peer whatever `to` says.
    assert_eq!(
        sendto(s.0, b"x".as_ptr(), 1, 0, v4(AWAY, 9).as_ptr(), 16),
        1
    );
    drop(s);

    // `ConnectEx` from `WSAIoctl` is the vetted one.
    let s = Sock::new(AF_INET, STREAM);
    let mut f = 0usize;
    let mut size = 0u32;
    let guid = WSAID_CONNECTEX;
    let got = WSAIoctl(
        s.0,
        SIO_GET_EXTENSION_FUNCTION_POINTER,
        (&raw const guid).cast(),
        16,
        (&raw mut f).cast(),
        8,
        &mut size,
        null,
        null,
    );
    assert_eq!((got, f), (0, connect_ex as *const () as usize));
    assert_eq!(
        connect_ex(s.0, v4(AWAY, 80).as_ptr(), 16, null, 0, n, null),
        0
    );
    assert_eq!(last_error(), WSAENETUNREACH);
    drop(s);

    // Datagram sends.
    let u = Sock::new(AF_INET, DATAGRAM);
    assert_eq!(
        sendto(u.0, b"x".as_ptr(), 1, 0, v4(AWAY, 9).as_ptr(), 16),
        SOCKET_ERROR
    );
    assert_eq!(last_error(), WSAENETUNREACH);
    assert_eq!(
        sendto(u.0, b"x".as_ptr(), 1, 0, v4(HOME, port).as_ptr(), 16),
        1
    );
    drop(u);

    // Forward and reverse lookups.
    let mut result = null;
    assert_eq!(
        getaddrinfo(
            c"example.com".as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            &mut result
        ),
        WSAHOST_NOT_FOUND
    );
    let name = utf16("example.com");
    assert_eq!(
        GetAddrInfoW(
            name.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            &mut result
        ),
        WSAHOST_NOT_FOUND
    );
    let mut text = [0 as c_char; 64];
    let away = v4(AWAY, 80);
    assert_eq!(
        getnameinfo(
            away.as_ptr(),
            16,
            text.as_mut_ptr(),
            64,
            std::ptr::null_mut(),
            0,
            0
        ),
        0
    );
    // SAFETY: NUL-terminated by `getnameinfo`.
    let numeric = unsafe { CStr::from_ptr(text.as_ptr()) };
    assert_eq!(numeric.to_str(), Ok("192.0.2.1"));
    assert_eq!(
        getnameinfo(
            away.as_ptr(),
            16,
            text.as_mut_ptr(),
            64,
            std::ptr::null_mut(),
            0,
            NI_NAMEREQD
        ),
        WSAHOST_NOT_FOUND
    );
    assert!(gethostbyaddr(AWAY.as_ptr().cast(), 4, 2).is_null());

    // WinHTTP.
    assert!(WinHttpConnect(null, name.as_ptr(), 443, 0).is_null());
    assert_eq!(win32_error(), ERROR_WINHTTP_CANNOT_CONNECT);
    assert_eq!(
        WinHttpGetProxyForUrlEx(null, name.as_ptr(), std::ptr::null(), 0),
        ERROR_WINHTTP_AUTODETECTION_FAILED
    );
}

/// The ordinals we override name the functions they stand for.
#[test]
fn ordinals_stand_for_their_winsock_functions() {
    let m = module();
    let by_ordinal: Vec<_> = m
        .exports
        .iter()
        .filter(|e| e.name.starts_with('#'))
        .collect();
    let numbers: Vec<_> = by_ordinal.iter().map(|e| e.name).collect();
    assert_eq!(numbers, ["#2", "#4", "#20", "#51", "#52", "#102", "#103"]);
    for e in by_ordinal {
        let named = m
            .exports
            .iter()
            .find(|n| n.address == e.address && !n.name.starts_with('#'))
            .unwrap();
        let ordinal = ImportSymbol::Ordinal(e.name[1..].parse().unwrap());
        assert_eq!(
            crate::HostPolicy::default().resolve("ws2_32.dll", &ordinal),
            crate::host::function("ws2_32.dll", named.name),
            "{} is not {}",
            e.name,
            named.name
        );
    }
}

/// Every override names a real export of its DLL. Some exports only
/// exist on newer Windows (WinHttpGetProxySettingsEx is absent from
/// Windows 10's winhttp.dll), and Wine lacks a few more. The HLE
/// supplies those itself or checks for them at run time, so they are
/// excused only when the host really lacks them.
#[test]
fn every_override_names_an_export_of_its_dll() {
    const NEWER_WINDOWS_ONLY: &[&str] = &["WinHttpGetProxySettingsEx"];
    const WINE_LACKS: &[&str] = &["WSAConnectByList", "GetAddrInfoExA"];
    let wine = crate::host::function("ntdll.dll", "wine_get_version").is_some();
    for m in [module()] {
        for e in m.exports.iter().filter(|e| !e.name.starts_with('#')) {
            let dll = home_of(&m, e.name);
            assert!(m.serves(dll), "{} does not serve {dll}", m.name);
            let present = crate::host::function(dll, e.name).is_some();
            let excused =
                NEWER_WINDOWS_ONLY.contains(&e.name) || wine && WINE_LACKS.contains(&e.name);
            assert!(present || excused, "{dll} lacks {}", e.name);
        }
    }
}

/// What a title's import is bound to, by name: ours where a call could
/// leave the machine, the host's own for the rest of the same DLLs.
#[test]
fn imports_that_could_leave_the_machine_are_ours() {
    let resolver = crate::HleResolver::default();
    for (dll, name) in [
        ("ws2_32.dll", "connect"),
        ("WS2_32.dll", "WSAConnectByList"),
        ("ws2_32.dll", "GetAddrInfoExA"),
        ("ws2_32.dll", "FreeAddrInfoEx"),
        ("ws2_32.dll", "WSASendMsg"),
        ("winhttp.dll", "WinHttpOpen"),
    ] {
        assert_eq!(
            resolver.classify(dll, &named(name)),
            Some(Provenance::Hle),
            "{dll}!{name}"
        );
    }
    for (dll, name) in [
        ("ws2_32.dll", "recv"),
        ("ws2_32.dll", "FreeAddrInfoExW"),
        ("iphlpapi.dll", "IcmpCreateFile"),
        ("iphlpapi.dll", "GetAdaptersAddresses"),
        ("crypt32.dll", "CertOpenStore"),
        ("crypt32.dll", "CertFreeCertificateChain"),
        (
            "api-ms-win-core-winrt-string-l1-1-0.dll",
            "WindowsCreateString",
        ),
    ] {
        assert_eq!(
            resolver.classify(dll, &named(name)),
            Some(Provenance::Host),
            "{dll}!{name}"
        );
    }
    // The Winsock ordinals are Winsock's: number 4 of another DLL is not
    // our `connect`.
    for dll in [
        "iphlpapi.dll",
        "crypt32.dll",
        "api-ms-win-core-winrt-l1-1-0.dll",
    ] {
        assert_ne!(
            resolver.classify(dll, &ImportSymbol::Ordinal(4)),
            Some(Provenance::Hle),
            "{dll} #4"
        );
    }
    assert_eq!(
        resolver.classify("ws2_32.dll", &ImportSymbol::Ordinal(4)),
        Some(Provenance::Hle)
    );
}

#[test]
fn bind_any_is_narrowed_to_loopback() {
    let _winsock = listener();
    let s = Sock::new(AF_INET, STREAM);
    assert_eq!(bind(s.0, v4([0; 4], 0).as_ptr(), 16), 0);
    let local = s.local().unwrap();
    assert_eq!(local.ip(), IpAddr::from(HOME));
    assert_ne!(local.port(), 0);

    // A loopback address is bound as the title gave it.
    let s = Sock::new(AF_INET, DATAGRAM);
    assert_eq!(bind(s.0, v4(HOME, 0).as_ptr(), 16), 0);
    assert_eq!(s.local().map(|a| a.ip()), Some(IpAddr::from(HOME)));

    let s = Sock::try_new(AF_INET6, STREAM);
    if s.0 == INVALID_SOCKET {
        crate::testing::skip("no IPv6 sockets on this host");
        return;
    }
    assert_eq!(bind(s.0, v6([0; 16], 0).as_ptr(), 28), 0);
    assert_eq!(
        s.local().map(|a| a.ip()),
        Some(IpAddr::from(Ipv6Addr::LOCALHOST))
    );
}

#[test]
fn bind_elsewhere_is_refused() {
    let _winsock = listener();
    let s = Sock::new(AF_INET, STREAM);
    let refused = |name: &[u8], len: i32| {
        wsa_error(0);
        (bind(s.0, name.as_ptr(), len), last_error())
    };
    let no = (SOCKET_ERROR, WSAEADDRNOTAVAIL);
    assert_eq!(refused(&v4(AWAY, 0), 16), no);
    assert_eq!(refused(&v4([10, 0, 0, 1], 8080), 16), no);
    assert_eq!(refused(&v6(AWAY6, 0), 28), no);
    // Not an address we can read: another family, or cut short.
    let mut other = [0u8; 28];
    other[0] = 1;
    assert_eq!(refused(&other, 28), no);
    assert_eq!(refused(&v4(HOME, 0), 15), no);
    // The socket was never bound.
    assert_eq!(s.local(), None);
}

#[test]
fn wsa_connect_and_join_leaf_away_are_refused() {
    let (listener, port) = listener();
    let null = std::ptr::null_mut::<c_void>();
    let s = Sock::new(AF_INET, STREAM);

    wsa_error(0);
    assert_eq!(
        WSAConnect(s.0, v4(AWAY, 80).as_ptr(), 16, null, null, null, null),
        SOCKET_ERROR
    );
    assert_eq!(last_error(), WSAENETUNREACH);

    wsa_error(0);
    assert_eq!(
        WSAConnect(s.0, v6(AWAY6, 80).as_ptr(), 28, null, null, null, null),
        SOCKET_ERROR
    );
    assert_eq!(last_error(), WSAENETUNREACH);

    wsa_error(0);
    assert_eq!(
        WSAJoinLeaf(s.0, v4(AWAY, 80).as_ptr(), 16, null, null, null, null, 0),
        INVALID_SOCKET
    );
    assert_eq!(last_error(), WSAENETUNREACH);

    // By name: a name needs a lookup, an address a route.
    let by_name = |node: &str| {
        let (node, service) = (utf16(node), utf16("80"));
        wsa_error(0);
        let n = std::ptr::null_mut();
        let connected = WSAConnectByNameW(
            s.0,
            node.as_ptr(),
            service.as_ptr(),
            n,
            null,
            n,
            null,
            null,
            null,
        );
        (connected, last_error())
    };
    assert_eq!(by_name("example.com"), (0, WSAHOST_NOT_FOUND));
    assert_eq!(by_name("192.0.2.1"), (0, WSAENETUNREACH));
    assert_eq!(by_name("2001:db8::1"), (0, WSAENETUNREACH));
    wsa_error(0);
    let n = std::ptr::null_mut();
    assert_eq!(
        WSAConnectByNameA(
            s.0,
            c"192.0.2.1".as_ptr(),
            c"80".as_ptr(),
            n,
            null,
            n,
            null,
            null,
            null
        ),
        0
    );
    assert_eq!(last_error(), WSAENETUNREACH);

    // None of that reached the listener; a loopback connect does.
    assert!(!connected(&listener));
    assert_eq!(
        WSAConnect(s.0, v4(HOME, port).as_ptr(), 16, null, null, null, null),
        0
    );
    assert!(connected(&listener));
}

/// A `WSABUF`.
#[repr(C)]
struct Buffer {
    len: u32,
    bytes: *const u8,
}

/// A `WSAMSG`.
#[repr(C)]
struct Message {
    name: *const u8,
    name_len: i32,
    buffers: *const Buffer,
    buffer_count: u32,
    control: Buffer,
    flags: u32,
}

#[test]
fn udp_sends_away_are_refused() {
    let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
    receiver
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let port = receiver.local_addr().unwrap().port();
    let received = || {
        let mut datagram = [0u8; 8];
        let n = receiver.recv(&mut datagram).unwrap();
        datagram[..n].to_vec()
    };
    let null = std::ptr::null_mut::<c_void>();
    let u = Sock::new(AF_INET, DATAGRAM);
    let (away, home) = (v4(AWAY, 9), v4(HOME, port));

    let send_to = |bytes: &'static [u8], to: &[u8; 16]| {
        let buffer = Buffer {
            len: bytes.len() as u32,
            bytes: bytes.as_ptr(),
        };
        let mut sent = 0u32;
        wsa_error(0);
        let result = WSASendTo(
            u.0,
            (&raw const buffer).cast(),
            1,
            &mut sent,
            0,
            to.as_ptr(),
            16,
            null,
            null,
        );
        (result, sent, last_error())
    };
    assert_eq!(send_to(b"a", &away), (SOCKET_ERROR, 0, WSAENETUNREACH));
    assert_eq!(send_to(b"b", &home), (0, 1, 0));
    assert_eq!(received(), b"b");

    let send_message = |bytes: &'static [u8], to: &[u8; 16]| {
        let buffer = Buffer {
            len: bytes.len() as u32,
            bytes: bytes.as_ptr(),
        };
        let message = Message {
            name: to.as_ptr(),
            name_len: 16,
            buffers: &buffer,
            buffer_count: 1,
            control: Buffer {
                len: 0,
                bytes: std::ptr::null(),
            },
            flags: 0,
        };
        let mut sent = 0u32;
        wsa_error(0);
        let result = WSASendMsg(u.0, (&raw const message).cast(), 0, &mut sent, null, null);
        (result, sent, last_error())
    };
    assert_eq!(send_message(b"c", &away), (SOCKET_ERROR, 0, WSAENETUNREACH));
    assert_eq!(send_message(b"d", &home), (0, 1, 0));
    // Nothing refused arrived in between.
    assert_eq!(received(), b"d");

    // `WSASendMsg` as `WSAIoctl` hands it out is the vetted one.
    let (mut f, mut size) = (0usize, 0u32);
    let guid = WSAID_WSASENDMSG;
    let got = WSAIoctl(
        u.0,
        SIO_GET_EXTENSION_FUNCTION_POINTER,
        (&raw const guid).cast(),
        16,
        (&raw mut f).cast(),
        8,
        &mut size,
        null,
        null,
    );
    assert_eq!((got, f), (0, WSASendMsg as *const () as usize));
}

/// The multi-pointer query would hand out Registered I/O, whose sends
/// take any address.
#[test]
fn multiple_extension_pointers_are_refused() {
    let _winsock = listener();
    let s = Sock::new(AF_INET, STREAM);
    let null = std::ptr::null_mut::<c_void>();
    let (mut table, mut size) = ([0usize; 16], 0u32);
    wsa_error(0);
    let got = WSAIoctl(
        s.0,
        SIO_GET_MULTIPLE_EXTENSION_FUNCTION_POINTER,
        null,
        0,
        table.as_mut_ptr().cast(),
        128,
        &mut size,
        null,
        null,
    );
    assert_eq!((got, last_error()), (SOCKET_ERROR, WSAEINVAL));
    assert_eq!((table, size), ([0; 16], 0));
}

/// The loopback addresses of an `ADDRINFOEX` list, of either encoding.
fn addresses(list: *const c_void) -> Vec<SocketAddr> {
    let mut found = Vec::new();
    let mut entry = list.cast::<AddrInfoEx>();
    // SAFETY: a list from `GetAddrInfoEx`.
    while let Some(e) = unsafe { entry.as_ref() } {
        found.extend(socket_addr(e.addr, e.addr_len as i32));
        entry = e.next;
    }
    found
}

const NS_DNS: u32 = 12;

#[test]
fn name_lookups_only_know_loopback() {
    let _winsock = listener();
    let null = std::ptr::null_mut::<c_void>();
    let no = std::ptr::null::<c_void>();

    // `GetAddrInfoExW`.
    let lookup = |name: &str| {
        let name = utf16(name);
        let mut list = null;
        let code = GetAddrInfoExW(
            name.as_ptr(),
            std::ptr::null(),
            NS_DNS,
            no,
            no,
            &mut list,
            no,
            null,
            no,
            std::ptr::null_mut(),
        );
        let found = addresses(list);
        if !list.is_null() {
            host!("ws2_32.dll", "FreeAddrInfoExW": extern "system" fn(*mut c_void))(list);
        }
        (code, found)
    };
    assert_eq!(lookup("example.com"), (WSAHOST_NOT_FOUND, vec![]));
    assert_eq!(lookup("www.example.com."), (WSAHOST_NOT_FOUND, vec![]));
    let (code, found) = lookup("localhost");
    assert_eq!(code, 0);
    assert!(!found.is_empty());
    assert!(found.iter().all(|a| loopback(a.ip())), "{found:?}");
    // A literal needs no lookup; connecting to it is vetted separately.
    let (code, found) = lookup("192.0.2.1");
    assert_eq!(code, 0);
    assert!(!found.is_empty());
    assert!(found.iter().all(|a| *a == (AWAY, 0).into()), "{found:?}");

    // `gethostbyname`.
    wsa_error(0);
    assert!(gethostbyname(c"example.com".as_ptr()).is_null());
    assert_eq!(last_error(), WSAHOST_NOT_FOUND);
    /// A `hostent`.
    #[repr(C)]
    struct HostEntry {
        name: *const c_char,
        aliases: *const *const c_char,
        kind: i16,
        length: i16,
        addresses: *const *const u8,
    }
    let entry = gethostbyname(c"localhost".as_ptr()).cast::<HostEntry>();
    // SAFETY: the host's `hostent` for this thread, or null.
    let entry = unsafe { entry.as_ref() }.expect("localhost");
    assert_eq!((entry.kind, entry.length), (AF_INET as i16, 4));
    // SAFETY: a null-terminated list of 4-byte addresses, not empty.
    let first = unsafe { (*entry.addresses).cast::<[u8; 4]>().read_unaligned() };
    assert!(loopback(Ipv4Addr::from(first).into()), "{first:?}");

    // The asynchronous forms: no task is started.
    let mut buffer = [0 as c_char; 1024];
    wsa_error(0);
    assert!(
        WSAAsyncGetHostByName(
            null,
            0x0400,
            c"example.com".as_ptr(),
            buffer.as_mut_ptr(),
            1024
        )
        .is_null()
    );
    assert_eq!(last_error(), WSAHOST_NOT_FOUND);
    wsa_error(0);
    assert!(
        WSAAsyncGetHostByAddr(
            null,
            0x0400,
            AWAY.as_ptr().cast(),
            4,
            AF_INET.into(),
            buffer.as_mut_ptr(),
            1024
        )
        .is_null()
    );
    assert_eq!(last_error(), WSAHOST_NOT_FOUND);

    // Name-space queries find nothing.
    let mut query = null;
    wsa_error(0);
    assert_eq!(WSALookupServiceBeginA(no, 0, &mut query), SOCKET_ERROR);
    assert_eq!(last_error(), WSASERVICE_NOT_FOUND);
    wsa_error(0);
    assert_eq!(WSALookupServiceBeginW(no, 0x0FF0, &mut query), SOCKET_ERROR);
    assert_eq!(last_error(), WSASERVICE_NOT_FOUND);
    assert!(query.is_null());

    // `GetNameInfoW`: numeric, as with no network, unless a name is required.
    let away = v4(AWAY, 80);
    let mut text = [0u16; 64];
    assert_eq!(
        GetNameInfoW(
            away.as_ptr(),
            16,
            text.as_mut_ptr(),
            64,
            std::ptr::null_mut(),
            0,
            0
        ),
        0
    );
    assert_eq!(wide(text.as_ptr()).as_deref(), Some("192.0.2.1"));
    wsa_error(0);
    assert_eq!(
        GetNameInfoW(
            away.as_ptr(),
            16,
            text.as_mut_ptr(),
            64,
            std::ptr::null_mut(),
            0,
            NI_NAMEREQD
        ),
        WSAHOST_NOT_FOUND
    );
    assert_eq!(last_error(), WSAHOST_NOT_FOUND);
    // Only the service was asked for: nothing to look up.
    let mut service = [0u16; 16];
    assert_eq!(
        GetNameInfoW(
            away.as_ptr(),
            16,
            std::ptr::null_mut(),
            0,
            service.as_mut_ptr(),
            16,
            0x08 // NI_NUMERICSERV
        ),
        0
    );
    assert_eq!(wide(service.as_ptr()).as_deref(), Some("80"));
}

/// A `SOCKET_ADDRESS_LIST` of `N`.
#[repr(C)]
struct AddressList<const N: usize> {
    count: i32,
    entries: [SocketAddress; N],
}

fn address_list<const N: usize>(addresses: [&[u8; 16]; N]) -> AddressList<N> {
    AddressList {
        count: N as i32,
        entries: addresses.map(|a| SocketAddress {
            addr: a.as_ptr(),
            len: 16,
        }),
    }
}

/// `WSAConnectByList`, and the stand-in for a host without it (Wine),
/// which every host can run.
#[test]
fn connect_by_list_reaches_loopback_and_sets_an_error_otherwise() {
    let (listener, port) = listener();
    let (home, closed, away) = (v4(HOME, port), v4(HOME, refusing()), v4(AWAY, 80));
    let no_buffer = std::ptr::null_mut::<u8>();
    let no_len = std::ptr::null_mut::<u32>();
    let null = std::ptr::null_mut::<c_void>();
    let by_list = |s: &Sock, list: *const u8| {
        wsa_error(0);
        let connected = WSAConnectByList(
            s.0,
            list,
            no_len,
            no_buffer,
            no_len,
            no_buffer,
            std::ptr::null(),
            null,
        );
        (connected, last_error())
    };

    // One candidate off the machine refuses the call; none is tried.
    let s = Sock::new(AF_INET, STREAM);
    let list = address_list([&away]);
    assert_eq!(by_list(&s, (&raw const list).cast()), (0, WSAENETUNREACH));
    let list = address_list([&home, &away]);
    assert_eq!(by_list(&s, (&raw const list).cast()), (0, WSAENETUNREACH));
    assert!(!connected(&listener));

    // A failure leaves a Winsock error for the thread: once it left
    // whatever the thread's last error happened to be.
    let winsock = 10000..12000;
    let list = address_list([&closed]);
    let (result, error) = by_list(&s, (&raw const list).cast());
    assert_eq!(result, 0);
    assert!(winsock.contains(&error), "after a refused connect: {error}");
    let (result, error) = by_list(&s, std::ptr::null());
    assert_eq!(result, 0);
    assert!(winsock.contains(&error), "without a list: {error}");
    drop(s);

    // A loopback candidate connects, and the buffers say between whom.
    let s = Sock::new(AF_INET, STREAM);
    let list = address_list([&home]);
    let (mut local, mut remote) = ([0u8; 28], [0u8; 28]);
    let (mut local_len, mut remote_len) = (28u32, 28u32);
    assert_eq!(
        WSAConnectByList(
            s.0,
            (&raw const list).cast(),
            &mut local_len,
            local.as_mut_ptr(),
            &mut remote_len,
            remote.as_mut_ptr(),
            std::ptr::null(),
            null,
        ),
        1,
        "error {}",
        last_error()
    );
    let (_, peer) = listener.accept().unwrap();
    assert_eq!((local_len, remote_len), (16, 16));
    assert_eq!(socket_addr(local.as_ptr(), 16), Some(peer));
    assert_eq!(socket_addr(remote.as_ptr(), 16), Some((HOME, port).into()));
    drop(s);

    // The stand-in: `entries` have been vetted by its caller.
    let stand_in =
        |s: &Sock, entries: &[SocketAddress], timeout: *const TimeVal, reserved: *mut c_void| {
            wsa_error(0);
            let connected = connect_by_list(
                s.0, entries, no_len, no_buffer, no_len, no_buffer, timeout, reserved,
            );
            (connected, last_error())
        };
    let entry = |a: &[u8; 16]| SocketAddress {
        addr: a.as_ptr(),
        len: 16,
    };
    let unlimited = std::ptr::null();

    // Invalid parameters.
    let s = Sock::new(AF_INET, STREAM);
    assert_eq!(stand_in(&s, &[], unlimited, null), (0, WSAEINVAL));
    let mut reserved = 0u8;
    assert_eq!(
        stand_in(&s, &[entry(&home)], unlimited, (&raw mut reserved).cast()),
        (0, WSAEINVAL)
    );
    assert!(!connected(&listener));

    // The error is the last attempt's.
    assert_eq!(
        stand_in(&s, &[entry(&closed)], unlimited, null),
        (0, WSAECONNREFUSED)
    );
    drop(s);

    // The time limit is looked at between attempts: after the first one
    // a limit of nothing has passed, and the second is not made.
    let s = Sock::new(AF_INET, STREAM);
    let at_once = TimeVal {
        seconds: 0,
        microseconds: 0,
    };
    assert_eq!(
        stand_in(&s, &[entry(&closed), entry(&home)], &at_once, null),
        (0, WSAETIMEDOUT)
    );
    assert!(!connected(&listener));
    drop(s);

    // A buffer without a length cannot be filled.
    let s = Sock::new(AF_INET, STREAM);
    wsa_error(0);
    assert_eq!(
        connect_by_list(
            s.0,
            &[entry(&home)],
            no_len,
            local.as_mut_ptr(),
            no_len,
            no_buffer,
            unlimited,
            null,
        ),
        0
    );
    assert_eq!(last_error(), WSAEFAULT);
    // The connection was made by then; the report is what failed.
    assert!(connected(&listener));

    // The candidates are tried in order until one connects.
    drop(s);
    let s = Sock::new(AF_INET, STREAM);
    let generous = TimeVal {
        seconds: 60,
        microseconds: 0,
    };
    let (mut local_len, mut remote_len) = (28u32, 28u32);
    wsa_error(0);
    let reached = connect_by_list(
        s.0,
        &[entry(&closed), entry(&home)],
        &mut local_len,
        local.as_mut_ptr(),
        &mut remote_len,
        remote.as_mut_ptr(),
        &generous,
        null,
    );
    let wine = crate::host::function("ntdll.dll", "wine_get_version").is_some();
    if wine && reached == 0 {
        // Windows connects a socket again after a refused attempt. Wine
        // on macOS does not, and the stand-in has only that socket: the
        // call fails there, with an error and without a connection.
        assert_ne!(last_error(), 0);
        assert!(!connected(&listener));
        crate::testing::skip("this Wine does not connect a socket again after a refused attempt");
        return;
    }
    assert_eq!(reached, 1, "error {}", last_error());
    let (_, peer) = listener.accept().unwrap();
    assert_eq!(socket_addr(local.as_ptr(), local_len as i32), Some(peer));
    assert_eq!(
        socket_addr(remote.as_ptr(), remote_len as i32),
        Some((HOME, port).into())
    );
    assert_eq!(s.local(), Some(peer));
}

/// True while `list` is one of ours, with canonical names swapped.
fn converted(list: *const c_void) -> bool {
    let converted = CONVERTED.lock().unwrap_or_else(|e| e.into_inner());
    converted.iter().any(|c| c.head == list as usize)
}

/// `GetAddrInfoExA`, and the stand-in for a host without it (Wine),
/// which every host can run.
#[test]
fn get_addr_info_ex_a_resolves_localhost_and_frees() {
    let _winsock = listener();
    let null = std::ptr::null_mut::<c_void>();
    let no = std::ptr::null::<c_void>();
    let no_handle = std::ptr::null_mut();
    const AI_CANONNAME: i32 = 0x2;
    let hints = AddrInfoEx {
        flags: AI_CANONNAME,
        family: AF_INET.into(),
        socket_type: STREAM,
        protocol: 0,
        addr_len: 0,
        canonical_name: null,
        addr: std::ptr::null_mut(),
        blob: null,
        blob_len: 0,
        provider: null,
        next: std::ptr::null_mut(),
    };
    let hints = (&raw const hints).cast::<c_void>();

    // Names that would need the network are refused before the host.
    let mut list = null;
    wsa_error(0);
    assert_eq!(
        GetAddrInfoExA(
            c"example.com".as_ptr(),
            std::ptr::null(),
            NS_DNS,
            no,
            no,
            &mut list,
            no,
            null,
            no,
            no_handle
        ),
        WSAHOST_NOT_FOUND
    );
    assert_eq!(last_error(), WSAHOST_NOT_FOUND);
    assert!(list.is_null());

    // `localhost` resolves: once it did not where the host lacks the
    // function, though the vetting had let it through.
    let code = GetAddrInfoExA(
        c"localhost".as_ptr(),
        c"80".as_ptr(),
        NS_DNS,
        no,
        hints,
        &mut list,
        no,
        null,
        no,
        no_handle,
    );
    assert_eq!(code, 0);
    let found = addresses(list);
    assert!(!found.is_empty());
    assert!(
        found.iter().all(|a| loopback(a.ip()) && a.port() == 80),
        "{found:?}"
    );
    // SAFETY: the list's first entry; its canonical name was asked for.
    let name = unsafe { (*list.cast::<AddrInfoEx>()).canonical_name };
    assert!(!name.is_null());
    // SAFETY: an ANSI string.
    let name = unsafe { CStr::from_ptr(name.cast()) };
    assert!(!name.is_empty());
    FreeAddrInfoEx(list);
    assert!(!converted(list));

    // A literal, without hints.
    let mut list = null;
    let code = GetAddrInfoExA(
        c"127.0.0.1".as_ptr(),
        std::ptr::null(),
        NS_DNS,
        no,
        no,
        &mut list,
        no,
        null,
        no,
        no_handle,
    );
    assert_eq!(code, 0);
    assert_eq!(addresses(list).first(), Some(&(HOME, 0).into()));
    FreeAddrInfoEx(list);
    FreeAddrInfoEx(null);

    // The stand-in gives what the wide function gives, in ANSI.
    let mut list = null;
    let code = addr_info_ex_a_through_w(
        c"localhost".as_ptr(),
        c"80".as_ptr(),
        NS_DNS,
        no,
        hints,
        &mut list,
        no,
        no_handle,
    );
    assert_eq!(code, 0);
    assert!(converted(list));
    let (name, service) = (utf16("localhost"), utf16("80"));
    let mut wide_list = null;
    let code = GetAddrInfoExW(
        name.as_ptr(),
        service.as_ptr(),
        NS_DNS,
        no,
        hints,
        &mut wide_list,
        no,
        null,
        no,
        no_handle,
    );
    assert_eq!(code, 0);
    assert_eq!(addresses(list), addresses(wide_list));
    // SAFETY: the first entries of both lists; the names were asked for.
    let (ansi, utf) = unsafe {
        let ansi = (*list.cast::<AddrInfoEx>()).canonical_name;
        let utf = (*wide_list.cast::<AddrInfoEx>()).canonical_name;
        assert!(!ansi.is_null() && !utf.is_null());
        (CStr::from_ptr(ansi.cast()), wide(utf.cast()).unwrap())
    };
    assert_eq!(ansi.to_str(), Ok(utf.as_str()));
    host!("ws2_32.dll", "FreeAddrInfoExW": extern "system" fn(*mut c_void))(wide_list);
    // Freeing it puts the host's names back first, and forgets the list.
    FreeAddrInfoEx(list);
    assert!(!converted(list));

    // The stand-in reports the wide function's failures as they are.
    let mut list = null;
    let code = addr_info_ex_a_through_w(
        c"localhost".as_ptr(),
        c"no such service, surely".as_ptr(),
        NS_DNS,
        no,
        no,
        &mut list,
        no,
        no_handle,
    );
    assert_ne!(code, 0);
    assert!(list.is_null());

    // The ANSI function cannot complete later; where it is ours, a call
    // that asks for that fails as every call once did.
    if crate::host::function("ws2_32.dll", "GetAddrInfoExA").is_none() {
        let mut overlapped = [0usize; 4];
        wsa_error(0);
        let code = GetAddrInfoExA(
            c"localhost".as_ptr(),
            std::ptr::null(),
            NS_DNS,
            no,
            no,
            &mut list,
            no,
            overlapped.as_mut_ptr().cast(),
            no,
            no_handle,
        );
        assert_eq!((code, last_error()), (WSAHOST_NOT_FOUND, WSAHOST_NOT_FOUND));
        assert!(list.is_null());
    }
}

/// What the stand-in for the host's `WinHttpOpen` was asked for.
static OPENED: Mutex<Vec<Opened>> = Mutex::new(Vec::new());

/// The agent, access type, proxy, bypass list and flags of one call.
type Opened = (usize, u32, usize, usize, u32);

extern "system" fn recording_open(
    agent: *const u16,
    access: u32,
    proxy: *const u16,
    bypass: *const u16,
    flags: u32,
) -> Handle {
    let mut opened = OPENED.lock().unwrap_or_else(|e| e.into_inner());
    opened.push((
        agent as usize,
        access,
        proxy as usize,
        bypass as usize,
        flags,
    ));
    std::ptr::without_provenance_mut(0x5e55)
}

#[test]
fn winhttp_never_uses_a_proxy() {
    const WINHTTP_ACCESS_TYPE_DEFAULT_PROXY: u32 = 0;
    const WINHTTP_ACCESS_TYPE_NAMED_PROXY: u32 = 3;
    const WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY: u32 = 4;
    const WINHTTP_FLAG_ASYNC: u32 = 0x1000_0000;
    const WINHTTP_OPTION_CONNECT_TIMEOUT: u32 = 3;
    let null = std::ptr::null_mut::<c_void>();
    let agent = utf16("victory test");
    let proxy = utf16("192.0.2.1:8080");

    // Whatever access a title asks for, the host is asked for none.
    for access in [
        WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
        WINHTTP_ACCESS_TYPE_NO_PROXY,
        WINHTTP_ACCESS_TYPE_NAMED_PROXY,
        WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
    ] {
        let session =
            open_without_proxy(recording_open, agent.as_ptr(), access, WINHTTP_FLAG_ASYNC);
        assert_eq!(session as usize, 0x5e55);
    }
    let asked = (
        agent.as_ptr() as usize,
        WINHTTP_ACCESS_TYPE_NO_PROXY,
        0,
        0,
        WINHTTP_FLAG_ASYNC,
    );
    let opened = std::mem::take(&mut *OPENED.lock().unwrap());
    assert_eq!(opened, [asked; 4]);

    // A real session, opened with a proxy named: no connection is made
    // for that, and none through it later.
    let session = WinHttpOpen(
        agent.as_ptr(),
        WINHTTP_ACCESS_TYPE_NAMED_PROXY,
        proxy.as_ptr(),
        std::ptr::null(),
        0,
    );
    assert!(!session.is_null(), "WinHttpOpen: {}", win32_error());

    // A proxy set later is accepted, and the host does not hear of it:
    // it would refuse the null handle, as it does for another option.
    /// A `WINHTTP_PROXY_INFO`.
    #[repr(C)]
    struct ProxyInfo {
        access: u32,
        proxy: *const u16,
        bypass: *const u16,
    }
    let info = ProxyInfo {
        access: WINHTTP_ACCESS_TYPE_NAMED_PROXY,
        proxy: proxy.as_ptr(),
        bypass: std::ptr::null(),
    };
    let size = size_of::<ProxyInfo>() as u32;
    let info = (&raw const info).cast::<c_void>();
    assert_eq!(
        WinHttpSetOption(session, WINHTTP_OPTION_PROXY, info, size),
        1
    );
    assert_eq!(WinHttpSetOption(null, WINHTTP_OPTION_PROXY, info, size), 1);
    let timeout = 1000u32;
    clear_win32_error();
    assert_eq!(
        WinHttpSetOption(
            null,
            WINHTTP_OPTION_CONNECT_TIMEOUT,
            (&raw const timeout).cast(),
            4
        ),
        0
    );
    assert_eq!(win32_error(), ERROR_INVALID_HANDLE);
    assert_eq!(
        WinHttpSetOption(
            session,
            WINHTTP_OPTION_CONNECT_TIMEOUT,
            (&raw const timeout).cast(),
            4
        ),
        1
    );

    // Proxy auto-detection fails without asking the network.
    let url = utf16("http://example.com/");
    let mut found = std::ptr::null_mut::<u16>();
    clear_win32_error();
    assert_eq!(WinHttpDetectAutoProxyConfigUrl(3, &mut found), 0);
    assert_eq!(win32_error(), ERROR_WINHTTP_AUTODETECTION_FAILED);
    assert!(found.is_null());
    clear_win32_error();
    assert_eq!(
        WinHttpGetProxyForUrl(session, url.as_ptr(), std::ptr::null(), null),
        0
    );
    assert_eq!(win32_error(), ERROR_WINHTTP_AUTODETECTION_FAILED);
    let failed = ERROR_WINHTTP_AUTODETECTION_FAILED;
    assert_eq!(
        WinHttpGetProxyForUrlEx(null, url.as_ptr(), std::ptr::null(), 0),
        failed
    );
    assert_eq!(
        WinHttpGetProxyForUrlEx2(null, url.as_ptr(), std::ptr::null(), 0, std::ptr::null(), 0),
        failed
    );
    assert_eq!(
        WinHttpGetProxySettingsEx(null, 0, std::ptr::null(), 0),
        failed
    );

    // Loopback is the one server a session can name.
    let close = host!("winhttp.dll", "WinHttpCloseHandle": extern "system" fn(Handle) -> i32);
    let (_listener, port) = listener();
    for server in ["localhost", "127.0.0.1", "[::1]"] {
        let server_name = utf16(server);
        let connection = WinHttpConnect(session, server_name.as_ptr(), port, 0);
        assert!(!connection.is_null(), "{server}: {}", win32_error());
        assert_eq!(close(connection), 1);
    }
    for server in ["example.com", "192.0.2.1", "localhost.example.com"] {
        let server_name = utf16(server);
        clear_win32_error();
        assert!(WinHttpConnect(session, server_name.as_ptr(), port, 0).is_null());
        assert_eq!(win32_error(), ERROR_WINHTTP_CANNOT_CONNECT, "{server}");
    }
    assert_eq!(close(session), 1);
}
