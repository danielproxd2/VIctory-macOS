//! Reading the addresses a title passes to Winsock, and telling loopback
//! from the network, by address and by Windows Runtime class name. No host
//! calls, so it builds and is tested everywhere.

use std::ffi::c_char;
use std::net::{IpAddr, SocketAddr};

pub(crate) const AF_INET: u16 = 2;
pub(crate) const AF_INET6: u16 = 23;

/// An IPv4 or IPv6 `sockaddr` of `len` bytes.
pub(crate) fn socket_addr(addr: *const u8, len: i32) -> Option<SocketAddr> {
    let len = usize::try_from(len).ok()?;
    if addr.is_null() || len < 4 {
        return None;
    }
    // SAFETY: the title passes `len` readable bytes.
    let b = unsafe { std::slice::from_raw_parts(addr, len) };
    let port = u16::from_be_bytes([b[2], b[3]]);
    match u16::from_le_bytes([b[0], b[1]]) {
        AF_INET if len >= 16 => Some((<[u8; 4]>::try_from(&b[4..8]).ok()?, port).into()),
        AF_INET6 if len >= 28 => Some((<[u8; 16]>::try_from(&b[8..24]).ok()?, port).into()),
        _ => None,
    }
}

/// A raw address of family `kind` (`gethostbyaddr` style).
pub(crate) fn raw_ip(addr: *const c_char, len: i32, kind: i32) -> Option<IpAddr> {
    let len = usize::try_from(len).ok()?;
    if addr.is_null() {
        return None;
    }
    // SAFETY: the title passes `len` readable bytes.
    let b = unsafe { std::slice::from_raw_parts(addr.cast::<u8>(), len) };
    match (u16::try_from(kind).ok()?, len) {
        (AF_INET, 4..) => Some(<[u8; 4]>::try_from(&b[..4]).ok()?.into()),
        (AF_INET6, 16..) => Some(<[u8; 16]>::try_from(&b[..16]).ok()?.into()),
        _ => None,
    }
}

pub(crate) fn loopback(ip: IpAddr) -> bool {
    ip.to_canonical().is_loopback()
}

/// `localhost` or a loopback literal (bracketed IPv6 too).
pub(crate) fn loopback_name(name: &str) -> bool {
    let bare = name.trim_start_matches('[').trim_end_matches(']');
    bare.eq_ignore_ascii_case("localhost") || bare.parse().is_ok_and(loopback)
}

/// A Windows Runtime class that works over the network, by its name:
/// sockets, connectivity and proximity (`Windows.Networking.*`), HTTP, feeds
/// and publishing (`Windows.Web.*`).
pub(crate) fn network_class(class: &str) -> bool {
    ["Windows.Networking.", "Windows.Web."].iter().any(|space| {
        class
            .get(..space.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(space))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `sockaddr_in` for `ip:port`.
    fn v4(ip: [u8; 4], port: u16) -> [u8; 16] {
        let mut a = [0u8; 16];
        a[..2].copy_from_slice(&AF_INET.to_le_bytes());
        a[2..4].copy_from_slice(&port.to_be_bytes());
        a[4..8].copy_from_slice(&ip);
        a
    }

    /// A `sockaddr_in6` for `[ip]:port`.
    fn v6(ip: [u8; 16], port: u16) -> [u8; 28] {
        let mut a = [0u8; 28];
        a[..2].copy_from_slice(&AF_INET6.to_le_bytes());
        a[2..4].copy_from_slice(&port.to_be_bytes());
        a[8..24].copy_from_slice(&ip);
        a
    }

    fn len(bytes: &[u8]) -> i32 {
        i32::try_from(bytes.len()).unwrap()
    }

    #[test]
    fn socket_addr_reads_family_port_and_address() {
        let a = v4([127, 0, 0, 1], 8080);
        assert_eq!(
            socket_addr(a.as_ptr(), len(&a)),
            Some("127.0.0.1:8080".parse().unwrap())
        );
        let mut ip = [0u8; 16];
        ip[15] = 1;
        let a = v6(ip, 443);
        assert_eq!(
            socket_addr(a.as_ptr(), len(&a)),
            Some("[::1]:443".parse().unwrap())
        );
        // Bytes past the structure change nothing.
        let mut long = [0xffu8; 40];
        long[..16].copy_from_slice(&v4([10, 1, 2, 3], 1));
        assert_eq!(
            socket_addr(long.as_ptr(), len(&long)),
            Some("10.1.2.3:1".parse().unwrap())
        );
    }

    #[test]
    fn socket_addr_rejects_short_and_negative_lengths() {
        let a = v4([127, 0, 0, 1], 80);
        assert_eq!(socket_addr(std::ptr::null(), 16), None);
        assert_eq!(socket_addr(a.as_ptr(), -1), None);
        assert_eq!(socket_addr(a.as_ptr(), i32::MIN), None);
        // Each prefix of a `sockaddr_in` that is not all of it.
        for short in 0..16 {
            assert_eq!(socket_addr(a.as_ptr(), short), None, "{short} bytes");
        }
        assert!(socket_addr(a.as_ptr(), 16).is_some());
        let a = v6([0; 16], 80);
        for short in 0..28 {
            assert_eq!(socket_addr(a.as_ptr(), short), None, "{short} bytes");
        }
        assert!(socket_addr(a.as_ptr(), 28).is_some());
        // Another address family, whatever its length.
        let mut other = [0u8; 28];
        other[..2].copy_from_slice(&1u16.to_le_bytes());
        assert_eq!(socket_addr(other.as_ptr(), 28), None);
    }

    #[test]
    fn raw_ip_table() {
        let four = [127u8, 0, 0, 1, 0xff];
        let mut sixteen = [0u8; 16];
        sixteen[15] = 1;
        let ip =
            |bytes: &[u8], len: i32, kind: u16| raw_ip(bytes.as_ptr().cast(), len, kind.into());
        assert_eq!(ip(&four, 4, AF_INET), Some("127.0.0.1".parse().unwrap()));
        // A longer buffer still holds the address.
        assert_eq!(ip(&four, 5, AF_INET), Some("127.0.0.1".parse().unwrap()));
        assert_eq!(ip(&sixteen, 16, AF_INET6), Some("::1".parse().unwrap()));
        // Too short for the family, a negative length, another family.
        assert_eq!(ip(&four, 3, AF_INET), None);
        assert_eq!(ip(&sixteen, 15, AF_INET6), None);
        assert_eq!(ip(&four, 4, AF_INET6), None);
        assert_eq!(ip(&four, -4, AF_INET), None);
        assert_eq!(ip(&four, 4, 1), None);
        assert_eq!(raw_ip(four.as_ptr().cast(), 4, -2), None);
        assert_eq!(raw_ip(four.as_ptr().cast(), 4, 0x1_0002), None);
        assert_eq!(raw_ip(std::ptr::null(), 4, AF_INET.into()), None);
    }

    #[test]
    fn loopback_names_table() {
        for name in [
            "localhost",
            "LocalHost",
            "127.0.0.1",
            "127.255.255.254",
            "::1",
            "[::1]",
            "::ffff:127.0.0.1",
        ] {
            assert!(loopback_name(name), "{name}");
        }
        for name in [
            "",
            "example.com",
            "localhost.example.com",
            "notlocalhost",
            "0.0.0.0",
            "10.0.0.1",
            "192.168.1.1",
            "128.0.0.1",
            "::",
            "[::2]",
            "::ffff:10.0.0.1",
            "127.0.0.1:80",
            "127.0.0.1.example.com",
        ] {
            assert!(!loopback_name(name), "{name}");
        }
    }

    #[test]
    fn loopback_covers_mapped_ipv4() {
        assert!(loopback("127.9.9.9".parse().unwrap()));
        assert!(loopback("::ffff:127.0.0.1".parse().unwrap()));
        assert!(!loopback("::ffff:192.168.0.1".parse().unwrap()));
        assert!(!loopback("169.254.0.1".parse().unwrap()));
    }

    #[test]
    fn network_classes_table() {
        for class in [
            "Windows.Networking.Sockets.StreamSocket",
            "Windows.Networking.Sockets.DatagramSocket",
            "Windows.Networking.Sockets.MessageWebSocket",
            "Windows.Networking.Connectivity.NetworkInformation",
            "Windows.Networking.BackgroundTransfer.BackgroundDownloader",
            "Windows.Networking.HostName",
            "Windows.Web.Http.HttpClient",
            "Windows.Web.Http.Filters.HttpBaseProtocolFilter",
            "Windows.Web.Syndication.SyndicationClient",
            "windows.web.http.httpclient",
        ] {
            assert!(network_class(class), "{class}");
        }
        for class in [
            "",
            "Windows.Networking",
            "Windows.Web",
            "Windows.WebUI.Core.WebUICommandBar",
            "Windows.NetworkingX.Thing",
            "Windows.Foundation.Uri",
            "Windows.Globalization.Calendar",
            "Windows.Storage.Streams.Buffer",
            "Contoso.Windows.Networking.Sockets.StreamSocket",
            // No character boundary where the name space would end.
            "Windows.Networking\u{e9}X",
        ] {
            assert!(!network_class(class), "{class}");
        }
    }
}
