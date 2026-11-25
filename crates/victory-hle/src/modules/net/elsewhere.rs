//! Ways off the machine that are not Winsock or WinHTTP, in DLLs the host
//! policy passes through.
//!
//! * Echo requests (`iphlpapi.dll`: `IcmpSendEcho`, `IcmpSendEcho2`,
//!   `IcmpSendEcho2Ex`, `Icmp6SendEcho2`) to anything but loopback fail,
//!   leaving `IP_DEST_NET_UNREACHABLE` as the last error.
//! * Windows Runtime classes under `Windows.Networking` and `Windows.Web`
//!   (the `api-ms-win-core-winrt-l1` API set: `RoGetActivationFactory`,
//!   `RoActivateInstance`) are not registered: `REGDB_E_CLASSNOTREG`.
//! * Certificate checks (`crypt32.dll`: `CertGetCertificateChain`,
//!   `CertVerifyRevocation`) use what the machine has cached and fetch
//!   nothing.
//!
//! The two failure codes are this runtime's choice for a machine it keeps
//! off the network, not the behaviour of a console.
//!
//! Not vetted, and the host's own: the rest of `iphlpapi.dll` (it tells a
//! title about the machine's real adapters, and `SendARP`,
//! `GetRTTAndHopCount` and `ResolveIpNetEntry2` put packets on the local
//! link), Windows Runtime classes in other name spaces, and COM classes a
//! title makes through `ole32.dll`.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::too_many_arguments)]

use std::ffi::c_void;
use std::net::{IpAddr, Ipv4Addr};

use windows_sys::Win32::Foundation::SetLastError;

use super::addr::{loopback, network_class, socket_addr};
use super::{Handle, once, shown};
use crate::host::host;
use crate::{HleModule, hle_export};

/// `ipexport.h`: `IP_STATUS_BASE` + 2.
pub(super) const IP_DEST_NET_UNREACHABLE: u32 = 11002;
pub(super) const REGDB_E_CLASSNOTREG: i32 = 0x8004_0154_u32 as i32;
pub(super) const CERT_CHAIN_CACHE_ONLY_URL_RETRIEVAL: u32 = 0x0000_0004;
pub(super) const CERT_CHAIN_DISABLE_AUTH_ROOT_AUTO_UPDATE: u32 = 0x0000_0100;
pub(super) const CERT_CHAIN_REVOCATION_CHECK_CACHE_ONLY: u32 = 0x8000_0000;
pub(super) const CERT_VERIFY_CACHE_ONLY_BASED_REVOCATION: u32 = 0x0000_0002;

// Echo requests.

/// True for a loopback destination; otherwise logs it and leaves
/// `IP_DEST_NET_UNREACHABLE` for the thread.
fn echo_stays(destination: Option<IpAddr>) -> bool {
    match destination {
        Some(ip) if loopback(ip) => true,
        ip => {
            once("network echo request blocked", shown(ip));
            // SAFETY: no pointers.
            unsafe { SetLastError(IP_DEST_NET_UNREACHABLE) };
            false
        }
    }
}

/// An `IPAddr`: the address in network byte order.
fn v4(address: u32) -> Option<IpAddr> {
    Some(Ipv4Addr::from(address.to_ne_bytes()).into())
}

/// The address in a `sockaddr_in6`.
fn v6(address: *const u8) -> Option<IpAddr> {
    socket_addr(address, 28).map(|a| a.ip())
}

pub extern "system" fn IcmpSendEcho(
    handle: Handle,
    destination: u32,
    request: *const c_void,
    request_size: u16,
    options: *const c_void,
    reply: *mut c_void,
    reply_size: u32,
    timeout: u32,
) -> u32 {
    if !echo_stays(v4(destination)) {
        return 0;
    }
    host!("iphlpapi.dll", "IcmpSendEcho": extern "system" fn(Handle, u32, *const c_void, u16, *const c_void, *mut c_void, u32, u32) -> u32)(
        handle,
        destination,
        request,
        request_size,
        options,
        reply,
        reply_size,
        timeout,
    )
}

pub extern "system" fn IcmpSendEcho2(
    handle: Handle,
    event: Handle,
    apc: *const c_void,
    apc_context: *mut c_void,
    destination: u32,
    request: *const c_void,
    request_size: u16,
    options: *const c_void,
    reply: *mut c_void,
    reply_size: u32,
    timeout: u32,
) -> u32 {
    if !echo_stays(v4(destination)) {
        return 0;
    }
    host!("iphlpapi.dll", "IcmpSendEcho2": extern "system" fn(Handle, Handle, *const c_void, *mut c_void, u32, *const c_void, u16, *const c_void, *mut c_void, u32, u32) -> u32)(
        handle,
        event,
        apc,
        apc_context,
        destination,
        request,
        request_size,
        options,
        reply,
        reply_size,
        timeout,
    )
}

pub extern "system" fn IcmpSendEcho2Ex(
    handle: Handle,
    event: Handle,
    apc: *const c_void,
    apc_context: *mut c_void,
    source: u32,
    destination: u32,
    request: *const c_void,
    request_size: u16,
    options: *const c_void,
    reply: *mut c_void,
    reply_size: u32,
    timeout: u32,
) -> u32 {
    if !echo_stays(v4(destination)) {
        return 0;
    }
    host!("iphlpapi.dll", "IcmpSendEcho2Ex": extern "system" fn(Handle, Handle, *const c_void, *mut c_void, u32, u32, *const c_void, u16, *const c_void, *mut c_void, u32, u32) -> u32)(
        handle,
        event,
        apc,
        apc_context,
        source,
        destination,
        request,
        request_size,
        options,
        reply,
        reply_size,
        timeout,
    )
}

/// `source` and `destination` are `sockaddr_in6`.
pub extern "system" fn Icmp6SendEcho2(
    handle: Handle,
    event: Handle,
    apc: *const c_void,
    apc_context: *mut c_void,
    source: *const u8,
    destination: *const u8,
    request: *const c_void,
    request_size: u16,
    options: *const c_void,
    reply: *mut c_void,
    reply_size: u32,
    timeout: u32,
) -> u32 {
    if !echo_stays(v6(destination)) {
        return 0;
    }
    host!("iphlpapi.dll", "Icmp6SendEcho2": extern "system" fn(Handle, Handle, *const c_void, *mut c_void, *const u8, *const u8, *const c_void, u16, *const c_void, *mut c_void, u32, u32) -> u32)(
        handle,
        event,
        apc,
        apc_context,
        source,
        destination,
        request,
        request_size,
        options,
        reply,
        reply_size,
        timeout,
    )
}

// The Windows Runtime.

/// True for a class (an `HSTRING`) that works over the network, which it
/// logs.
fn network_class_id(class: Handle) -> bool {
    let mut len = 0u32;
    let text = host!("combase.dll", "WindowsGetStringRawBuffer": extern "system" fn(Handle, *mut u32) -> *mut u16)(
        class, &mut len,
    );
    if text.is_null() {
        return false;
    }
    // SAFETY: the string's `len` UTF-16 units, as the host just said.
    let name = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len as usize) });
    let blocked = network_class(&name);
    if blocked {
        once("network runtime class blocked", name);
    }
    blocked
}

pub extern "system" fn RoGetActivationFactory(
    class: Handle,
    iid: *const c_void,
    factory: *mut *mut c_void,
) -> i32 {
    if network_class_id(class) {
        if !factory.is_null() {
            // SAFETY: the title's place for the factory.
            unsafe { factory.write(std::ptr::null_mut()) };
        }
        return REGDB_E_CLASSNOTREG;
    }
    host!("combase.dll", "RoGetActivationFactory": extern "system" fn(Handle, *const c_void, *mut *mut c_void) -> i32)(
        class, iid, factory,
    )
}

pub extern "system" fn RoActivateInstance(class: Handle, instance: *mut *mut c_void) -> i32 {
    if network_class_id(class) {
        if !instance.is_null() {
            // SAFETY: the title's place for the object.
            unsafe { instance.write(std::ptr::null_mut()) };
        }
        return REGDB_E_CLASSNOTREG;
    }
    host!("combase.dll", "RoActivateInstance": extern "system" fn(Handle, *mut *mut c_void) -> i32)(
        class, instance,
    )
}

// Certificates.

pub(super) type GetChain = extern "system" fn(
    Handle,
    *const c_void,
    *const c_void,
    Handle,
    *const c_void,
    u32,
    *const c_void,
    *mut *const c_void,
) -> i32;

pub(super) type VerifyRevocation =
    extern "system" fn(u32, u32, u32, *const *const c_void, u32, *const c_void, *mut c_void) -> i32;

/// Building a chain may fetch issuer certificates, trust lists and roots
/// (from URLs in the certificate, and from Windows Update) and revocation
/// lists or OCSP answers. The three flags turn each of those into a look
/// in the machine's cache (learn.microsoft.com,
/// wincrypt/nf-wincrypt-certgetcertificatechain); the chain then says what
/// it could not check, as on a machine that is offline.
pub extern "system" fn CertGetCertificateChain(
    engine: Handle,
    certificate: *const c_void,
    time: *const c_void,
    store: Handle,
    parameters: *const c_void,
    flags: u32,
    reserved: *const c_void,
    chain: *mut *const c_void,
) -> i32 {
    let get = host!("crypt32.dll", "CertGetCertificateChain": extern "system" fn(Handle, *const c_void, *const c_void, Handle, *const c_void, u32, *const c_void, *mut *const c_void) -> i32);
    chain_from_cache(
        get,
        engine,
        certificate,
        time,
        store,
        parameters,
        flags,
        reserved,
        chain,
    )
}

/// [`CertGetCertificateChain`] with the host's function as a parameter, so
/// that a test can see what the host is asked for.
pub(super) fn chain_from_cache(
    get: GetChain,
    engine: Handle,
    certificate: *const c_void,
    time: *const c_void,
    store: Handle,
    parameters: *const c_void,
    flags: u32,
    reserved: *const c_void,
    chain: *mut *const c_void,
) -> i32 {
    let cached = flags
        | CERT_CHAIN_CACHE_ONLY_URL_RETRIEVAL
        | CERT_CHAIN_DISABLE_AUTH_ROOT_AUTO_UPDATE
        | CERT_CHAIN_REVOCATION_CHECK_CACHE_ONLY;
    get(
        engine,
        certificate,
        time,
        store,
        parameters,
        cached,
        reserved,
        chain,
    )
}

/// A revocation check asks the servers a certificate names; the flag
/// keeps the handler from "accessing any network-based resources"
/// (learn.microsoft.com, wincrypt/nf-wincrypt-certverifyrevocation).
pub extern "system" fn CertVerifyRevocation(
    encoding: u32,
    kind: u32,
    count: u32,
    contexts: *const *const c_void,
    flags: u32,
    parameters: *const c_void,
    status: *mut c_void,
) -> i32 {
    let verify = host!("crypt32.dll", "CertVerifyRevocation": extern "system" fn(u32, u32, u32, *const *const c_void, u32, *const c_void, *mut c_void) -> i32);
    revocation_from_cache(
        verify, encoding, kind, count, contexts, flags, parameters, status,
    )
}

/// [`CertVerifyRevocation`] with the host's function as a parameter.
pub(super) fn revocation_from_cache(
    verify: VerifyRevocation,
    encoding: u32,
    kind: u32,
    count: u32,
    contexts: *const *const c_void,
    flags: u32,
    parameters: *const c_void,
    status: *mut c_void,
) -> i32 {
    verify(
        encoding,
        kind,
        count,
        contexts,
        flags | CERT_VERIFY_CACHE_ONLY_BASED_REVOCATION,
        parameters,
        status,
    )
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "net-elsewhere",
        dlls: &["iphlpapi.dll", "crypt32.dll", "api-ms-win-core-winrt-l1-*"],
        exports: vec![
            hle_export!(IcmpSendEcho),
            hle_export!(IcmpSendEcho2),
            hle_export!(IcmpSendEcho2Ex),
            hle_export!(Icmp6SendEcho2),
            hle_export!(RoGetActivationFactory),
            hle_export!(RoActivateInstance),
            hle_export!(CertGetCertificateChain),
            hle_export!(CertVerifyRevocation),
        ],
    }
}
