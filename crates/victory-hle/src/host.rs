//! Pass-through to the host's own implementation of shared APIs.

use victory_pe::ImportSymbol;

/// Which DLLs may be satisfied by the host's own implementation.
///
/// Console titles link mostly against Windows API sets that behave the same
/// on PC. Passing those through lets bring-up focus on what really differs.
/// Anything whose semantics differ on console (memory, files, threads) is
/// overridden in `modules`, which always wins over the host.
#[derive(Debug, Clone)]
pub struct HostPolicy {
    /// Lower-case patterns; a trailing `*` matches a prefix.
    pub allow: Vec<String>,
}

impl Default for HostPolicy {
    fn default() -> Self {
        let allow = [
            // Includes the Windows Runtime's activation functions, whose
            // network classes `modules::net::elsewhere` refuses.
            "api-ms-win-core-*",
            "api-ms-win-eventing-*",
            "api-ms-win-security-*",
            "api-ms-win-crt-*",
            "kernel32.dll",
            "kernelbase.dll",
            "ntdll.dll",
            "ucrtbase.dll",
            "vcruntime140*",
            "msvcp140*",
            // Off-machine traffic is refused in `modules::net`: Winsock,
            // WinHTTP, and what a certificate check would fetch.
            "ws2_32.dll",
            "bcrypt.dll",
            "crypt32.dll",
            "ncrypt.dll",
            "winhttp.dll",
            "sspicli.dll",
            "cabinet.dll",
            "avrt.dll",
            // Window, message and shell APIs: the host's desktop window
            // stands in for the console's single core window.
            "ext-ms-win-rtcore-ntuser-*",
            "api-ms-win-shcore-*",
            "msvcp_win.dll",
            "ole32.dll",
            "advapi32.dll",
            // Echo requests are refused in `modules::net::elsewhere`.
            "iphlpapi.dll",
            // The title's crash handler writes minidumps with it (loaded by name).
            "dbghelp.dll",
            // Same APIs as the console's (XAudio2 ordinal 1 is XAudio2Create
            // on both); Media Foundation reads local media files.
            "xaudio2_9.dll",
            "mfplat.dll",
            "mfreadwrite.dll",
        ];
        HostPolicy {
            allow: allow.iter().map(|s| s.to_string()).collect(),
        }
    }
}

impl HostPolicy {
    /// No pass-through at all: shows the full surface a title needs.
    pub fn none() -> Self {
        HostPolicy { allow: Vec::new() }
    }

    pub fn allows(&self, dll: &str) -> bool {
        let dll = dll.to_ascii_lowercase();
        self.allow
            .iter()
            .any(|pattern| match pattern.strip_suffix('*') {
                Some(prefix) => dll.starts_with(prefix),
                None => dll == *pattern,
            })
    }

    pub(crate) fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Option<usize> {
        if !self.allows(dll) {
            return None;
        }
        imp::resolve(dll, symbol)
    }
}

/// The host's own `name` from the System32 DLL `dll`: what a title import
/// of it is bound to when we do not override it.
pub(crate) fn function(dll: &str, name: &str) -> Option<usize> {
    let name = ImportSymbol::Name {
        hint: 0,
        name: name.into(),
    };
    imp::resolve(dll, &name)
}

/// The host's own function behind a title import we override: what the
/// import would have been bound to. Every binding is listed in its
/// module's `HOST_IMPORTS`, which [`missing_exports`] checks at start-up.
///
/// A host that lacks the export (Wine lacks a few) must not take the
/// process down on the first call, which arrives from title code across
/// `extern "system"`: the binding then is a stand-in with the same
/// signature that fails the call the way its family does ([`failure`]).
#[cfg(windows)]
macro_rules! host {
    ($dll:literal, $name:literal: extern $abi:literal fn($($arg:ty),* $(,)?) $(-> $ret:ty)?) => {{
        extern $abi fn lacking($(_: $arg),*) $(-> $ret)? {
            crate::host::lacking($dll, $name)
        }
        static ADDRESS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        let address = *ADDRESS.get_or_init(|| {
            let lacking: extern $abi fn($($arg),*) $(-> $ret)? = lacking;
            crate::host::function_or($dll, $name, lacking as usize)
        });
        // SAFETY: the host exports this function with this signature; the
        // stand-in has it by construction.
        unsafe { std::mem::transmute::<usize, extern $abi fn($($arg),*) $(-> $ret)?>(address) }
    }};
}
#[cfg(windows)]
pub(crate) use host;

/// The host's `name` from `dll`, or `stand_in` with a line in the log.
#[cfg(windows)]
pub(crate) fn function_or(dll: &str, name: &str, stand_in: usize) -> usize {
    function(dll, name).unwrap_or_else(|| {
        tracing::error!(
            dll,
            name,
            "the host lacks a function we call; calls to it fail"
        );
        stand_in
    })
}

/// The bindings of [`host!`] the host cannot satisfy, as (DLL, name).
pub(crate) fn missing_exports() -> Vec<(&'static str, &'static str)> {
    #[cfg(windows)]
    let tables = {
        use crate::modules::{files, net, xsystem};
        [
            files::HOST_IMPORTS,
            net::HOST_IMPORTS,
            xsystem::HOST_IMPORTS,
        ]
    };
    #[cfg(not(windows))]
    let tables: [&[(&str, &str)]; 0] = [];
    tables
        .iter()
        .flat_map(|table| table.iter().copied())
        .filter(|(dll, name)| function(dll, name).is_none())
        .collect()
}

/// The error a failed call leaves for its thread.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThreadError {
    /// `SetLastError`.
    Win32(u32),
    /// `WSASetLastError`.
    Winsock(i32),
    /// The C runtime's `errno`.
    Errno(i32),
}

#[cfg(any(windows, test))]
const ERROR_PROC_NOT_FOUND: u32 = 127;
#[cfg(any(windows, test))]
const WSAEOPNOTSUPP: i32 = 10045;
/// What `getaddrinfo` returns for a failure that will not go away.
#[cfg(any(windows, test))]
const WSANO_RECOVERY: i32 = 11003;
/// The C runtime's "function not implemented".
#[cfg(any(windows, test))]
const ENOSYS: i32 = 40;

/// How a call to `dll!name` fails when the host has no such function: the
/// error left for the thread and the return value, sign-extended. By the
/// conventions of the public references: Win32 functions return `FALSE`
/// or an invalid handle and set the last error, registry functions return
/// the error, Winsock functions return `SOCKET_ERROR`, an invalid socket
/// or null and set the Winsock error, name resolution returns its error
/// code, and C runtime functions return -1, null or an `errno_t` and set
/// `errno`, and Windows Runtime functions return a failure `HRESULT` (a
/// string buffer query returns null). A stand-in does not write through
/// the call's pointers.
#[cfg(any(windows, test))]
pub(crate) fn failure(dll: &str, name: &str) -> (ThreadError, i64) {
    let starts = |prefix: &str| name.starts_with(prefix);
    match dll {
        "ws2_32.dll" => {
            let value = match name {
                _ if starts("getaddrinfo") || starts("GetAddrInfo") => WSANO_RECOVERY.into(),
                _ if starts("getnameinfo") || starts("GetNameInfo") => WSANO_RECOVERY.into(),
                _ if starts("gethostby") || starts("WSAAsyncGetHostBy") => 0,
                "WSAGetLastError" => WSAEOPNOTSUPP.into(),
                _ => -1,
            };
            (ThreadError::Winsock(WSAEOPNOTSUPP), value)
        }
        "advapi32.dll" => (
            ThreadError::Win32(ERROR_PROC_NOT_FOUND),
            ERROR_PROC_NOT_FOUND.into(),
        ),
        "combase.dll" => (
            ThreadError::Win32(ERROR_PROC_NOT_FOUND),
            if starts("Ro") {
                // HRESULT_FROM_WIN32(ERROR_PROC_NOT_FOUND), sign-extended.
                (0x8007_0000_u32 | ERROR_PROC_NOT_FOUND) as i32 as i64
            } else {
                0
            },
        ),
        "ucrtbase.dll" | "msvcp140.dll" => {
            let value = match name {
                _ if name.ends_with("_s") => ENOSYS.into(),
                "fopen" | "_wfopen" | "_wfsopen" | "fread" => 0,
                _ if dll == "msvcp140.dll" => 0,
                _ => -1,
            };
            (ThreadError::Errno(ENOSYS), value)
        }
        _ => {
            let invalid = starts("CreateFile")
                || starts("FindFirstFile")
                || matches!(name, "GetFileAttributesW" | "GetFileAttributesA");
            let value = match name {
                // Returns an `HRESULT`, for which 0 would say it worked:
                // `HRESULT_FROM_WIN32(ERROR_PROC_NOT_FOUND)`.
                "CopyFile2" => i64::from(0x8007_007F_u32 as i32),
                _ if invalid => -1,
                _ => 0,
            };
            (ThreadError::Win32(ERROR_PROC_NOT_FOUND), value)
        }
    }
}

/// A return type of a host function, made from [`failure`]'s value.
#[cfg(windows)]
pub(crate) trait Answer {
    fn answer(value: i64) -> Self;
}

#[cfg(windows)]
impl Answer for () {
    fn answer(_: i64) {}
}

#[cfg(windows)]
impl Answer for i32 {
    fn answer(value: i64) -> i32 {
        value as i32
    }
}

#[cfg(windows)]
impl Answer for isize {
    fn answer(value: i64) -> isize {
        value as isize
    }
}

#[cfg(windows)]
impl Answer for u32 {
    fn answer(value: i64) -> u32 {
        value as u32
    }
}

#[cfg(windows)]
impl Answer for usize {
    fn answer(value: i64) -> usize {
        value as usize
    }
}

#[cfg(windows)]
impl<T> Answer for *mut T {
    fn answer(value: i64) -> *mut T {
        std::ptr::without_provenance_mut(value as usize)
    }
}

/// A call to `dll!name`, which the host lacks: fails it as [`failure`]
/// says.
#[cfg(windows)]
pub(crate) fn lacking<R: Answer>(dll: &str, name: &str) -> R {
    let (error, value) = failure(dll, name);
    imp::leave(error);
    R::answer(value)
}

/// The (DLL, name) of every [`host!`] binding written in `source`.
#[cfg(test)]
pub(crate) fn bindings_in(source: &str) -> std::collections::BTreeSet<(String, String)> {
    /// The text of a string literal at the start of `text`, and the rest.
    fn literal(text: &str) -> Option<(&str, &str)> {
        let text = text.trim_start().strip_prefix('"')?;
        let end = text.find('"')?;
        Some((&text[..end], &text[end + 1..]))
    }
    source
        .split("host!(")
        .skip(1)
        .filter_map(|after| {
            let (dll, rest) = literal(after)?;
            let (name, _) = literal(rest.trim_start().strip_prefix(',')?)?;
            Some((dll.to_owned(), name.to_owned()))
        })
        .collect()
}

/// Checks a module's `HOST_IMPORTS` against its `source` (every binding
/// listed, in order, once) and against the host: every function exists,
/// except exactly `wine_lacks` under Wine.
#[cfg(all(test, windows))]
pub(crate) fn check_bindings(source: &str, table: &[(&str, &str)], wine_lacks: &[(&str, &str)]) {
    let written = bindings_in(source);
    let listed: Vec<_> = table
        .iter()
        .map(|(dll, name)| (dll.to_string(), name.to_string()))
        .collect();
    assert!(listed.is_sorted(), "HOST_IMPORTS is kept sorted");
    assert_eq!(
        listed,
        written.into_iter().collect::<Vec<_>>(),
        "HOST_IMPORTS lists the bindings in the source"
    );
    let wine = function("ntdll.dll", "wine_get_version").is_some();
    let lacking: Vec<_> = table
        .iter()
        .copied()
        .filter(|(dll, name)| function(dll, name).is_none())
        .collect();
    let expected = if wine { wine_lacks } else { &[] };
    assert_eq!(lacking, expected, "the host lacks exactly these");
    for (dll, name) in lacking {
        assert!(missing_exports().contains(&(dll, name)));
    }
}

#[cfg(windows)]
mod imp {
    use std::collections::HashMap;
    use std::ffi::CString;
    use std::sync::{LazyLock, Mutex};

    use victory_pe::ImportSymbol;
    use windows_sys::Win32::System::LibraryLoader::{
        GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExA,
    };

    /// Module handles by lower-case DLL name (stored as usize: Send + Sync).
    static MODULES: LazyLock<Mutex<HashMap<String, usize>>> = LazyLock::new(Default::default);

    pub(super) fn resolve(dll: &str, symbol: &ImportSymbol) -> Option<usize> {
        let module = module(dll)?;
        let proc = match symbol {
            ImportSymbol::Name { name, .. } => {
                let name = CString::new(name.as_str()).ok()?;
                // SAFETY: valid module handle, NUL-terminated name.
                unsafe { GetProcAddress(module as _, name.as_ptr().cast()) }
            }
            // SAFETY: MAKEINTRESOURCE-style ordinal lookup on a valid module.
            ImportSymbol::Ordinal(o) => unsafe { GetProcAddress(module as _, *o as usize as _) },
        };
        proc.map(|f| f as *const () as usize)
    }

    /// Leaves `error` for the calling thread.
    pub(super) fn leave(error: super::ThreadError) {
        use windows_sys::Win32::Foundation::SetLastError;
        match error {
            // SAFETY: sets the thread's last error.
            super::ThreadError::Win32(code) => unsafe { SetLastError(code) },
            // The Winsock error is the thread's last error; through
            // Winsock's own function where the host has it.
            super::ThreadError::Winsock(code) => {
                match super::function("ws2_32.dll", "WSASetLastError") {
                    // SAFETY: the host's `void WSASetLastError(int)`.
                    Some(set) => unsafe {
                        std::mem::transmute::<usize, extern "system" fn(i32)>(set)(code)
                    },
                    // SAFETY: as above.
                    None => unsafe { SetLastError(code as u32) },
                }
            }
            super::ThreadError::Errno(code) => {
                if let Some(set) = super::function("ucrtbase.dll", "_set_errno") {
                    // SAFETY: the host's `errno_t _set_errno(int)`.
                    unsafe { std::mem::transmute::<usize, extern "C" fn(i32) -> i32>(set)(code) };
                }
            }
        }
    }

    fn module(dll: &str) -> Option<usize> {
        let key = dll.to_ascii_lowercase();
        let mut cache = MODULES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(&m) = cache.get(&key) {
            return Some(m);
        }
        let name = CString::new(key.as_str()).ok()?;
        // System32 only: never pick up a same-named DLL from the current folder or PATH.
        // SAFETY: NUL-terminated DLL name; only allow-listed system DLLs get here.
        let handle = unsafe {
            LoadLibraryExA(
                name.as_ptr().cast(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        } as usize;
        (handle != 0).then(|| *cache.entry(key).or_insert(handle))
    }
}

#[cfg(not(windows))]
mod imp {
    pub(super) fn resolve(_dll: &str, _symbol: &victory_pe::ImportSymbol) -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the default policy passes through to the host, by DLL name:
    /// whole names, and prefixes for the patterns that end in `*`.
    #[test]
    fn pass_through_policy_table() {
        let policy = HostPolicy::default();
        for dll in [
            "kernel32.dll",
            "KERNEL32.DLL",
            "KernelBase.dll",
            "ntdll.dll",
            "ucrtbase.dll",
            "ws2_32.dll",
            "WS2_32.dll",
            "winhttp.dll",
            "crypt32.dll",
            "iphlpapi.dll",
            "IPHLPAPI.DLL",
            "ole32.dll",
            "xaudio2_9.dll",
            // Patterns: versioned API sets and runtime libraries.
            "api-ms-win-core-synch-l1-2-0.dll",
            "API-MS-Win-Core-File-l1-1-0.dll",
            "api-ms-win-core-winrt-l1-1-0.dll",
            "api-ms-win-core-winrt-string-l1-1-0.dll",
            "api-ms-win-crt-runtime-l1-1-0.dll",
            "api-ms-win-eventing-provider-l1-1-0.dll",
            "api-ms-win-security-base-l1-1-0.dll",
            "api-ms-win-shcore-scaling-l1-1-1.dll",
            "ext-ms-win-rtcore-ntuser-window-l1-1-0.dll",
            "vcruntime140.dll",
            "vcruntime140_1.dll",
            "msvcp140.dll",
            "msvcp140_atomic_wait.dll",
        ] {
            assert!(policy.allows(dll), "{dll}");
        }
        for dll in [
            "",
            "*",
            // Console-only: ours, or missing.
            "xgameruntime.dll",
            "xmem.dll",
            "d3d12_xs.dll",
            // Other ways onto the network, which nothing here vets.
            "wininet.dll",
            "urlmon.dll",
            "dnsapi.dll",
            "mswsock.dll",
            "wsock32.dll",
            "icmp.dll",
            "cryptnet.dll",
            "combase.dll",
            "webio.dll",
            "httpapi.dll",
            "netapi32.dll",
            "rasapi32.dll",
            "wlanapi.dll",
            "bluetoothapis.dll",
            "msxml6.dll",
            // Desktop surface a console title has no use for.
            "user32.dll",
            "shell32.dll",
            // A whole name is not a prefix, and a prefix is not a suffix.
            "ws2_32.dll.dll",
            "kernel32",
            "my-kernel32.dll",
            "xapi-ms-win-core-synch-l1-2-0.dll",
            "api-ms-win-net-isolation-l1-1-0.dll",
            "ext-ms-win-ntuser-window-l1-1-0.dll",
            "xaudio2_8.dll",
        ] {
            assert!(!policy.allows(dll), "{dll}");
        }
        // The policy that passes nothing through.
        let none = HostPolicy::none();
        for dll in ["kernel32.dll", "api-ms-win-core-synch-l1-2-0.dll", ""] {
            assert!(!none.allows(dll), "{dll}");
        }
        // A pattern is the title's DLL name in lower case.
        let custom = HostPolicy {
            allow: vec!["one.dll".into(), "two-*".into()],
        };
        assert!(custom.allows("ONE.dll") && custom.allows("Two-l1.dll"));
        assert!(!custom.allows("two.dll") && !custom.allows("one.dll2"));
        // Where there is no host to ask, nothing resolves either way.
        #[cfg(not(windows))]
        assert_eq!(
            policy.resolve(
                "kernel32.dll",
                &ImportSymbol::Name {
                    hint: 0,
                    name: "Sleep".into()
                }
            ),
            None
        );
    }

    #[test]
    fn bindings_are_read_from_source_text() {
        let source = r#"
            host!("a.dll", "One": extern "system" fn() -> i32)();
            let f = host!(
                "b.dll",
                "Two": extern "C" fn(i32)
            );
            host!("a.dll", "One": extern "system" fn() -> i32)();
            // The macro itself, `host!`, and host!(not a literal) are not bindings.
        "#;
        let found: Vec<_> = bindings_in(source).into_iter().collect();
        let pair = |dll: &str, name: &str| (dll.to_owned(), name.to_owned());
        assert_eq!(found, [pair("a.dll", "One"), pair("b.dll", "Two")]);
    }

    /// How a call fails when the host lacks the function, per family.
    #[test]
    fn a_lacking_function_fails_as_its_family_does() {
        let win32 = ThreadError::Win32(127);
        assert_eq!(failure("kernel32.dll", "DeleteFileW"), (win32, 0));
        assert_eq!(failure("kernel32.dll", "CreateFile2"), (win32, -1));
        assert_eq!(failure("kernel32.dll", "FindFirstFileExW"), (win32, -1));
        assert_eq!(failure("kernel32.dll", "GetFileAttributesA"), (win32, -1));
        assert_eq!(failure("kernel32.dll", "GetFileAttributesExW"), (win32, 0));
        assert_eq!(failure("kernel32.dll", "GetFullPathNameW"), (win32, 0));
        assert_eq!(failure("kernel32.dll", "FindFirstFileExA"), (win32, -1));
        // An `HRESULT`: never 0, which would say the copy was made.
        let not_found = i64::from(0x8007_007F_u32 as i32);
        assert_eq!(failure("kernel32.dll", "CopyFile2"), (win32, not_found));
        assert!(not_found < 0, "a failure HRESULT");
        assert_eq!(failure("winhttp.dll", "WinHttpOpen"), (win32, 0));
        assert_eq!(failure("advapi32.dll", "RegOpenKeyExW"), (win32, 127));
        assert_eq!(failure("iphlpapi.dll", "IcmpSendEcho"), (win32, 0));
        assert_eq!(
            failure("crypt32.dll", "CertGetCertificateChain"),
            (win32, 0)
        );
        // A Windows Runtime call must not look like it succeeded (S_OK is 0).
        let not_found = i64::from(0x8007_007F_u32 as i32);
        assert_eq!(
            failure("combase.dll", "RoGetActivationFactory"),
            (win32, not_found)
        );
        assert_eq!(
            failure("combase.dll", "RoActivateInstance"),
            (win32, not_found)
        );
        assert_eq!(
            failure("combase.dll", "WindowsGetStringRawBuffer"),
            (win32, 0)
        );

        let winsock = ThreadError::Winsock(10045);
        assert_eq!(failure("ws2_32.dll", "connect"), (winsock, -1));
        assert_eq!(failure("ws2_32.dll", "WSAJoinLeaf"), (winsock, -1));
        assert_eq!(failure("ws2_32.dll", "GetAddrInfoExA"), (winsock, 11003));
        assert_eq!(failure("ws2_32.dll", "getnameinfo"), (winsock, 11003));
        assert_eq!(failure("ws2_32.dll", "gethostbyname"), (winsock, 0));
        assert_eq!(failure("ws2_32.dll", "WSAAsyncGetHostByAddr"), (winsock, 0));

        let errno = ThreadError::Errno(40);
        assert_eq!(failure("ucrtbase.dll", "_wopen"), (errno, -1));
        assert_eq!(failure("ucrtbase.dll", "_wfindfirst64"), (errno, -1));
        assert_eq!(failure("ucrtbase.dll", "fclose"), (errno, -1));
        assert_eq!(failure("ucrtbase.dll", "_wfopen"), (errno, 0));
        assert_eq!(failure("ucrtbase.dll", "fread"), (errno, 0));
        assert_eq!(failure("ucrtbase.dll", "_wfopen_s"), (errno, 40));
        let fiopen = "?_Fiopen@std@@YAPEAU_iobuf@@PEBDHH@Z";
        assert_eq!(failure("msvcp140.dll", fiopen), (errno, 0));
    }

    /// Nothing to bind on a host that is not Windows.
    #[cfg(not(windows))]
    #[test]
    fn no_bindings_off_windows() {
        assert_eq!(missing_exports(), []);
    }

    /// A binding the host cannot satisfy fails each call with its
    /// family's error. It once panicked on the first call, inside a
    /// function title code calls.
    #[cfg(windows)]
    #[test]
    fn a_missing_host_export_fails_the_call_cleanly() {
        use std::ffi::c_void;
        use windows_sys::Win32::Foundation::{GetLastError, SetLastError};
        let null = std::ptr::null::<u16>();

        let delete =
            host!("kernel32.dll", "VictoryLacksThisW": extern "system" fn(*const u16) -> i32);
        // SAFETY: the thread's last error.
        unsafe { SetLastError(0) };
        assert_eq!(delete(null), 0);
        // SAFETY: as above.
        assert_eq!(unsafe { GetLastError() }, 127);
        // Again: the stand-in is the binding now.
        assert_eq!(delete(null), 0);

        let create = host!("kernel32.dll", "CreateFileVictoryLacks": extern "system" fn(*const u16, u32) -> *mut c_void);
        assert_eq!(create(null, 0) as isize, -1);

        let last_error = function("ws2_32.dll", "WSAGetLastError").expect("WSAGetLastError");
        // SAFETY: the host's `int WSAGetLastError(void)`.
        let last_error =
            unsafe { std::mem::transmute::<usize, extern "system" fn() -> i32>(last_error) };
        let send = host!("ws2_32.dll", "VictoryLacksThis": extern "system" fn(usize, *const u8, i32) -> i32);
        assert_eq!(send(0, std::ptr::null(), 0), -1);
        assert_eq!(last_error(), 10045);
        let join =
            host!("ws2_32.dll", "VictoryLacksThisSocket": extern "system" fn(usize) -> usize);
        assert_eq!(join(0), usize::MAX);
        let resolve =
            host!("ws2_32.dll", "GetAddrInfoVictoryLacks": extern "system" fn(*const u16) -> i32);
        assert_eq!(resolve(null), 11003);
        host!("ws2_32.dll", "VictoryLacksThisVoid": extern "system" fn(i32))(1);

        let errno = function("ucrtbase.dll", "_errno").expect("_errno");
        // SAFETY: the host's `int* _errno(void)`.
        let errno = unsafe { std::mem::transmute::<usize, extern "C" fn() -> *mut i32>(errno) };
        // SAFETY: the thread's `errno`.
        unsafe { *errno() = 0 };
        let open =
            host!("ucrtbase.dll", "_victory_lacks_this": extern "C" fn(*const u16, i32) -> i32);
        assert_eq!(open(null, 0), -1);
        // SAFETY: as above.
        assert_eq!(unsafe { *errno() }, 40);
        let registry =
            host!("advapi32.dll", "RegVictoryLacksThis": extern "system" fn(*mut c_void) -> u32);
        assert_eq!(registry(std::ptr::null_mut()), 127);
    }
}
