//! The stand-in title (tests/fixtures/hle-probe) bound against the real
//! resolver: our runtime's registry, the host policy and the overrides of
//! host functions, as `victory run` binds a title. Each test calls an
//! export of the probe, which calls its import, and each import the tests
//! rely on is held to its provider by name.
#![cfg(windows)]

use std::ffi::c_void;
use std::path::Path;
use std::sync::OnceLock;

use victory_hle::modules::{files, xasync, xgameruntime};
use victory_hle::{HleResolver, HostPolicy, runtime};
use victory_loader::{
    BindReport, ImportResolver, LoadedImage, Provenance, Resolution, UnimplementedPolicy,
    WithTitleDlls, prepare,
};
use victory_pe::ImportSymbol;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE};

#[path = "../../victory-loader/tests/common/mod.rs"]
mod common;
use common::fixture;

/// Passes every question on to `inner` and keeps what it answered: who
/// provides each import, by the import's own name.
struct Recording<'a> {
    inner: &'a dyn ImportResolver,
    answers: std::cell::RefCell<Vec<(String, Option<Provenance>)>>,
}

impl ImportResolver for Recording<'_> {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        let answer = self.inner.resolve(dll, symbol);
        let provenance = match answer {
            Resolution::Found { provenance, .. } => Some(provenance),
            Resolution::Missing => None,
        };
        let import = format!("{}!{symbol}", dll.to_ascii_lowercase());
        self.answers.borrow_mut().push((import, provenance));
        answer
    }
}

/// DLLs the host or our runtime provide are never taken from the title
/// (the rule `victory run` maps a title's DLLs by).
fn title_dll(dll: &str) -> bool {
    !HostPolicy::default().allows(dll) && !victory_hle::registry().iter().any(|m| m.serves(dll))
}

/// The probe, mapped and bound as a title is.
struct Probe {
    image: LoadedImage,
    report: BindReport,
    /// Who provides each of its imports.
    answers: Vec<(String, Option<Provenance>)>,
    /// The folders its files are in, kept for as long as it is mapped.
    _dirs: [tempfile::TempDir; 2],
}

/// Maps the probe and the title DLL it imports from, and binds both
/// against the real resolver.
fn bind_probe() -> Probe {
    let prepared = prepare(&std::fs::read(fixture("hle_probe.dll")).unwrap()).unwrap();
    let cache = tempfile::tempdir().unwrap();
    let image = victory_loader::load(&prepared, cache.path()).unwrap();
    let title_dir = tempfile::tempdir().unwrap();
    std::fs::copy(fixture("title_a.dll"), title_dir.path().join("title_a.dll")).unwrap();
    let dlls =
        victory_loader::load_title_dlls(image.info(), title_dir.path(), cache.path(), &title_dll)
            .unwrap();
    let hle = HleResolver::default();
    let resolver = WithTitleDlls {
        inner: &hle,
        dlls: &dlls,
    };
    for (_, dll) in &dlls {
        dll.bind_imports(&resolver, UnimplementedPolicy::ReturnZero)
            .unwrap();
    }
    let recording = Recording {
        inner: &resolver,
        answers: Default::default(),
    };
    let report = image
        .bind_imports(&recording, UnimplementedPolicy::ReturnZero)
        .unwrap();
    Probe {
        image,
        report,
        answers: recording.answers.into_inner(),
        _dirs: [cache, title_dir],
    }
}

/// One mapping for the tests of this process: they call into it side by
/// side, as a title's threads do.
fn probe() -> &'static Probe {
    static PROBE: OnceLock<Probe> = OnceLock::new();
    PROBE.get_or_init(bind_probe)
}

impl Probe {
    /// The address of the probe's export `name`.
    fn export(&self, name: &str) -> usize {
        self.image
            .export(name)
            .unwrap_or_else(|| panic!("the probe exports no {name}"))
    }

    /// Fails unless `import` (`dll!name`) was bound to `wanted`.
    #[track_caller]
    fn assert_provided_by(&self, import: &str, wanted: Option<Provenance>) {
        let found: Vec<_> = self.answers.iter().filter(|(i, _)| i == import).collect();
        assert_eq!(found.len(), 1, "{import} in {:?}", self.answers);
        assert_eq!(found[0].1, wanted, "{import}");
    }
}

const FROM_HLE: [&str; 12] = [
    "xgameruntime.dll!XGameRuntimeInitialize",
    "xmem.dll!XMemAlloc",
    "xmem.dll!XMemFree",
    "xmem.dll!XMemAllocDefault",
    "xmem.dll!XMemFreeDefault",
    "xmem.dll!XMemVirtualAlloc",
    "ws2_32.dll!connect",
    "ws2_32.dll!#4",
    "kernel32.dll!CreateFileW",
    "kernel32.dll!GetFileAttributesW",
    "kernel32.dll!LoadLibraryExA",
    "kernel32.dll!GetProcAddress",
];
const FROM_HOST: [&str; 2] = [
    "kernel32.dll!GetCurrentProcessId",
    "kernel32.dll!GetTickCount",
];
const FROM_TITLE: [&str; 3] = [
    "title_a.dll!title_a_named",
    "title_a.dll!#7",
    "title_a.dll!TITLE_A_DATA",
];
/// A DLL nobody has, on purpose, and a delay-loaded one nobody has.
const MISSING: [&str; 2] = [
    "victory-probe-delayed.dll!VIctoryProbeDelayed",
    "victory-probe-missing.dll!VIctoryProbeNotImplemented",
];

#[test]
fn probe_binds_against_the_real_resolver() {
    let probe = probe();
    for import in FROM_HLE {
        probe.assert_provided_by(import, Some(Provenance::Hle));
    }
    for import in FROM_HOST {
        probe.assert_provided_by(import, Some(Provenance::Host));
    }
    for import in FROM_TITLE {
        probe.assert_provided_by(import, Some(Provenance::Title));
    }
    // Missing is what the probe imports to be missing. Its own runtime
    // (Rust's, whose imports depend on the toolchain) may import from a
    // DLL the host policy does not pass through, which is then missing by
    // that rule; from a DLL our runtime or the policy answers for,
    // nothing is missing.
    let missing: Vec<_> = probe
        .answers
        .iter()
        .filter(|(_, provenance)| provenance.is_none())
        .map(|(import, _)| import.as_str())
        .collect();
    for import in MISSING {
        assert!(missing.contains(&import), "{import} in {missing:?}");
    }
    for import in &missing {
        let (dll, _) = import.split_once('!').unwrap();
        assert!(
            MISSING.contains(import) || title_dll(dll),
            "{import} is missing from a DLL that is answered for"
        );
    }

    // The report says the same.
    let count = |wanted: Provenance| {
        let provided = |(_, p): &&(String, Option<Provenance>)| *p == Some(wanted);
        probe.answers.iter().filter(provided).count()
    };
    assert_eq!(probe.report.hle, count(Provenance::Hle));
    assert_eq!(probe.report.host, count(Provenance::Host));
    assert_eq!(probe.report.title, count(Provenance::Title));
    let reported: Vec<_> = probe
        .report
        .missing
        .iter()
        .map(|m| format!("{}!{}", m.dll.to_ascii_lowercase(), m.symbol))
        .collect();
    assert_eq!(reported, missing);
}

type XmemAlloc = extern "system" fn(usize, u64) -> *mut c_void;
type XmemFree = extern "system" fn(*mut c_void, u64);
type HookCounts = extern "system" fn() -> u64;

/// The console's memory API, called by the title: the blocks are memory
/// the title can use.
#[test]
fn title_memory_calls_reach_our_allocator() {
    type VirtualAlloc = extern "system" fn(*mut c_void, usize, u32, u64, u32) -> *mut c_void;
    const MEM_COMMIT_RESERVE: u32 = 0x3000;
    const PAGE_READWRITE: u32 = 4;
    let probe = probe();
    // SAFETY: the probe declares these exports with these signatures, and
    // the imports they call are bound.
    let (alloc, free, virtual_alloc) = unsafe {
        (
            std::mem::transmute::<usize, XmemAlloc>(probe.export("probe_xmem_alloc")),
            std::mem::transmute::<usize, XmemFree>(probe.export("probe_xmem_free")),
            std::mem::transmute::<usize, VirtualAlloc>(probe.export("probe_xmem_virtual_alloc")),
        )
    };
    let block = alloc(100, 0).cast::<u8>();
    assert!(!block.is_null());
    // SAFETY: 100 bytes that are ours until freed.
    unsafe {
        block.write_bytes(0xA5, 100);
        assert_eq!(block.add(99).read(), 0xA5);
    }
    free(block.cast(), 0);

    let size = 64 << 10;
    let pages = virtual_alloc(
        std::ptr::null_mut(),
        size,
        MEM_COMMIT_RESERVE,
        0,
        PAGE_READWRITE,
    );
    assert!(!pages.is_null());
    // SAFETY: committed read-write memory of that size, zeroed by the host.
    unsafe {
        let pages = pages.cast::<u8>();
        assert_eq!(pages.add(size - 1).read(), 0);
        pages.add(size - 1).write(7);
        assert_eq!(pages.add(size - 1).read(), 7);
    }
}

#[link(name = "ws2_32")]
unsafe extern "system" {
    fn socket(family: i32, kind: i32, protocol: i32) -> usize;
    fn closesocket(socket: usize) -> i32;
    fn ioctlsocket(socket: usize, command: i32, argument: *mut u32) -> i32;
    fn WSAGetLastError() -> i32;
    fn WSASetLastError(error: i32);
}

const AF_INET: i32 = 2;
const SOCK_STREAM: i32 = 1;
const INVALID_SOCKET: usize = !0;
const FIONBIO: i32 = 0x8004_667E_u32 as i32;
const WSAENETUNREACH: i32 = 10051;

/// A `sockaddr_in` for `ip`:`port`.
fn sockaddr(ip: [u8; 4], port: u16) -> [u8; 16] {
    let mut address = [0; 16];
    address[..2].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
    address[2..4].copy_from_slice(&port.to_be_bytes());
    address[4..8].copy_from_slice(&ip);
    address
}

/// A title's `connect`, imported by name and by ordinal, goes through our
/// network guard: loopback connects, anything else has no route.
#[test]
fn title_connects_reach_loopback_only() {
    type Connect = extern "system" fn(usize, *const u8, i32) -> i32;
    let probe = probe();
    // Binding it starts Winsock, which the sockets below need.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    for export in ["probe_connect", "probe_connect_ordinal"] {
        // SAFETY: the probe declares both exports with this signature, and
        // the import each calls is bound.
        let connect: Connect = unsafe { std::mem::transmute(probe.export(export)) };

        // SAFETY: plain Winsock calls on sockets of our own; the address
        // is a whole `sockaddr_in`.
        unsafe {
            let local = socket(AF_INET, SOCK_STREAM, 0);
            assert_ne!(local, INVALID_SOCKET, "{export}");
            let to = sockaddr([127, 0, 0, 1], port);
            assert_eq!(connect(local, to.as_ptr(), to.len() as i32), 0, "{export}");
            let (_, peer) = listener.accept().unwrap();
            assert!(peer.ip().is_loopback(), "{export}: {peer}");
            closesocket(local);

            // TEST-NET-1 (RFC 5737), which is nobody's address. The socket
            // does not block, so a connect that got as far as the host
            // would answer `WSAEWOULDBLOCK`, not this.
            let remote = socket(AF_INET, SOCK_STREAM, 0);
            assert_ne!(remote, INVALID_SOCKET, "{export}");
            let mut on = 1;
            assert_eq!(ioctlsocket(remote, FIONBIO, &mut on), 0, "{export}");
            let to = sockaddr([192, 0, 2, 1], 80);
            WSASetLastError(0);
            assert_eq!(
                connect(remote, to.as_ptr(), to.len() as i32),
                -1,
                "{export}"
            );
            assert_eq!(WSAGetLastError(), WSAENETUNREACH, "{export}");
            closesocket(remote);
        }
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// A title's file calls go through our path overrides: a path on a virtual
/// drive, which the host does not have, names a file in the folder mounted
/// there.
#[test]
fn title_files_on_a_virtual_drive_land_in_the_mounted_folder() {
    type Create = extern "system" fn(*const u16) -> *mut c_void;
    type Attributes = extern "system" fn(*const u16) -> u32;
    const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    let probe = probe();
    // SAFETY: the probe declares these exports with these signatures, and
    // the imports they call are bound.
    let (create, attributes) = unsafe {
        (
            std::mem::transmute::<usize, Create>(probe.export("probe_create")),
            std::mem::transmute::<usize, Attributes>(probe.export("probe_attributes")),
        )
    };
    let folder = tempfile::tempdir().unwrap();
    let drive = files::mount(folder.path()).expect("a free drive letter");
    assert!(
        !Path::new(&format!("{drive}:\\")).exists(),
        "{drive}: is a drive of the host"
    );

    let path = wide(&format!("{drive}:\\probe-created.txt"));
    assert_eq!(attributes(path.as_ptr()), INVALID_FILE_ATTRIBUTES);
    let handle = create(path.as_ptr());
    // SAFETY: reads this thread's last error.
    let error = unsafe { GetLastError() };
    assert_ne!(handle, INVALID_HANDLE_VALUE, "error {error}");
    // SAFETY: the handle `CreateFileW` just returned.
    assert_ne!(unsafe { CloseHandle(handle) }, 0);
    assert!(folder.path().join("probe-created.txt").is_file());

    let found = attributes(path.as_ptr());
    assert_ne!(found, INVALID_FILE_ATTRIBUTES);
    assert_eq!(found & FILE_ATTRIBUTE_DIRECTORY, 0, "{found:#x}");
    let absent = wide(&format!("{drive}:\\probe-absent.txt"));
    assert_eq!(attributes(absent.as_ptr()), INVALID_FILE_ATTRIBUTES);
}

/// The game runtime as a title's stub finds it: `LoadLibraryExA` answers
/// with our pseudo module, `GetProcAddress` on it with our `QueryApiImpl`.
#[test]
fn title_finds_the_runtime_and_queries_an_api_area() {
    type Query = extern "system" fn(
        *const xgameruntime::Guid,
        *const xgameruntime::Guid,
        *mut *mut c_void,
    ) -> i32;
    let probe = probe();
    // SAFETY: the probe declares the export with this signature (its
    // pointers are to GUIDs), and the imports it calls are bound.
    let query: Query = unsafe { std::mem::transmute(probe.export("probe_query_api")) };

    // The task-queue area, whose provider id is its interface id too.
    let id = xasync::PROVIDER;
    let mut object = std::ptr::null_mut();
    assert_eq!(query(&id, &id, &mut object), xgameruntime::S_OK);
    assert!(!object.is_null());
    let mut direct = std::ptr::null_mut();
    assert_eq!(
        xgameruntime::QueryApiImpl(&id, &id, &mut direct),
        xgameruntime::S_OK
    );
    assert_eq!(object, direct, "the object our runtime serves");

    // An area nobody implements fails, and nothing is handed out.
    let nobodys = xgameruntime::Guid::new(0x5eed, 1, 2, [3; 8]);
    let mut object = std::ptr::null_mut();
    assert_eq!(
        query(&nobodys, &nobodys, &mut object),
        xgameruntime::NOT_SUPPORTED
    );
    assert!(object.is_null());
}

/// Which part the child process is to play; unset in ordinary runs.
const CHILD: &str = "VICTORY_PROBE_HLE_CHILD";

/// Runs the test `test` of this binary alone in a child process that plays
/// `part`, in the folder `dir`, and returns what it printed.
fn child(test: &str, part: &str, dir: &Path) -> String {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(CHILD, part)
        .current_dir(dir)
        .output()
        .unwrap();
    let printed = String::from_utf8_lossy(&out.stdout).into_owned();
    let errors = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{part}: {printed}\n{errors}");
    printed
}

/// The value of the one line `name=value` in `printed`.
#[track_caller]
fn said<'a>(printed: &'a str, name: &str) -> &'a str {
    let found: Vec<_> = printed
        .lines()
        .filter_map(|l| l.trim_end().strip_prefix(name)?.strip_prefix('='))
        .collect();
    match found[..] {
        [value] => value,
        _ => panic!("{} lines say {name} in:\n{printed}", found.len()),
    }
}

/// In a child process of the next test: the probe is the title's main
/// image, and the console's allocator is called by it and by us.
#[test]
fn hooks_child() {
    if std::env::var(CHILD).as_deref() != Ok("hooks") {
        return;
    }
    let probe = bind_probe();
    // SAFETY: the probe declares these exports with these signatures, and
    // the imports they call are bound.
    let (alloc, free, counts) = unsafe {
        (
            std::mem::transmute::<usize, XmemAlloc>(probe.export("probe_xmem_alloc")),
            std::mem::transmute::<usize, XmemFree>(probe.export("probe_xmem_free")),
            std::mem::transmute::<usize, HookCounts>(probe.export("probe_hook_counts")),
        )
    };
    let counted = || {
        let counts = counts();
        format!("{}:{}", counts >> 32, counts as u32)
    };
    println!();
    // No main image yet: nobody's hooks are called.
    let early = alloc(64, 0);
    println!("early_block={}", !early.is_null());
    println!("before={}", counted());

    runtime::set_main_image(probe.image.base());
    let block = alloc(100, 0).cast::<u8>();
    println!("block={}", !block.is_null());
    if !block.is_null() {
        // SAFETY: 100 bytes that are ours until freed.
        unsafe { block.write_bytes(0x5A, 100) };
    }
    println!("after_alloc={}", counted());
    free(block.cast(), 0);
    println!("after_free={}", counted());
    // Middleware calls our export itself, not through the title's import.
    let other = victory_hle::modules::xmem::XMemAlloc(32, 0);
    println!("after_direct_alloc={}", counted());
    victory_hle::modules::xmem::XMemFree(other, 0);
    victory_hle::modules::xmem::XMemFree(early, 0);
    println!("after_direct_frees={}", counted());
    // The system allocator behind the hooks is not hooked.
    let plain = victory_hle::modules::xmem::XMemAllocDefault(32, 0);
    victory_hle::modules::xmem::XMemFreeDefault(plain, 0);
    println!("after_defaults={}", counted());
}

/// The title's executable may export `__XMemAllocationHooks__`, its own
/// allocation and free routines: once the title's main image is known,
/// `XMemAlloc` and `XMemFree` call them, for the title and for everyone
/// else, and the `...Default` pair does not. The hooks are looked up once
/// per process, hence a process of its own.
#[test]
fn allocation_hooks_of_the_main_image_are_called() {
    let dir = tempfile::tempdir().unwrap();
    let printed = child("hooks_child", "hooks", dir.path());
    assert_eq!(said(&printed, "early_block"), "true", "{printed}");
    assert_eq!(said(&printed, "before"), "0:0", "{printed}");
    assert_eq!(said(&printed, "block"), "true", "{printed}");
    assert_eq!(said(&printed, "after_alloc"), "1:0", "{printed}");
    assert_eq!(said(&printed, "after_free"), "1:1", "{printed}");
    assert_eq!(said(&printed, "after_direct_alloc"), "2:1", "{printed}");
    assert_eq!(said(&printed, "after_direct_frees"), "2:3", "{printed}");
    assert_eq!(said(&printed, "after_defaults"), "2:3", "{printed}");
}
