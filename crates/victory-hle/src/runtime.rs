//! Process-wide state the implementations need.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use victory_pe::ImportSymbol;

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

/// Finds an export of the title image mapped at a base address, by name or
/// ordinal: `None` if the image has none, or if it is forwarded to one
/// nobody has.
pub type TitleExports = Box<dyn Fn(usize, &ImportSymbol) -> Option<usize> + Send + Sync>;

static TITLE_EXPORTS: std::sync::OnceLock<TitleExports> = std::sync::OnceLock::new();

/// Set once, by whoever mapped the title (it has the images and the
/// resolver; see [`title_export_in`]); later calls are ignored.
pub fn set_title_exports(exports: TitleExports) {
    let _ = TITLE_EXPORTS.set(exports);
}

/// What the title's `GetProcAddress` answers for a title image. The host is
/// never asked: its `GetProcAddress` follows a forwarded export by loading
/// the DLL the forwarder names, which for a title's DLL is the file as the
/// title ships it, not the copy we prepared and bound. Until
/// [`set_title_exports`] is called, title images have no exports here.
pub fn title_export(base: usize, symbol: &ImportSymbol) -> Option<usize> {
    TITLE_EXPORTS.get()?(base, symbol)
}

/// The export `symbol` of the image among `images` that is mapped at
/// `base`, as import binding resolves it: a forwarded export is followed
/// through `inner` (our runtime, the host) and through `images`, never
/// through the Windows loader. For [`set_title_exports`].
#[cfg(windows)]
pub fn title_export_in(
    images: &[(String, victory_loader::LoadedImage)],
    inner: &dyn victory_loader::ImportResolver,
    base: usize,
    symbol: &ImportSymbol,
) -> Option<usize> {
    use victory_loader::{ImportResolver, Resolution, WithTitleDlls};
    let (name, _) = images.iter().find(|(_, image)| image.base() == base)?;
    let resolver = WithTitleDlls {
        inner,
        dlls: images,
    };
    match resolver.resolve(name, symbol) {
        Resolution::Found { address, .. } => Some(address),
        Resolution::Missing => None,
    }
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

/// One of the three things a manifest tells us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    TitleId,
    StoreId,
    StorageSize,
}

impl Field {
    /// The element's path from the root.
    pub fn path(self) -> &'static str {
        match self {
            Field::TitleId => "Game/TitleId",
            Field::StoreId => "Game/StoreId",
            Field::StorageSize => "Game/PersistentLocalStorage/SizeMB",
        }
    }
}

/// A rule the manifest reader holds to that a manifest broke, so that the
/// part it names was not taken ([`Manifest::refused`]). A title whose id
/// was refused runs as title 00000000, with its storage in that folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The file is neither UTF-8 nor UTF-16 behind a byte-order mark:
    /// nothing of it was read.
    Encoding,
    /// A tag, comment or CDATA section never closes: nothing from its
    /// start on was read.
    UnclosedTag,
    /// A quote inside a tag never closes, which makes the rest of the
    /// file that tag: nothing from its start on was read.
    UnbalancedQuote,
    /// The element is given more than once: none of them was taken.
    Repeated(Field),
    /// `Game/TitleId` is not one to eight hexadecimal digits (a sign, a
    /// `0x` or anything else in it): no id was taken.
    TitleIdNotHex,
    /// `Game/PersistentLocalStorage/SizeMB` is not a number in decimal
    /// digits: no quota was taken.
    SizeNotDecimal,
}

impl Refusal {
    /// The rule's name, as the log has it.
    pub fn rule(self) -> &'static str {
        match self {
            Refusal::Encoding => "encoding",
            Refusal::UnclosedTag => "unclosed-tag",
            Refusal::UnbalancedQuote => "unbalanced-quote",
            Refusal::Repeated(_) => "repeated-element",
            Refusal::TitleIdNotHex => "title-id-not-hex",
            Refusal::SizeNotDecimal => "size-not-decimal",
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Encoding => f.write_str(
                "the file is neither UTF-8 nor UTF-16 behind a byte-order mark; nothing is read",
            ),
            Refusal::UnclosedTag => f.write_str(
                "a tag, comment or CDATA section never closes; nothing from its start on is read",
            ),
            Refusal::UnbalancedQuote => f.write_str(
                "a quote inside a tag never closes; nothing from the tag's start on is read",
            ),
            Refusal::Repeated(field) => {
                write!(f, "{} is given more than once; none is taken", field.path())
            }
            Refusal::TitleIdNotHex => write!(
                f,
                "{} is not one to eight hexadecimal digits; no title id is taken",
                Field::TitleId.path()
            ),
            Refusal::SizeNotDecimal => write!(
                f,
                "{} is not a number in decimal digits; no storage quota is taken",
                Field::StorageSize.path()
            ),
        }
    }
}

/// What a manifest gave ([`Title::read_manifest`]), and what of it was not
/// taken and why.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    pub title: Title,
    /// Each rule the manifest broke, once, in the order: the file, then
    /// title id, store id and storage size.
    pub refused: Vec<Refusal>,
}

impl Manifest {
    /// The manifest of `text`. Logs nothing.
    fn parse(text: &str) -> Manifest {
        let (elements, cut) = elements(text);
        let mut refused: Vec<Refusal> = cut.into_iter().collect();
        let mut get = |field: Field| {
            let mut found = elements.iter().filter(|(p, _)| p == field.path());
            match (found.next(), found.next()) {
                (Some((_, text)), None) => Some(text.as_str()),
                (Some(_), Some(_)) => {
                    refused.push(Refusal::Repeated(field));
                    None
                }
                (None, _) => None,
            }
        };
        let (id, store_id, size) = (
            get(Field::TitleId),
            get(Field::StoreId),
            get(Field::StorageSize),
        );
        // An element that is there and gives no value is a refusal, one
        // that is not there is not.
        let mut number = |value: Option<&str>, radix: u32, most: u64, rule: Refusal| {
            let number = plain_number(value?, radix).filter(|n| *n <= most);
            if number.is_none() {
                refused.push(rule);
            }
            number
        };
        let id = number(id, 16, u32::MAX.into(), Refusal::TitleIdNotHex);
        let id = id.map_or(0, |id| id as u32);
        let storage_mb = number(size, 10, u64::MAX, Refusal::SizeNotDecimal);
        let title = Title {
            id,
            store_id: store_id.unwrap_or_default().to_owned(),
            storage: PathBuf::new(),
            storage_bytes: storage_mb.map_or(0, |mb| mb.saturating_mul(1 << 20)),
        };
        // In the documented order, whatever order they were found in.
        refused.sort_by_key(|r| match r {
            Refusal::Encoding | Refusal::UnclosedTag | Refusal::UnbalancedQuote => 0,
            Refusal::Repeated(Field::TitleId) | Refusal::TitleIdNotHex => 1,
            Refusal::Repeated(Field::StoreId) => 2,
            Refusal::Repeated(Field::StorageSize) | Refusal::SizeNotDecimal => 3,
        });
        Manifest { title, refused }
    }

    /// One warning for each refusal, naming its rule: a refused title id
    /// means the title runs as 00000000, which must not pass unseen.
    fn warned(self) -> Manifest {
        for refused in &self.refused {
            tracing::warn!(
                rule = refused.rule(),
                "the title's manifest is not taken as it stands: {refused}"
            );
        }
        self
    }
}

impl Title {
    /// Id, store id and storage quota from the manifest (zero or empty
    /// where it has none); the storage folder is left for the caller.
    ///
    /// How the console reads a manifest that is not what the tools write is
    /// not known, so nothing is guessed: an element that occurs more than
    /// once, a title id that is not plain hexadecimal digits and a size
    /// that is not plain decimal digits all count as not given. Each such
    /// case is logged as a warning; [`Title::read_manifest`] also returns
    /// them.
    pub fn from_manifest(text: &str) -> Title {
        Manifest::parse(text).warned().title
    }

    /// [`Title::read_manifest`] without its reasons (they are logged).
    pub fn from_manifest_bytes(bytes: &[u8]) -> Title {
        Title::read_manifest(bytes).title
    }

    /// [`Title::from_manifest`] of the manifest file's bytes: UTF-8, or
    /// UTF-16 behind a byte-order mark (XML requires the mark of a UTF-16
    /// document). Bytes that are neither say nothing about the title.
    ///
    /// With the title comes every [`Refusal`]: what the manifest holds
    /// that was not taken, and by which rule. Each is logged as one
    /// warning; the caller tells the user. A manifest that simply lacks an
    /// element (or an empty file, as for no manifest) has none.
    pub fn read_manifest(bytes: &[u8]) -> Manifest {
        let manifest = match manifest_text(bytes) {
            Some(text) => Manifest::parse(&text),
            None => Manifest {
                title: Title::default(),
                refused: vec![Refusal::Encoding],
            },
        };
        manifest.warned()
    }

    /// Where game saves live: next to persistent local storage, under the
    /// data directory, never the cache.
    pub fn saves(&self) -> PathBuf {
        self.storage.with_file_name("saves")
    }
}

/// The number `value` is in digits of `radix` and nothing else (no sign,
/// no prefix, no space), if it is one and fits 64 bits.
fn plain_number(value: &str, radix: u32) -> Option<u64> {
    let digits = !value.is_empty() && value.chars().all(|c| c.is_digit(radix));
    digits.then(|| u64::from_str_radix(value, radix).ok())?
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

/// The trimmed text of one element: comments inside it are skipped, not
/// treated as the end of its value. A manifest generator can write
/// `<TitleId><!-- generated -->639AA8DD</TitleId>`, which must still give
/// the title's id rather than an empty field.
fn field_text(rest: &str) -> String {
    let mut out = String::new();
    let mut rest = rest;
    loop {
        let Some(open) = rest.find('<') else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..open]);
        if rest[open..].starts_with("<!--") {
            let Some(end) = rest[open..].find("-->") else {
                break;
            };
            rest = &rest[open + end + 3..];
        } else {
            // A child element, a closing tag or anything else ends the text.
            break;
        }
    }
    out.trim().to_owned()
}

/// Each element of an XML text as its path from the root (`Game/TitleId`)
/// and its trimmed text up to the first child, the predefined entities
/// decoded; comments, CDATA sections, declarations and attributes are
/// skipped. Names are taken as written (a namespace prefix is part of the
/// name), and the text ends at a tag that never closes; that there was
/// one, and of which kind, is the second value.
fn elements(text: &str) -> (Vec<(String, String)>, Option<Refusal>) {
    let mut out = Vec::new();
    let mut path: Vec<&str> = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        rest = &rest[open..];
        let end = if rest.starts_with("<!--") {
            rest.find("-->").map(|e| e + 3).ok_or(Refusal::UnclosedTag)
        } else if rest.starts_with("<![CDATA[") {
            rest.find("]]>").map(|e| e + 3).ok_or(Refusal::UnclosedTag)
        } else {
            tag_end(rest)
        };
        let end = match end {
            Ok(end) => end,
            Err(cut) => return (out, Some(cut)),
        };
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
        let body = field_text(rest);
        let body = [
            ("&lt;", "<"),
            ("&gt;", ">"),
            ("&quot;", "\""),
            ("&apos;", "'"),
            ("&amp;", "&"),
        ]
        .iter()
        .fold(body, |b, (entity, c)| b.replace(entity, c));
        out.push((path.join("/"), body));
        if closed {
            path.pop();
        }
    }
    (out, None)
}

/// Where the tag that starts `text` ends, past its `>`; a `>` in a quoted
/// attribute value is part of the value (XML allows it there unescaped).
/// A tag that never ends says why: a quote in it that never closes, or
/// no `>` at all.
fn tag_end(text: &str) -> Result<usize, Refusal> {
    let mut quote = None;
    for (at, c) in text.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(open), _) if c == open => quote = None,
            (None, '>') => return Ok(at + 1),
            _ => {}
        }
    }
    Err(match quote {
        Some(_) => Refusal::UnbalancedQuote,
        None => Refusal::UnclosedTag,
    })
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
    fn a_quoted_angle_bracket_does_not_end_its_tag() {
        let text = "<Game><ShellVisuals DefaultDisplayName=\"A > B\" Description='x>y'/>\
                    <TitleId>639AA8DD</TitleId>\
                    <PersistentLocalStorage><SizeMB>64</SizeMB></PersistentLocalStorage></Game>";
        let t = Title::from_manifest(text);
        assert_eq!((t.id, t.storage_bytes), (0x639A_A8DD, 64 << 20));
    }

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

    fn refused(text: &str) -> Vec<Refusal> {
        let manifest = Title::read_manifest(text.as_bytes());
        assert_eq!(manifest.title, Title::from_manifest(text), "{text}");
        manifest.refused
    }

    /// A manifest that breaks one of the reader's rules still yields a
    /// title (id 0 where the id was refused), and says which rule: the
    /// caller reports it, so that a title does not run as 00000000 with
    /// no word of why. A manifest that only lacks an element breaks none.
    #[test]
    fn a_refused_manifest_says_which_rule_it_broke() {
        use Refusal::*;
        let size = |size: &str| {
            format!(
                "<Game><PersistentLocalStorage><SizeMB>{size}</SizeMB>\
                 </PersistentLocalStorage></Game>"
            )
        };
        let id = |id: &str| format!("<Game><TitleId>{id}</TitleId></Game>");

        // Nothing wrong, or nothing there.
        let whole = format!(
            "<?xml version=\"1.0\"?><Game note=\"a>b\">{ID}<StoreId>9ABC</StoreId>\
             <PersistentLocalStorage><SizeMB>16</SizeMB></PersistentLocalStorage></Game>"
        );
        let none: &[Refusal] = &[];
        for text in [
            whole.as_str(),
            "",
            "<Game/>",
            "<Game><StoreId>A</StoreId></Game>",
            "text",
        ] {
            assert_eq!(refused(text), none, "{text}");
        }
        // A prefixed name is another element: the ones we know are simply
        // not there, which is no broken rule.
        assert_eq!(refused("<m:Game><m:TitleId>1</m:TitleId></m:Game>"), none);

        // Said twice.
        assert_eq!(
            refused(&format!("<Game>{ID}{ID}</Game>")),
            [Repeated(Field::TitleId)]
        );
        assert_eq!(
            refused(&format!("<Game>{ID}<TitleId/></Game>")),
            [Repeated(Field::TitleId)]
        );
        let two_roots = "<Game><TitleId>639AA8DD</TitleId><StoreId>A</StoreId></Game>\
                         <Game><TitleId>11111111</TitleId><StoreId>B</StoreId></Game>";
        assert_eq!(
            refused(two_roots),
            [Repeated(Field::TitleId), Repeated(Field::StoreId)]
        );
        assert_eq!(
            refused("<Game><StoreId>A</StoreId><StoreId>A</StoreId></Game>"),
            [Repeated(Field::StoreId)]
        );
        let twice = "<Game><PersistentLocalStorage><SizeMB>1</SizeMB><SizeMB>1</SizeMB>\
                     </PersistentLocalStorage></Game>";
        assert_eq!(refused(twice), [Repeated(Field::StorageSize)]);

        // Not the plain digits the tools write.
        for text in [
            "+639AA8DD",
            "-1",
            "0x639AA8DD",
            "639A A8DD",
            "639AA8DD0",
            "639AA8DG",
            "",
        ] {
            let manifest = Title::read_manifest(id(text).as_bytes());
            assert_eq!(manifest.refused, [TitleIdNotHex], "{text:?}");
            assert_eq!(manifest.title.id, 0, "{text:?}");
        }
        assert_eq!(refused("<Game><TitleId/></Game>"), [TitleIdNotHex]);
        assert_eq!(refused(&id("FFFFFFFF")), none);
        for text in [
            "+16",
            "-16",
            "0x10",
            "1 6",
            "16MB",
            "1.5",
            "",
            "99999999999999999999",
        ] {
            let manifest = Title::read_manifest(size(text).as_bytes());
            assert_eq!(manifest.refused, [SizeNotDecimal], "{text:?}");
            assert_eq!(manifest.title.storage_bytes, 0, "{text:?}");
        }
        assert_eq!(refused(&size("18446744073709551615")), none);

        // Cut short: by a quote that never closes, or a tag, comment or
        // CDATA section that never does. What stood before it is taken.
        let quote = "<Game note=\"><TitleId>639AA8DD</TitleId></Game>";
        let manifest = Title::read_manifest(quote.as_bytes());
        assert_eq!(
            (manifest.title.id, &manifest.refused[..]),
            (0, &[UnbalancedQuote][..])
        );
        let later = format!("<Game>{ID}<Other a='></Other></Game>");
        let manifest = Title::read_manifest(later.as_bytes());
        assert_eq!(
            (manifest.title.id, &manifest.refused[..]),
            (0x639A_A8DD, &[UnbalancedQuote][..])
        );
        for text in [
            "<Game><TitleId",
            "<Game><!-- <TitleId>639AA8DD</TitleId></Game>",
            "<Game><![CDATA[ <TitleId>639AA8DD</TitleId></Game>",
            "<",
            "</",
        ] {
            assert_eq!(refused(text), [UnclosedTag], "{text}");
        }

        // Several at once: the file first, then id, store id, size.
        let all = format!(
            "<Game><PersistentLocalStorage><SizeMB>+16</SizeMB></PersistentLocalStorage>\
             <StoreId>A</StoreId><StoreId>B</StoreId>{ID}{ID}<Other"
        );
        assert_eq!(
            refused(&all),
            [
                UnclosedTag,
                Repeated(Field::TitleId),
                Repeated(Field::StoreId),
                SizeNotDecimal
            ]
        );
        assert_eq!(Title::from_manifest(&all), Title::default());

        // Not text in its encoding: nothing is read, and that is the one
        // reason.
        let utf16: Vec<u8> = whole.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let marked = [&[0xff, 0xfe][..], &utf16].concat();
        assert_eq!(Title::read_manifest(&marked).refused, none);
        let odd = &marked[..marked.len() - 1];
        let lone_surrogate = [&marked[..], &[0x00, 0xd8]].concat();
        for bytes in [odd, &lone_surrogate, b"\xff", b"<Game>\xff</Game>"] {
            let manifest = Title::read_manifest(bytes);
            assert_eq!(manifest.refused, [Encoding], "{bytes:?}");
            assert_eq!(manifest.title, Title::default(), "{bytes:?}");
        }
    }

    /// The rules have names of their own, and their sentences name the
    /// element they are about.
    #[test]
    fn refusals_name_their_rule() {
        use Refusal::*;
        let all = [
            Encoding,
            UnclosedTag,
            UnbalancedQuote,
            Repeated(Field::TitleId),
            Repeated(Field::StoreId),
            Repeated(Field::StorageSize),
            TitleIdNotHex,
            SizeNotDecimal,
        ];
        let names: std::collections::HashSet<&str> = all.iter().map(|r| r.rule()).collect();
        assert_eq!(names.len(), 6, "{names:?}");
        let sentences: std::collections::HashSet<String> =
            all.iter().map(|r| r.to_string()).collect();
        assert_eq!(sentences.len(), all.len(), "{sentences:?}");
        assert!(TitleIdNotHex.to_string().contains("Game/TitleId"));
        assert!(
            Repeated(Field::StoreId)
                .to_string()
                .contains("Game/StoreId")
        );
        let size = "Game/PersistentLocalStorage/SizeMB";
        assert!(SizeNotDecimal.to_string().contains(size));
        assert!(Repeated(Field::StorageSize).to_string().contains(size));
    }

    /// Collects the rule each warning names, on the thread it is set for.
    struct Warnings(std::sync::Arc<Mutex<Vec<String>>>);

    impl tracing::Subscriber for Warnings {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            struct Rule(String);
            impl tracing::field::Visit for Rule {
                fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                    if field.name() == "rule" {
                        self.0 = value.to_owned();
                    }
                }
                fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
            }
            let mut rule = Rule("no rule".to_owned());
            event.record(&mut rule);
            if *event.metadata().level() != tracing::Level::WARN {
                rule.0 += " (not a warning)";
            }
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(rule.0);
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// What reading `read` logs: the rule of each event, in order.
    fn logged(read: impl FnOnce()) -> Vec<String> {
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        // A second subscriber, alive while ours is made: with only one in
        // the process, tracing decides whether a warning already seen by
        // another test is of interest by asking this thread's subscriber
        // before ours is set, and the answer (no) sticks.
        let other = std::sync::Arc::new(Mutex::new(Vec::new()));
        let _other = tracing::Dispatch::new(Warnings(other));
        tracing::subscriber::with_default(Warnings(seen.clone()), read);
        let seen = seen.lock().unwrap_or_else(|e| e.into_inner());
        seen.clone()
    }

    /// Each refusal is one warning that names its rule, whichever way the
    /// manifest is read; a manifest that breaks no rule logs nothing.
    #[test]
    fn each_refusal_is_one_warning() {
        let all = format!(
            "<Game><PersistentLocalStorage><SizeMB>+16</SizeMB></PersistentLocalStorage>\
             {ID}{ID}<Other a=\">"
        );
        let rules = ["unbalanced-quote", "repeated-element", "size-not-decimal"];
        assert_eq!(logged(|| drop(Title::read_manifest(all.as_bytes()))), rules);
        assert_eq!(
            logged(|| drop(Title::from_manifest_bytes(all.as_bytes()))),
            rules
        );
        assert_eq!(logged(|| drop(Title::from_manifest(&all))), rules);
        assert_eq!(logged(|| drop(Title::read_manifest(b"\xff"))), ["encoding"]);
        assert_eq!(
            logged(|| drop(game("<TitleId>-1</TitleId>"))),
            ["title-id-not-hex"]
        );
        assert_eq!(
            logged(|| drop(Title::from_manifest("<Game><TitleId"))),
            ["unclosed-tag"]
        );
        let none: [&str; 0] = [];
        assert_eq!(logged(|| drop(game(ID))), none);
        assert_eq!(logged(|| drop(Title::read_manifest(b""))), none);
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
            let (found, _) = elements(&text);
            assert!(found.len() <= text.matches('<').count(), "{text:?}");
            let t = Title::from_manifest(&text);
            assert_eq!(t.storage, PathBuf::new());
            let named = |name: &str| found.iter().any(|(path, _)| path == name);
            assert!(t.id == 0 || named("Game/TitleId"), "{text:?}");
            assert!(t.store_id.is_empty() || named("Game/StoreId"), "{text:?}");
            assert_eq!(t.storage_bytes % (1 << 20), 0, "{text:?}");
            // A title id element that gave no id always has its reason:
            // no manifest quietly becomes title 00000000.
            let manifest = Manifest::parse(&text);
            assert_eq!(manifest.title, t, "{text:?}");
            let ids = found.iter().filter(|(path, _)| path == "Game/TitleId");
            let ids: Vec<&str> = ids.map(|(_, value)| value.as_str()).collect();
            if t.id == 0 && !ids.is_empty() {
                let zero = ids.len() == 1 && plain_number(ids[0], 16) == Some(0);
                let reason = |r: &Refusal| {
                    matches!(
                        r,
                        Refusal::Repeated(Field::TitleId) | Refusal::TitleIdNotHex
                    )
                };
                assert!(zero || manifest.refused.iter().any(reason), "{text:?}");
            }
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

    #[test]
    fn inline_comments_do_not_truncate_a_field() {
        let text = "<Game>\
            <TitleId><!-- generated -->639AA8DD</TitleId>\
            <StoreId>9&amp;<!-- keep -->C</StoreId>\
            <PersistentLocalStorage><SizeMB><!-- n -->1024</SizeMB></PersistentLocalStorage>\
            </Game>";
        let t = Title::from_manifest(text);
        assert_eq!(t.id, 0x639A_A8DD);
        assert_eq!(t.store_id, "9&C");
        assert_eq!(t.storage_bytes, 1024 << 20);
    }

    /// A comment inside a field that never closes ends the field's text
    /// where it opens, and is reported as the tag that never closes.
    #[test]
    fn an_unclosed_comment_in_a_field_ends_it_and_is_reported() {
        let text = "<Game><TitleId>639AA8DD<!-- cut</TitleId><StoreId>9C</StoreId></Game>";
        let manifest = Title::read_manifest(text.as_bytes());
        assert_eq!(manifest.refused, [Refusal::UnclosedTag]);
        assert_eq!(manifest.title.id, 0x639A_A8DD);
        // Nothing after it is read.
        assert_eq!(manifest.title.store_id, "");
    }
}
