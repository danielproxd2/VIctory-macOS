//! Process-wide state the implementations need.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

static MAIN_IMAGE_BASE: AtomicUsize = AtomicUsize::new(0);

/// Records where the title's main image is mapped. The title expects
/// `GetModuleHandle(NULL)` and friends to return this, not our own exe.
pub fn set_main_image(base: usize) {
    MAIN_IMAGE_BASE.store(base, Ordering::Release);
}

pub fn main_image() -> Option<usize> {
    match MAIN_IMAGE_BASE.load(Ordering::Acquire) {
        0 => None,
        base => Some(base),
    }
}

/// Original file of each title image we mapped, by base address.
static IMAGE_PATHS: Mutex<Vec<(usize, PathBuf)>> = Mutex::new(Vec::new());

/// Records the file a title image was mapped from: the title asks for its
/// own path (`GetModuleFileName`) to find its data, and must not get our
/// cache copy.
pub fn set_image_path(base: usize, path: &Path) {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_owned());
    let mut paths = IMAGE_PATHS.lock().unwrap_or_else(|e| e.into_inner());
    paths.retain(|(b, _)| *b != base);
    paths.push((base, path));
}

/// Forgets a title image that was unmapped again.
pub fn forget_image(base: usize) {
    let mut paths = IMAGE_PATHS.lock().unwrap_or_else(|e| e.into_inner());
    paths.retain(|(b, _)| *b != base);
}

pub fn image_path(base: usize) -> Option<PathBuf> {
    let paths = IMAGE_PATHS.lock().unwrap_or_else(|e| e.into_inner());
    paths
        .iter()
        .find(|(b, _)| *b == base)
        .map(|(_, p)| p.clone())
}

/// The title image whose original file is called `file` (any case).
pub fn image_named(file: &str) -> Option<usize> {
    let paths = IMAGE_PATHS.lock().unwrap_or_else(|e| e.into_inner());
    paths
        .iter()
        .find(|(_, p)| p.file_name().is_some_and(|n| n.eq_ignore_ascii_case(file)))
        .map(|(b, _)| *b)
}

/// Maps a title DLL the title loads itself (`LoadLibrary`) and returns its
/// base; `Ok(None)` if `file` is not one of the title's DLLs.
pub type TitleDllLoader = Box<dyn Fn(&str) -> Result<Option<usize>, String> + Send + Sync>;

static TITLE_DLL_LOADER: std::sync::OnceLock<TitleDllLoader> = std::sync::OnceLock::new();

/// Set once, by whoever mapped the title (it knows the folders and the
/// resolver); later calls are ignored.
pub fn set_title_dll_loader(loader: TitleDllLoader) {
    let _ = TITLE_DLL_LOADER.set(loader);
}

pub fn load_title_dll(file: &str) -> Result<Option<usize>, String> {
    TITLE_DLL_LOADER.get().map_or(Ok(None), |load| load(file))
}

/// What the title's manifest (`MicrosoftGame.config` beside its executable)
/// says about it, and where its persistent local storage lives.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Title {
    pub id: u32,
    /// The store's product id (empty if the manifest has none).
    pub store_id: String,
    /// Persistent local storage: our folder for it and the title's quota.
    /// Game saves live beside it ([`Title::saves`]).
    pub storage: PathBuf,
    pub storage_bytes: u64,
}

impl Title {
    /// Id, store id and storage quota from the manifest (zero or empty
    /// where it has none); the storage folder is left for the caller.
    ///
    /// How the console reads a manifest that is not what the tools write is
    /// not known, so nothing is guessed: an element that occurs more than
    /// once, a title id that is not plain hexadecimal digits and a size
    /// that is not plain decimal digits all count as not given.
    pub fn from_manifest(text: &str) -> Title {
        let elements = elements(text);
        let get = |path: &str| {
            let mut found = elements.iter().filter(|(p, _)| p == path);
            match (found.next(), found.next()) {
                (Some((_, text)), None) => Some(text.as_str()),
                _ => None,
            }
        };
        let digits = |v: &str, radix: u32| !v.is_empty() && v.chars().all(|c| c.is_digit(radix));
        let id = get("Game/TitleId")
            .filter(|v| digits(v, 16))
            .and_then(|v| u32::from_str_radix(v, 16).ok());
        let storage_mb = get("Game/PersistentLocalStorage/SizeMB")
            .filter(|v| digits(v, 10))
            .and_then(|v| v.parse().ok());
        Title {
            id: id.unwrap_or(0),
            store_id: get("Game/StoreId").unwrap_or_default().to_owned(),
            storage: PathBuf::new(),
            storage_bytes: storage_mb.map_or(0, |mb: u64| mb.saturating_mul(1 << 20)),
        }
    }

    /// [`Title::from_manifest`] of the manifest file's bytes: UTF-8, or
    /// UTF-16 behind a byte-order mark (XML requires the mark of a UTF-16
    /// document). Bytes that are neither say nothing about the title.
    pub fn from_manifest_bytes(bytes: &[u8]) -> Title {
        match manifest_text(bytes) {
            Some(text) => Title::from_manifest(&text),
            None => {
                tracing::warn!("the title's manifest is neither UTF-8 nor marked UTF-16; ignored");
                Title::default()
            }
        }
    }

    /// Where game saves live: next to persistent local storage, under the
    /// data directory, never the cache.
    pub fn saves(&self) -> PathBuf {
        self.storage.with_file_name("saves")
    }
}

/// The text of a manifest file, if its bytes are text in its encoding.
fn manifest_text(bytes: &[u8]) -> Option<String> {
    let utf16 = |bytes: &[u8], unit: fn([u8; 2]) -> u16| {
        let (units, []) = bytes.as_chunks::<2>() else {
            return None;
        };
        let units: Vec<u16> = units.iter().map(|u| unit(*u)).collect();
        String::from_utf16(&units).ok()
    };
    match bytes {
        [0xff, 0xfe, rest @ ..] => utf16(rest, u16::from_le_bytes),
        [0xfe, 0xff, rest @ ..] => utf16(rest, u16::from_be_bytes),
        _ => std::str::from_utf8(bytes).ok().map(str::to_owned),
    }
}

/// Where the tag that `rest` starts with ends (just past its `>`), if it
/// does. A `>` inside a quoted attribute value is part of the value, as in
/// XML.
fn tag_end(rest: &str) -> Option<usize> {
    let mut quote = None;
    for (at, c) in rest.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(open), c) if c == open => quote = None,
            (None, '>') => return Some(at + 1),
            _ => {}
        }
    }
    None
}

/// Each element of an XML text as its path from the root (`Game/TitleId`)
/// and its trimmed text up to the first child, the predefined entities
/// decoded; comments, CDATA sections, declarations and attributes are
/// skipped. Names are taken as written (a namespace prefix is part of the
/// name), and the text ends at a tag that never closes.
fn elements(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut path: Vec<&str> = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        rest = &rest[open..];
        let end = if rest.starts_with("<!--") {
            rest.find("-->").map(|e| e + 3)
        } else if rest.starts_with("<![CDATA[") {
            rest.find("]]>").map(|e| e + 3)
        } else {
            tag_end(rest)
        };
        let Some(end) = end else { break };
        let tag = &rest[1..end - 1];
        rest = &rest[end..];
        if tag.starts_with(['!', '?']) {
            continue;
        }
        if let Some(name) = tag.strip_prefix('/') {
            if let Some(at) = path.iter().rposition(|n| *n == name.trim()) {
                path.truncate(at);
            }
            continue;
        }
        let closed = tag.ends_with('/');
        let name = tag
            .trim_end_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or("");
        path.push(name);
        let body = rest[..rest.find('<').unwrap_or(rest.len())].trim();
        let body = [
            ("&lt;", "<"),
            ("&gt;", ">"),
            ("&quot;", "\""),
            ("&apos;", "'"),
            ("&amp;", "&"),
        ]
        .iter()
        .fold(body.to_owned(), |b, (entity, c)| b.replace(entity, c));
        out.push((path.join("/"), body));
        if closed {
            path.pop();
        }
    }
    out
}

static TITLE: Mutex<Option<Title>> = Mutex::new(None);

pub fn set_title(title: Title) {
    *TITLE.lock().unwrap_or_else(|e| e.into_inner()) = Some(title);
}

/// The running title; the default (id 0, no storage) until set.
pub fn title() -> Title {
    TITLE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default()
}

/// Gives the process the console's millisecond timers. Titles pace
/// themselves with short waits (Grand Theft Auto VI's audio mixer sleeps
/// about 4 ms per 5.3 ms block) and cannot tune the timer on the console;
/// a desktop process waits in 15.6 ms ticks unless it asks for finer ones,
/// and Windows 11 drops that request while the window is hidden unless the
/// process opts out. Process-wide, for the whole run.
#[cfg(windows)]
pub fn use_console_timers() {
    use windows_sys::Win32::Media::timeBeginPeriod;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
        ProcessPowerThrottling, SetProcessInformation,
    };
    let keep = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        StateMask: 0,
    };
    // SAFETY: plain process settings; `keep` outlives the call.
    unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            (&raw const keep).cast(),
            size_of_val(&keep) as u32,
        );
        timeBeginPeriod(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_title_id_store_id_and_storage_quota() {
        let text = "\u{feff}<?xml version=\"1.0\"?>\n<Game configVersion=\"1\">\
                    <!-- <TitleId>00000000</TitleId> -->\
                    <Identity Name=\"x\"/>\n  <TitleId>639AA8DD</TitleId>\n\
                    <![CDATA[ <TitleId>1</TitleId> ]]>                    <StoreId>9&amp;C</StoreId><Other><SizeMB>7</SizeMB></Other>\
                    <PersistentLocalStorage>\n  <SizeMB>1024</SizeMB> <!-- c -->\n\
                    </PersistentLocalStorage></Game>";
        let t = Title::from_manifest(text);
        assert_eq!((t.id, t.storage_bytes), (0x639A_A8DD, 1024 << 20));
        assert_eq!(t.store_id, "9&C");
        assert_eq!(Title::from_manifest("").storage_bytes, 0);
    }

    #[test]
    fn a_huge_storage_quota_does_not_wrap() {
        // 2^44 MiB is 2^64 bytes: a plain shift wraps to 0 ("no quota").
        let text = "<Game><PersistentLocalStorage><SizeMB>17592186044416</SizeMB>\
                    </PersistentLocalStorage></Game>";
        assert_eq!(Title::from_manifest(text).storage_bytes, u64::MAX);
    }

    const ID: &str = "<TitleId>639AA8DD</TitleId>";

    fn game(inner: &str) -> Title {
        Title::from_manifest(&format!("<Game>{inner}</Game>"))
    }

    /// Manifests the tools would not write. What the console makes of them
    /// is not known, so each either reads as the well-formed XML it is or
    /// leaves the value out (id 0, no store id, no quota); none is read
    /// leniently into a value its text does not plainly give.
    #[test]
    fn manifest_edge_cases() {
        assert_eq!(game(ID).id, 0x639A_A8DD);

        // A `>` in a quoted attribute value is not the end of its tag.
        for text in [
            "<Game note=\"a>b\"><TitleId>639AA8DD</TitleId></Game>",
            "<Game><TitleId kind='a>b'>639AA8DD</TitleId></Game>",
            "<Game><Identity Name=\"<TitleId>1</TitleId>\"/><TitleId>639AA8DD</TitleId></Game>",
            "<Game><Other a=\"it's\" b='\"'/><TitleId>639AA8DD</TitleId></Game>",
        ] {
            assert_eq!(Title::from_manifest(text).id, 0x639A_A8DD, "{text}");
        }

        // Tags that never close: nothing after them is read.
        for text in [
            "<Game><TitleId",
            "<Game><TitleId>639AA8DD</TitleId",
            "<Game note=\"><TitleId>639AA8DD</TitleId></Game>",
            "<Game><!-- <TitleId>639AA8DD</TitleId></Game>",
            "<Game><![CDATA[ <TitleId>639AA8DD</TitleId></Game>",
            "<",
            "<<<<",
            ">>>>",
            "</",
            "</>",
            "<>",
            "<!-->",
            "<Game/><TitleId>639AA8DD</TitleId>",
        ] {
            let t = Title::from_manifest(text);
            assert!(t.id == 0 || t.id == 0x639A_A8DD, "{text}: {:08X}", t.id);
            assert_eq!((t.store_id.as_str(), t.storage_bytes), ("", 0), "{text}");
        }
        assert_eq!(Title::from_manifest("<Game><TitleId").id, 0);
        assert_eq!(
            Title::from_manifest("<Game note=\"><TitleId>639AA8DD</TitleId></Game>").id,
            0
        );

        // A namespace prefix is part of the name: not the element we know.
        let prefixed = "<mgc:Game xmlns:mgc=\"urn:x\"><mgc:TitleId>639AA8DD</mgc:TitleId>\
                        <mgc:StoreId>9ABC</mgc:StoreId></mgc:Game>";
        assert_eq!(Title::from_manifest(prefixed), Title::default());
        assert_eq!(game("<x:TitleId>639AA8DD</x:TitleId>").id, 0);

        // The id is hexadecimal digits, as the tools write it; white space
        // around it is not part of it.
        assert_eq!(game("<TitleId>\n  639aa8dd\t</TitleId>").id, 0x639A_A8DD);
        assert_eq!(game("<TitleId>0000002A</TitleId>").id, 0x2A);
        for id in [
            "0x639AA8DD",
            "0X639AA8DD",
            "+639AA8DD",
            "-1",
            "639A A8DD",
            "639AA8DD0",
            "639AA8DG",
            "\u{ff16}39AA8DD",
            "",
            "&amp;",
        ] {
            assert_eq!(game(&format!("<TitleId>{id}</TitleId>")).id, 0, "{id:?}");
        }
        let quota = |size: &str| {
            let size = format!("<SizeMB>{size}</SizeMB>");
            game(&format!(
                "<PersistentLocalStorage>{size}</PersistentLocalStorage>"
            ))
            .storage_bytes
        };
        assert_eq!(quota(" 16 "), 16 << 20);
        for size in ["+16", "-16", "0x10", "1 6", "16MB", "1.5", ""] {
            assert_eq!(quota(size), 0, "{size:?}");
        }

        // Said twice is not said: no value is picked.
        let two_roots = "<Game><TitleId>639AA8DD</TitleId><StoreId>A</StoreId></Game>\
                         <Game><TitleId>11111111</TitleId><StoreId>B</StoreId></Game>";
        assert_eq!(Title::from_manifest(two_roots), Title::default());
        assert_eq!(game(&format!("{ID}{ID}")).id, 0);
        assert_eq!(game(&format!("{ID}<TitleId/>")).id, 0);
        // Another element of the same name elsewhere is another element.
        assert_eq!(game(&format!("{ID}<Other>{ID}</Other>")).id, 0x639A_A8DD);
    }

    /// The manifest file as bytes: UTF-8 with or without its mark, UTF-16
    /// of either byte order behind its mark; anything else is no manifest.
    #[test]
    fn manifests_are_read_in_their_encoding() {
        let text = "<?xml version=\"1.0\" encoding=\"utf-16\"?>\n<Game configVersion=\"1\">\
                    <TitleId>639AA8DD</TitleId><StoreId>9\u{e9}\u{1f600}</StoreId>\
                    <PersistentLocalStorage><SizeMB>8</SizeMB></PersistentLocalStorage></Game>";
        let expected = Title::from_manifest(text);
        assert_eq!(
            (expected.id, expected.storage_bytes),
            (0x639A_A8DD, 8 << 20)
        );
        assert_eq!(expected.store_id, "9\u{e9}\u{1f600}");

        let utf16 = |mark: [u8; 2], bytes: fn(u16) -> [u8; 2]| {
            let units = text.encode_utf16().flat_map(bytes);
            mark.into_iter().chain(units).collect::<Vec<u8>>()
        };
        let little = utf16([0xff, 0xfe], u16::to_le_bytes);
        let big = utf16([0xfe, 0xff], u16::to_be_bytes);
        let marked_utf8 = [b"\xef\xbb\xbf", text.as_bytes()].concat();
        for bytes in [text.as_bytes(), &marked_utf8, &little, &big] {
            assert_eq!(Title::from_manifest_bytes(bytes), expected);
        }

        // Not text in its encoding: no title, rather than part of one.
        let none = Title::default();
        let unmarked = &little[2..];
        let odd = &little[..little.len() - 1];
        let lone_surrogate = [&little[..], &[0x00, 0xd8]].concat();
        let bad_utf8 = [text.as_bytes(), b"\xff"].concat();
        let swapped = [&[0xfe, 0xff][..], &little[2..]].concat();
        for bytes in [unmarked, odd, &lone_surrogate, &bad_utf8, &swapped] {
            assert_eq!(Title::from_manifest_bytes(bytes), none);
        }
        for bytes in [&b""[..], b"\xff\xfe", b"\xfe\xff", b"\xff", b"\xef\xbb\xbf"] {
            assert_eq!(Title::from_manifest_bytes(bytes), none);
        }
    }

    /// A small xorshift generator, so the random cases are the same on
    /// every run and host.
    struct Random(u64);

    impl Random {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Text made of the pieces a manifest is made of, in any order.
    fn random_manifest(random: &mut Random) -> String {
        const PIECES: &[&str] = &[
            "<",
            ">",
            "</",
            "/>",
            "<!--",
            "-->",
            "<![CDATA[",
            "]]>",
            "<?",
            "?>",
            "<!",
            "\"",
            "'",
            "=",
            "/",
            " ",
            "\n",
            "&",
            "&amp;",
            "&lt;",
            "&gt;",
            ";",
            "<Game>",
            "</Game>",
            "<TitleId>",
            "</TitleId>",
            "Game",
            "TitleId",
            "StoreId",
            "PersistentLocalStorage",
            "SizeMB",
            "mgc:",
            "0x",
            "639AA8DD",
            "1024",
            "f",
            "9",
            "+",
            "-",
            "\u{e9}",
            "\u{feff}",
            "\u{1f600}",
            "\0",
        ];
        (0..random.below(40))
            .map(|_| PIECES[random.below(PIECES.len())])
            .collect()
    }

    /// Whatever the text, parsing ends and what it yields is well formed:
    /// element text never holds a tag, and the title holds only what an
    /// element of the right name gave.
    #[test]
    fn random_manifests_never_panic() {
        let mut random = Random(0x9e37_79b9_7f4a_7c15);
        let (mut with_id, mut without) = (0, 0);
        for _ in 0..20_000 {
            let text = random_manifest(&mut random);
            let found = elements(&text);
            assert!(found.len() <= text.matches('<').count(), "{text:?}");
            let t = Title::from_manifest(&text);
            assert_eq!(t.storage, PathBuf::new());
            let named = |name: &str| found.iter().any(|(path, _)| path == name);
            assert!(t.id == 0 || named("Game/TitleId"), "{text:?}");
            assert!(t.store_id.is_empty() || named("Game/StoreId"), "{text:?}");
            assert_eq!(t.storage_bytes % (1 << 20), 0, "{text:?}");
            // The file's encoding does not change what is read.
            assert_eq!(Title::from_manifest_bytes(text.as_bytes()), t, "{text:?}");
            let utf16 = text.encode_utf16().flat_map(u16::to_le_bytes);
            let utf16: Vec<u8> = [0xff, 0xfe].into_iter().chain(utf16).collect();
            assert_eq!(Title::from_manifest_bytes(&utf16), t, "{text:?}");
            // Nor do bytes that are no text at all stop it.
            let noise: Vec<u8> = (0..random.below(64)).map(|_| random.next() as u8).collect();
            let cut = &utf16[..random.below(utf16.len() + 1)];
            for bytes in [&noise[..], cut] {
                let t = Title::from_manifest_bytes(bytes);
                assert_eq!(t.storage, PathBuf::new());
                assert_eq!(t.storage_bytes % (1 << 20), 0, "{bytes:?}");
            }
            if t.id == 0 {
                without += 1;
            } else {
                with_id += 1;
            }
        }
        // The cases reach both outcomes.
        assert!(with_id > 0 && without > 0, "{with_id} {without}");
    }
}
