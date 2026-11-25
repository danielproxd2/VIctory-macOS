use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use crate::Converter;

/// Identifies one conversion result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey(blake3::Hash);

impl CacheKey {
    pub fn new(converter: &dyn Converter, input: &[u8]) -> Self {
        let mut h = blake3::Hasher::new();
        h.update(converter.id().as_bytes());
        h.update(&[0]);
        h.update(&converter.version().to_le_bytes());
        h.update(input);
        CacheKey(h.finalize())
    }

    pub fn to_hex(&self) -> String {
        self.0.to_hex().to_string()
    }
}

/// Content-addressed store under the user's cache directory.
///
/// An entry is one file, `<first two hex digits of the key>/<the rest>`:
/// a 40-byte header (the output's length as a little-endian `u64`, then
/// the BLAKE3 of the output) and the output. The header is checked every
/// time an entry is read, so a file that was cut short, damaged, or
/// written by a version without the header counts as missing and is
/// converted again. Entries are staged as `.tmp-<pid>-<n>` beside their
/// final name and renamed into place.
#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
}

const HEADER_BYTES: usize = 8 + blake3::OUT_LEN;
const STAGING_PREFIX: &str = ".tmp-";
/// A staging file this old belongs to a run that died: no conversion's
/// output takes an hour to write.
const STALE_STAGING: Duration = Duration::from_secs(60 * 60);

impl Cache {
    /// Opens (creating) a cache at `root`. Refuses locations inside the repo.
    /// Staging files that runs which died left behind are removed.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        victory_core::ensure_outside_repo(&root)?;
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        let cache = Cache { root };
        cache.remove_stale_staging_files();
        Ok(cache)
    }

    /// Where the entry for `key` is, whether or not it exists. The file is
    /// not the output alone: read it with [`Cache::read`].
    pub fn path_for(&self, key: &CacheKey) -> PathBuf {
        let hex = key.to_hex();
        self.root.join(&hex[..2]).join(&hex[2..])
    }

    /// The stored output for `key`; `None` when there is no entry or the
    /// file is not a whole entry.
    pub fn read(&self, key: &CacheKey) -> Option<Vec<u8>> {
        read_entry(&self.path_for(key))
    }

    /// Forgets the entry for `key`, for a caller that cannot use what it
    /// holds.
    pub fn remove(&self, key: &CacheKey) {
        let _ = std::fs::remove_file(self.path_for(key));
    }

    /// Returns the cached output for `input`, converting it first if needed.
    pub fn get_or_convert(&self, converter: &dyn Converter, input: &[u8]) -> Result<Vec<u8>> {
        let key = CacheKey::new(converter, input);
        let path = self.path_for(&key);
        if let Some(output) = read_entry(&path) {
            tracing::trace!(converter = converter.id(), key = %key.to_hex(), "cache hit");
            return Ok(output);
        }
        let output = converter
            .convert(input)
            .with_context(|| format!("converter {} v{}", converter.id(), converter.version()))?;
        let dir = path.parent().expect("hashed path has a parent");
        std::fs::create_dir_all(dir)?;
        // Write-then-rename so readers never see partial output.
        let tmp = tempfile_in(dir);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        let written = file
            .write_all(&header(&output))
            .and_then(|()| file.write_all(&output));
        drop(file);
        let published = written.and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(e) = published {
            let _ = std::fs::remove_file(&tmp);
            // Another thread or process may have published the same entry
            // first, and on Windows a rename onto a file someone is reading
            // can be refused. A whole entry at the name is what was wanted.
            if read_entry(&path).is_none() {
                return Err(e).with_context(|| format!("storing {}", path.display()));
            }
        }
        Ok(output)
    }

    /// Removes the entries written longest ago until those left hold at
    /// most `max_bytes` (file sizes, headers included). Returns how many
    /// were removed. Reading an entry does not make it newer.
    pub fn prune(&self, max_bytes: u64) -> Result<usize> {
        let mut entries = vec![];
        for dir in self.shards() {
            for entry in std::fs::read_dir(&dir)?.flatten() {
                let Ok(meta) = entry.metadata() else { continue };
                if !meta.is_file() || is_staging(&entry.file_name()) {
                    continue;
                }
                let written = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                entries.push((written, entry.path(), meta.len()));
            }
        }
        // Newest first; the path settles ties so a run is repeatable.
        entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let (mut kept, mut removed) = (0u64, 0);
        for (_, path, len) in entries {
            if kept.saturating_add(len) <= max_bytes {
                kept += len;
            } else if std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// The folders entries are in: two hex digits under the root.
    fn shards(&self) -> Vec<PathBuf> {
        let Ok(dirs) = std::fs::read_dir(&self.root) else {
            return vec![];
        };
        dirs.flatten()
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.len() == 2 && name.bytes().all(|b| b.is_ascii_hexdigit())
            })
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect()
    }

    fn remove_stale_staging_files(&self) {
        for dir in self.shards() {
            let Ok(files) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in files.flatten() {
                if !is_staging(&entry.file_name()) {
                    continue;
                }
                // A time in the future has no age: the file stays.
                let age = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok());
                if age.is_some_and(|a| a >= STALE_STAGING) {
                    tracing::debug!(path = %entry.path().display(), "removing a stale staging file");
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}

fn is_staging(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().starts_with(STAGING_PREFIX)
}

fn header(output: &[u8]) -> [u8; HEADER_BYTES] {
    let mut h = [0; HEADER_BYTES];
    h[..8].copy_from_slice(&(output.len() as u64).to_le_bytes());
    h[8..].copy_from_slice(blake3::hash(output).as_bytes());
    h
}

/// The output in the entry file at `path`, if the file is a whole entry:
/// the header's length and hash are those of the bytes after it.
fn read_entry(path: &Path) -> Option<Vec<u8>> {
    let mut data = std::fs::read(path).ok()?;
    let output = data.get(HEADER_BYTES..)?;
    if data[..HEADER_BYTES] != header(output) {
        tracing::debug!(path = %path.display(), "cache entry does not match its header");
        return None;
    }
    data.drain(..HEADER_BYTES);
    Some(data)
}

fn tempfile_in(dir: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("{STAGING_PREFIX}{}-{n}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Upper {
        version: u32,
        runs: AtomicUsize,
    }

    impl Converter for Upper {
        fn id(&self) -> &'static str {
            "test.upper"
        }
        fn version(&self) -> u32 {
            self.version
        }
        fn convert(&self, input: &[u8]) -> Result<Vec<u8>> {
            self.runs.fetch_add(1, Ordering::Relaxed);
            Ok(input.to_ascii_uppercase())
        }
    }

    fn upper() -> Upper {
        Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        }
    }

    /// Every file under the cache, relative to its root.
    fn files(cache: &Cache) -> Vec<PathBuf> {
        let mut found = vec![];
        for dir in cache.shards() {
            for file in std::fs::read_dir(dir).unwrap().flatten() {
                found.push(file.path().strip_prefix(&cache.root).unwrap().to_owned());
            }
        }
        found.sort();
        found
    }

    fn staging_files(cache: &Cache) -> Vec<PathBuf> {
        files(cache)
            .into_iter()
            .filter(|f| is_staging(f.file_name().unwrap()))
            .collect()
    }

    #[test]
    fn converts_once_then_hits() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let conv = upper();
        let key = CacheKey::new(&conv, b"abc");
        assert_eq!(cache.read(&key), None);
        let a = cache.get_or_convert(&conv, b"abc").unwrap();
        let b = cache.get_or_convert(&conv, b"abc").unwrap();
        assert_eq!((a.as_slice(), b.as_slice()), (&b"ABC"[..], &b"ABC"[..]));
        assert_eq!(conv.runs.load(Ordering::Relaxed), 1);
        assert_eq!(cache.read(&key).as_deref(), Some(&b"ABC"[..]));

        // The file is the header, then the output.
        let file = std::fs::read(cache.path_for(&key)).unwrap();
        assert_eq!(file.len(), HEADER_BYTES + 3);
        assert_eq!(file[..8], 3u64.to_le_bytes());
        assert_eq!(&file[8..HEADER_BYTES], blake3::hash(b"ABC").as_bytes());
        assert_eq!(&file[HEADER_BYTES..], b"ABC");

        // An empty output is an entry like any other.
        assert_eq!(cache.get_or_convert(&conv, b"").unwrap(), b"");
        assert_eq!(cache.get_or_convert(&conv, b"").unwrap(), b"");
        assert_eq!(conv.runs.load(Ordering::Relaxed), 2);

        cache.remove(&key);
        assert_eq!(cache.read(&key), None);
        assert_eq!(cache.get_or_convert(&conv, b"abc").unwrap(), b"ABC");
        assert_eq!(conv.runs.load(Ordering::Relaxed), 3);
    }

    /// Stores the entry for `input`, lets `damage` change its file, and
    /// expects the next request to convert again, return the right output
    /// and leave a whole entry behind.
    fn damaged_entry_is_converted_again(input: &[u8], damage: impl Fn(&mut Vec<u8>)) {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let conv = upper();
        let key = CacheKey::new(&conv, input);
        let expected = input.to_ascii_uppercase();
        assert_eq!(cache.get_or_convert(&conv, input).unwrap(), expected);
        let path = cache.path_for(&key);
        let whole = std::fs::read(&path).unwrap();
        let mut file = whole.clone();
        damage(&mut file);
        assert_ne!(file, whole);
        std::fs::write(&path, &file).unwrap();

        assert_eq!(cache.read(&key), None, "a damaged entry is not served");
        assert_eq!(cache.get_or_convert(&conv, input).unwrap(), expected);
        assert_eq!(conv.runs.load(Ordering::Relaxed), 2, "converted again");
        assert_eq!(std::fs::read(&path).unwrap(), whole, "and stored again");
        assert_eq!(cache.get_or_convert(&conv, input).unwrap(), expected);
        assert_eq!(conv.runs.load(Ordering::Relaxed), 2);
        assert_eq!(staging_files(&cache), Vec::<PathBuf>::new());
    }

    #[test]
    fn a_truncated_entry_is_converted_again() {
        let input = b"an input long enough to cut in several places";
        let len = HEADER_BYTES + input.len();
        for keep in [
            len - 1,
            len - 7,
            HEADER_BYTES + 1,
            HEADER_BYTES,
            HEADER_BYTES - 1,
            8,
            1,
        ] {
            damaged_entry_is_converted_again(input, |file| file.truncate(keep));
        }
        // Longer than its header says is no better.
        damaged_entry_is_converted_again(input, |file| file.push(b'!'));
    }

    #[test]
    fn a_corrupted_entry_is_converted_again() {
        let input = b"an input whose stored output gets a bit flipped";
        // In the length, in the hash, in the output's first and last byte.
        for at in [
            0,
            7,
            8,
            HEADER_BYTES - 1,
            HEADER_BYTES,
            HEADER_BYTES + input.len() - 1,
        ] {
            damaged_entry_is_converted_again(input, |file| file[at] ^= 0x10);
        }
        // What the cache wrote before entries had a header: the output alone.
        damaged_entry_is_converted_again(input, |file| {
            file.drain(..HEADER_BYTES);
        });
        // A length that claims far more than the file holds.
        damaged_entry_is_converted_again(input, |file| {
            file[..8].copy_from_slice(&u64::MAX.to_le_bytes());
        });
    }

    #[test]
    fn an_empty_entry_is_converted_again() {
        damaged_entry_is_converted_again(b"abc", |file| file.clear());
        // Also when the output itself is empty: the header must be there.
        damaged_entry_is_converted_again(b"", |file| file.clear());
    }

    #[test]
    fn stale_staging_files_are_removed_on_open() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let cache = Cache::open(&root).unwrap();
        let conv = upper();
        cache.get_or_convert(&conv, b"abc").unwrap();
        let entry = cache.path_for(&CacheKey::new(&conv, b"abc"));
        let dir = entry.parent().unwrap();

        let aged = |name: &str, age: Duration| {
            let path = dir.join(name);
            std::fs::write(&path, b"half an entry").unwrap();
            let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.set_modified(SystemTime::now() - age).unwrap();
            path
        };
        let hours = |n: u64| Duration::from_secs(n * 60 * 60);
        let stale = aged(".tmp-4242-0", hours(2));
        let ancient = aged(".tmp-1-99", hours(24 * 400));
        let recent = aged(".tmp-4242-1", Duration::from_secs(60));
        let fresh = aged(".tmp-4242-2", Duration::ZERO);
        // Old, but not a staging file: an entry, and a file of someone else's.
        let other = aged("tmp-notes", hours(48));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&entry)
            .unwrap();
        file.set_modified(SystemTime::now() - hours(48)).unwrap();
        drop(file);
        // A staging name outside the entry folders is not ours to remove.
        let outside = root.join(".tmp-4242-3");
        std::fs::write(&outside, b"x").unwrap();
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&outside)
            .unwrap();
        file.set_modified(SystemTime::now() - hours(48)).unwrap();
        drop(file);

        let cache = Cache::open(&root).unwrap();
        assert!(!stale.exists() && !ancient.exists(), "stale files are gone");
        assert!(
            recent.exists() && fresh.exists(),
            "a running conversion's stay"
        );
        assert!(other.exists() && outside.exists() && entry.exists());
        assert_eq!(cache.get_or_convert(&conv, b"abc").unwrap(), b"ABC");
        assert_eq!(
            conv.runs.load(Ordering::Relaxed),
            1,
            "the entry is still a hit"
        );
    }

    /// Makes conversions of one input overlap. A conversion leaves a file
    /// in the folder of its input and does not return before `parties`
    /// files are there: every party is inside the converter for that input
    /// at the same moment, whether the parties are threads or processes.
    /// A party that waits in vain fails its conversion and tells the rest.
    struct Meet {
        dir: PathBuf,
        parties: usize,
    }

    /// Far longer than a party needs to arrive, also for a process that
    /// Wine has yet to start.
    const MEET_TIMEOUT: Duration = Duration::from_secs(60);

    impl Meet {
        fn wait(&self, input: &[u8]) -> Result<()> {
            static ARRIVALS: AtomicUsize = AtomicUsize::new(0);
            let place = self.dir.join(&blake3::hash(input).to_hex()[..16]);
            std::fs::create_dir_all(&place)?;
            let n = ARRIVALS.fetch_add(1, Ordering::Relaxed);
            std::fs::write(place.join(format!("{}-{n}", std::process::id())), b"")?;
            let failed = self.dir.join("failed");
            let deadline = std::time::Instant::now() + MEET_TIMEOUT;
            loop {
                let inside = std::fs::read_dir(&place)?.count();
                if inside >= self.parties {
                    return Ok(());
                }
                if failed.exists() || std::time::Instant::now() >= deadline {
                    let _ = std::fs::write(&failed, b"");
                    anyhow::bail!(
                        "{} conversions of one input did not overlap ({inside} arrived)",
                        self.parties
                    );
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    /// Its output depends on nothing but the input. With a [`Meet`], a
    /// conversion waits for the others inside it.
    struct Slow {
        runs: AtomicUsize,
        meet: Option<Meet>,
    }

    impl Slow {
        fn new(meet: Option<Meet>) -> Self {
            let runs = AtomicUsize::new(0);
            Slow { runs, meet }
        }
        fn runs(&self) -> usize {
            self.runs.load(Ordering::Relaxed)
        }
    }

    impl Converter for Slow {
        fn id(&self) -> &'static str {
            "test.slow"
        }
        fn version(&self) -> u32 {
            1
        }
        fn convert(&self, input: &[u8]) -> Result<Vec<u8>> {
            self.runs.fetch_add(1, Ordering::Relaxed);
            if let Some(meet) = &self.meet {
                meet.wait(input)?;
            }
            Ok(slow_output(input))
        }
    }

    fn slow_output(input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 << 10);
        let mut block = *blake3::hash(input).as_bytes();
        while out.len() < 64 << 10 {
            out.extend_from_slice(&block);
            block = *blake3::hash(&block).as_bytes();
        }
        out
    }

    /// The whole entry for `input`, with one bit of its output flipped: an
    /// entry file of the right size that only its hash gives away.
    fn store_damaged(cache: &Cache, input: &[u8]) {
        let path = cache.path_for(&CacheKey::new(&Slow::new(None), input));
        let output = slow_output(input);
        let mut file = header(&output).to_vec();
        file.extend_from_slice(&output);
        *file.last_mut().unwrap() ^= 1;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, file).unwrap();
    }

    #[test]
    fn threads_converting_one_key_publish_one_consistent_output() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        const THREADS: usize = 8;
        const KEYS: usize = 24;
        let inputs: Vec<Vec<u8>> = (0..KEYS).map(|k| format!("key {k}").into_bytes()).collect();
        // Every thread asks for every key; what went wrong, if anything.
        let all_ask = |conv: &Slow| -> Vec<String> {
            std::thread::scope(|scope| {
                let threads: Vec<_> = (0..THREADS)
                    .map(|_| {
                        scope.spawn(|| {
                            let mut failures = vec![];
                            for input in &inputs {
                                match cache.get_or_convert(conv, input) {
                                    Ok(out) if out == slow_output(input) => {}
                                    Ok(out) => failures.push(format!("{} wrong bytes", out.len())),
                                    Err(e) => failures.push(format!("{e:#}")),
                                }
                            }
                            failures
                        })
                    })
                    .collect();
                threads
                    .into_iter()
                    .flat_map(|t| t.join().expect("no thread panics"))
                    .collect()
            })
        };
        // One whole entry per key and nothing else.
        let whole = |round: &str| {
            assert_eq!(staging_files(&cache), Vec::<PathBuf>::new(), "{round}");
            assert_eq!(files(&cache).len(), KEYS, "{round}");
            for input in &inputs {
                let key = CacheKey::new(&Slow::new(None), input);
                assert_eq!(cache.read(&key), Some(slow_output(input)), "{round}");
            }
        };
        let meeting = |round: &str| {
            Slow::new(Some(Meet {
                dir: tmp.path().join(round),
                parties: THREADS,
            }))
        };

        // Nothing is stored: no conversion of a key returns before every
        // thread is converting that key, and then all of them publish it.
        let conv = meeting("empty");
        assert_eq!(all_ask(&conv), Vec::<String>::new(), "empty cache");
        assert_eq!(conv.runs(), THREADS * KEYS, "all overlapped");
        whole("empty cache");

        // Everything is stored: nobody converts.
        let conv = Slow::new(None);
        assert_eq!(all_ask(&conv), Vec::<String>::new(), "full cache");
        assert_eq!(conv.runs(), 0, "all hits");
        whole("full cache");

        // Every entry is damaged: no thread is served one, all of them
        // convert every key again, together, and whole entries replace
        // the damaged ones while other threads are reading them.
        for input in &inputs {
            store_damaged(&cache, input);
        }
        let conv = meeting("damaged");
        assert_eq!(all_ask(&conv), Vec::<String>::new(), "damaged cache");
        assert_eq!(conv.runs(), THREADS * KEYS, "none served, all overlapped");
        whole("damaged cache");
    }

    const CHILDREN: usize = 3;
    const CHILD_KEYS: usize = 40;

    fn child_inputs() -> Vec<Vec<u8>> {
        (0..CHILD_KEYS)
            .map(|k| format!("shared key {k}").into_bytes())
            .collect()
    }

    /// One of the processes of `processes_converting_one_key_agree`; does
    /// nothing in a normal run.
    #[test]
    fn convert_child() {
        let Some(root) = std::env::var_os("VICTORY_CACHE_CHILD") else {
            return;
        };
        let cache = Cache::open(PathBuf::from(root)).unwrap();
        // The folder's name is no entry folder's.
        let conv = Slow::new(Some(Meet {
            dir: cache.root.join("meet"),
            parties: CHILDREN,
        }));
        let mut bad = 0;
        for input in child_inputs() {
            match cache.get_or_convert(&conv, &input) {
                Ok(out) if out == slow_output(&input) => {}
                Ok(_) => {
                    println!("ERROR wrong bytes");
                    bad += 1;
                }
                Err(e) => {
                    println!("ERROR {e:#}");
                    bad += 1;
                }
            }
        }
        println!("DONE bad={bad} runs={}", conv.runs());
    }

    /// Each process is inside the converter for every key while the others
    /// are (the conversions meet), so all of them publish every entry at
    /// about the same time. A third of the keys start with a damaged
    /// entry, which no process may be served.
    #[test]
    fn processes_converting_one_key_agree() {
        use std::io::Read;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let cache = Cache::open(&root).unwrap();
        for input in child_inputs().iter().step_by(3) {
            store_damaged(&cache, input);
        }
        let children: Vec<_> = (0..CHILDREN)
            .map(|_| {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "cache::tests::convert_child", "--nocapture"])
                    .args(["--test-threads=1"])
                    .env("VICTORY_CACHE_CHILD", &root)
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for mut child in children {
            let mut text = String::new();
            let mut out = child.stdout.take().unwrap();
            out.read_to_string(&mut text).unwrap();
            assert!(child.wait().unwrap().success(), "{text}");
            let done = text
                .lines()
                .find_map(|l| l.split_once("DONE bad=0 runs="))
                .unwrap_or_else(|| panic!("a child failed: {text}"));
            let runs: usize = done.1.split_whitespace().next().unwrap().parse().unwrap();
            assert_eq!(runs, CHILD_KEYS, "this child converted every key: {text}");
        }
        // Every entry is whole, and nothing is left of the publishing.
        assert_eq!(staging_files(&cache), Vec::<PathBuf>::new());
        assert_eq!(files(&cache).len(), CHILD_KEYS);
        let conv = Slow::new(None);
        for input in child_inputs() {
            assert_eq!(
                cache.get_or_convert(&conv, &input).unwrap(),
                slow_output(&input)
            );
        }
        assert_eq!(conv.runs(), 0, "all hits afterwards");
    }

    #[test]
    fn prune_keeps_the_newest_entries_within_the_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let conv = upper();
        // Five entries of 100 bytes of output each, written an hour apart,
        // the first the oldest.
        let inputs: Vec<Vec<u8>> = (0..5u8).map(|i| vec![b'a' + i; 100]).collect();
        let size = (HEADER_BYTES + 100) as u64;
        let now = SystemTime::now();
        for (i, input) in inputs.iter().enumerate() {
            cache.get_or_convert(&conv, input).unwrap();
            let path = cache.path_for(&CacheKey::new(&conv, input));
            let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            let age = Duration::from_secs(60 * 60 * (5 - i as u64));
            file.set_modified(now - age).unwrap();
        }
        let held = |cache: &Cache| -> Vec<bool> {
            inputs
                .iter()
                .map(|i| cache.read(&CacheKey::new(&conv, i)).is_some())
                .collect()
        };
        // A staging file is not an entry: prune leaves it alone.
        let staging = cache
            .path_for(&CacheKey::new(&conv, &inputs[0]))
            .with_file_name(".tmp-1-1");
        std::fs::write(&staging, vec![0; 1000]).unwrap();

        // Room for everything: nothing goes.
        assert_eq!(cache.prune(5 * size).unwrap(), 0);
        assert_eq!(cache.prune(u64::MAX).unwrap(), 0);
        assert_eq!(held(&cache), [true; 5]);
        // Room for three and a bit: the two oldest go.
        assert_eq!(cache.prune(3 * size + size - 1).unwrap(), 2);
        assert_eq!(held(&cache), [false, false, true, true, true]);
        assert_eq!(cache.prune(3 * size).unwrap(), 0);
        // One byte short of one entry: nothing fits.
        assert_eq!(cache.prune(size - 1).unwrap(), 3);
        assert_eq!(held(&cache), [false; 5]);
        assert!(staging.exists());
        assert_eq!(cache.prune(0).unwrap(), 0);

        // What was pruned is converted again on request.
        let runs = conv.runs.load(Ordering::Relaxed);
        assert_eq!(
            cache.get_or_convert(&conv, &inputs[0]).unwrap(),
            vec![b'A'; 100]
        );
        assert_eq!(conv.runs.load(Ordering::Relaxed), runs + 1);
    }

    #[test]
    fn failed_publication_removes_the_staging_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let conv = upper();
        let path = cache.path_for(&CacheKey::new(&conv, b"abc"));
        std::fs::create_dir_all(&path).unwrap();

        assert!(cache.get_or_convert(&conv, b"abc").is_err());
        assert!(path.is_dir());
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn version_bump_invalidates() {
        let v1 = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let v2 = Upper {
            version: 2,
            runs: AtomicUsize::new(0),
        };
        assert_ne!(CacheKey::new(&v1, b"x"), CacheKey::new(&v2, b"x"));
    }

    #[test]
    fn refuses_cache_inside_repo() {
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("cache");
        assert!(Cache::open(inside).is_err());
    }
}
