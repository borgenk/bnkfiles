//! Thumbnails that survive closing the window.
//!
//! One file per directory under `$XDG_CACHE_HOME/bnkfiles/`, named by a hash of
//! the directory's path. Not the XDG thumbnail spec (a directory of PNGs named by
//! URI hash): one read on warm start beats a thousand `open`s, and the cost — no
//! cache interop with other file managers — is one a personal tool can pay.
//!
//! ```text
//!   "BNKT"  u32 version  u32 count
//!   per entry:  u16 name_len, name
//!               i64 mtime   u64 size      <- the fingerprint
//!               u16 edge    u16 w  u16 h  <- what was made
//!               u32 len, raw ARGB pixels  <- w*h*4 bytes, little-endian
//! ```
//!
//! The payload is raw rather than compressed: see [`pack_pixels`].
//!
//! **Nothing here is trusted.** A cache file is data on disk that another program,
//! a full filesystem, or a crash mid-write can have mangled, and it is read at
//! start-up before anything else has had a chance to go wrong. Every length is
//! checked against what remains before it is used, and *any* inconsistency
//! discards the whole file rather than salvaging part of one: a cache is an
//! optimization, so throwing it away costs a rebuild and nothing else.
//!
//! Per entry, the (name, mtime, size) fingerprint must match the live listing or
//! that thumbnail is ignored. That is what makes an edited photo show its new
//! contents rather than the cached picture of its old ones.

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;

use crate::files::FsEntry;

/// Identifies the format. A file that does not start with these four bytes is
/// not ours and is not read.
const MAGIC: &[u8; 4] = b"BNKT";
/// Bumped whenever the layout below changes, so an old cache is discarded rather
/// than misread as a new one.
const VERSION: u32 = 1;

/// The header, and the fixed part of an entry: name_len(2) + mtime(8) + size(8)
/// + edge(2) + w(2) + h(2) + payload_len(4).
const HEADER: usize = 12;
const ENTRY_FIXED: usize = 28;

/// A ceiling on what will be read back. A directory of thumbnails is a few tens
/// of megabytes; a file claiming more than this is corrupt or hostile, and
/// rebuilding beats trying to load it.
const MAX_FILE: u64 = 256 * 1024 * 1024;
/// A ceiling on one entry's payload, for the same reason.
const MAX_PAYLOAD: usize = 8 * 1024 * 1024;

/// How much one directory's file may hold. Comfortably under [`MAX_FILE`], so a
/// file this app wrote is never one it then refuses to read, and small enough
/// that writing it is not a visible pause on a slow disk. At the default cell
/// size that is several hundred thumbnails; a larger folder keeps the first of
/// them and decodes the rest again.
pub const DIR_BUDGET: usize = 64 * 1024 * 1024;

/// How much disk the whole cache may take before the least recently used
/// directories are dropped.
pub const BUDGET: u64 = 512 * 1024 * 1024;

/// One cached thumbnail: the fingerprint of the file it was made from, the size
/// it was made at, and the pixels themselves.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Cached {
    /// The file's name as the directory holds it, not as it is displayed: two
    /// names that differ only in bytes that are not UTF-8 display alike.
    pub name: OsString,
    pub mtime_secs: i64,
    pub size: u64,
    /// The thumbnail edge the grid asked for when this was made. A cache entry
    /// made for a different cell size is a miss: the display scale changed, and a
    /// 160px thumbnail blown up to 240 looks worse than a fresh decode.
    pub edge: u16,
    pub width: u16,
    pub height: u16,
    /// The thumbnail itself: `width * height` pixels, four little-endian bytes
    /// each, in the same ARGB order the renderer uses.
    pub pixels: Vec<u8>,
}

impl Cached {
    /// The bytes this entry takes in the file.
    fn encoded_len(&self) -> usize {
        ENTRY_FIXED + self.name.len() + self.pixels.len()
    }

    /// Whether this entry still describes `entry`, at the size being drawn.
    pub fn matches(&self, entry: &FsEntry, edge: u16) -> bool {
        self.edge == edge
            && self.mtime_secs == entry.mtime_secs
            && self.size == entry.size
            && entry.path.file_name() == Some(self.name.as_os_str())
    }
}

/// Cut `entries` down to the leading run whose encoded form fits in `budget`
/// bytes. The caller orders them by which it would rather keep.
pub fn fit(entries: &mut Vec<Cached>, budget: usize) {
    let mut size = HEADER;
    let keep = entries
        .iter()
        .take_while(|entry| {
            size += entry.encoded_len();
            size <= budget
        })
        .count();
    entries.truncate(keep);
}

/// Serialize entries into the on-disk form.
pub fn encode(entries: &[Cached]) -> Vec<u8> {
    let size: usize = HEADER + entries.iter().map(Cached::encoded_len).sum::<usize>();
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for entry in entries {
        // A name longer than u16 cannot exist on any filesystem this runs on, but
        // truncating the length silently would desynchronize the whole file, so
        // the entry is skipped instead. (The count above is then one too high,
        // which the reader treats as a short file and discards -- correct, if
        // blunt, and unreachable in practice.)
        let Ok(name_len) = u16::try_from(entry.name.len()) else {
            continue;
        };
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(entry.name.as_bytes());
        out.extend_from_slice(&entry.mtime_secs.to_le_bytes());
        out.extend_from_slice(&entry.size.to_le_bytes());
        out.extend_from_slice(&entry.edge.to_le_bytes());
        out.extend_from_slice(&entry.width.to_le_bytes());
        out.extend_from_slice(&entry.height.to_le_bytes());
        out.extend_from_slice(&(entry.pixels.len() as u32).to_le_bytes());
        out.extend_from_slice(&entry.pixels);
    }
    out
}

/// Parse the on-disk form, or `None` if it is not exactly what it claims to be.
///
/// Every read is bounds-checked before it happens and a single inconsistency
/// fails the whole parse. This is the function a torn write lands in.
pub fn decode(bytes: &[u8]) -> Option<Vec<Cached>> {
    if bytes.len() < HEADER || &bytes[..4] != MAGIC {
        return None;
    }
    if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != VERSION {
        return None;
    }
    let count = u32::from_le_bytes(bytes[8..12].try_into().ok()?) as usize;
    // A count that could not fit in the bytes we have, whatever the payloads.
    if count.checked_mul(ENTRY_FIXED)? > bytes.len() {
        return None;
    }
    let mut entries = Vec::with_capacity(count);
    let mut at = HEADER;
    for _ in 0..count {
        let take = |at: &mut usize, n: usize| -> Option<&[u8]> {
            let end = at.checked_add(n)?;
            let slice = bytes.get(*at..end)?;
            *at = end;
            Some(slice)
        };
        let name_len = u16::from_le_bytes(take(&mut at, 2)?.try_into().ok()?) as usize;
        let name = OsString::from_vec(take(&mut at, name_len)?.to_vec());
        let mtime_secs = i64::from_le_bytes(take(&mut at, 8)?.try_into().ok()?);
        let size = u64::from_le_bytes(take(&mut at, 8)?.try_into().ok()?);
        let edge = u16::from_le_bytes(take(&mut at, 2)?.try_into().ok()?);
        let width = u16::from_le_bytes(take(&mut at, 2)?.try_into().ok()?);
        let height = u16::from_le_bytes(take(&mut at, 2)?.try_into().ok()?);
        let payload_len = u32::from_le_bytes(take(&mut at, 4)?.try_into().ok()?) as usize;
        if payload_len > MAX_PAYLOAD {
            return None;
        }
        // The payload's length is implied by the dimensions beside it. A file
        // where they disagree is one whose pixels would be read as the wrong
        // shape, so it is refused rather than reshaped.
        if payload_len != (width as usize) * (height as usize) * 4 {
            return None;
        }
        let pixels = take(&mut at, payload_len)?.to_vec();
        entries.push(Cached {
            name,
            mtime_secs,
            size,
            edge,
            width,
            height,
            pixels,
        });
    }
    // Trailing bytes mean the file is not what its header said it was.
    (at == bytes.len()).then_some(entries)
}

/// The directory the caches live in.
///
/// Owned rather than read from the environment at each call, so the cache can be
/// pointed at a directory a test made — and so two instances of it could never
/// disagree about where the files are.
#[derive(Clone)]
pub struct Cache {
    dir: PathBuf,
}

impl Cache {
    /// `$XDG_CACHE_HOME/bnkfiles`, falling back to `$HOME/.cache/bnkfiles`.
    /// `None` on a machine with neither, which simply runs without a cache.
    pub fn from_env() -> Option<Self> {
        Some(Self { dir: cache_dir()? })
    }

    /// The cache file for `dir`.
    pub fn path_for(&self, dir: &Path) -> PathBuf {
        self.dir.join(format!("{:016x}.bnkt", hash_path(dir)))
    }

    /// Read the cache for `dir`. An absent, unreadable, or damaged file is simply an
    /// empty cache: every caller's answer to that is to decode the thumbnails afresh.
    pub fn load(&self, dir: &Path) -> Vec<Cached> {
        let path = self.path_for(dir);
        let Ok(meta) = std::fs::metadata(&path) else {
            return Vec::new();
        };
        if !meta.is_file() || meta.len() > MAX_FILE {
            return Vec::new();
        }
        std::fs::read(&path)
            .ok()
            .and_then(|bytes| decode(&bytes))
            .unwrap_or_default()
    }

    /// Write the cache for `dir`, atomically.
    ///
    /// A same-directory temp file renamed over the target, so a crash or a second
    /// instance writing at the same moment leaves either the old cache or the new one
    /// whole, never a half-written file that the next start-up has to detect. Saving
    /// nothing removes the file rather than writing an empty one.
    pub fn save(&self, dir: &Path, entries: &[Cached]) -> std::io::Result<()> {
        let path = self.path_for(dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if entries.is_empty() {
            // Not an error if it was not there to begin with.
            if let Err(e) = std::fs::remove_file(&path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    return Err(e);
                }
            }
            return Ok(());
        }
        let bytes = encode(entries);
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        let staged = (|| {
            use std::io::Write;
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()
        })();
        match staged.and_then(|()| std::fs::rename(&tmp, &path)) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    /// Drop whole directory caches, oldest first, until the cache fits in `budget`.
    ///
    /// By file mtime, which a read does not change and a write does: the least
    /// recently *written* cache goes first. Coarser than tracking use, and enough —
    /// the directories being browsed are the ones being rewritten.
    pub fn trim(&self, budget: u64) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = entries
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("bnkt"))
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                Some((meta.modified().ok()?, meta.len(), e.path()))
            })
            .collect();
        let mut total: u64 = files.iter().map(|(_, len, _)| len).sum();
        if total <= budget {
            return;
        }
        // Oldest first, so the newest caches are the ones that survive.
        files.sort_by_key(|(modified, _, _)| *modified);
        for (_, len, path) in files {
            if total <= budget {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(len);
            }
        }
    }
}

/// Pack an ARGB thumbnail into the cache's payload bytes.
///
/// Raw, not compressed: the only deflate encoder in the tree (`zune-png`'s, here
/// for decoding) emits stored blocks, which grow the payload rather than shrink
/// it, and a real encoder is not worth owning for the disk it would save. Raw
/// also makes the load side free: a cached thumbnail is unpacked straight into
/// pixels with no decoder in the path at all.
pub fn pack_pixels(pixels: &[u32], width: u16, height: u16) -> Option<Vec<u8>> {
    let (w, h) = (width as usize, height as usize);
    let count = w.checked_mul(h)?;
    if count == 0 || pixels.len() < count {
        return None;
    }
    let mut out = Vec::with_capacity(count * 4);
    for pixel in &pixels[..count] {
        out.extend_from_slice(&pixel.to_le_bytes());
    }
    Some(out)
}

/// Unpack payload bytes back into pixels, or `None` if they are not the shape the
/// entry claims.
pub fn unpack_pixels(bytes: &[u8], width: u16, height: u16) -> Option<Vec<u32>> {
    let count = (width as usize).checked_mul(height as usize)?;
    if count == 0 || bytes.len() != count * 4 {
        return None;
    }
    Some(
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&pixel| u32::from_le_bytes(pixel))
            .collect(),
    )
}

/// Writes cache files on a thread of its own, one at a time, in the order asked.
///
/// A save is a copy of every thumbnail in a folder, a write, and an fsync: tens of
/// megabytes and, on a slow disk, a pause long enough to see. None of that
/// belongs between two frames. One thread rather than one per save, so two saves
/// of the same folder cannot stage over each other.
///
/// Dropping the writer waits for what is queued, which is what makes the save on
/// the way out of the app land.
pub struct Writer {
    jobs: Option<mpsc::Sender<(PathBuf, Vec<Cached>)>>,
    thread: Option<JoinHandle<()>>,
}

impl Writer {
    /// Start the writer over `cache`, or `None` if its thread cannot be started,
    /// which leaves the app running without saving thumbnails. `report` says
    /// whether a failed save is worth a line on stderr.
    pub fn start(cache: Cache, report: bool) -> Option<Self> {
        let (jobs, queue) = mpsc::channel::<(PathBuf, Vec<Cached>)>();
        let thread = std::thread::Builder::new()
            .name("thumbcache".into())
            .spawn(move || {
                for (dir, entries) in queue {
                    // A cache that cannot be written is a cache that is not there.
                    if let Err(e) = cache.save(&dir, &entries) {
                        if report {
                            eprintln!(
                                "bnkfiles: could not save thumbnails for {}: {e}",
                                dir.display()
                            );
                        }
                    }
                    cache.trim(BUDGET);
                }
            })
            .ok()?;
        Some(Self {
            jobs: Some(jobs),
            thread: Some(thread),
        })
    }

    /// Queue `entries` as the whole cache for `dir`, replacing what was there.
    /// Handed nothing, the file is removed.
    pub fn save(&self, dir: PathBuf, entries: Vec<Cached>) {
        if let Some(jobs) = &self.jobs {
            // A send only fails once the thread is gone, and then there is
            // nobody to write it.
            let _ = jobs.send((dir, entries));
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // Closing the channel ends the thread's loop once the queue is drained.
        self.jobs = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// `$XDG_CACHE_HOME/bnkfiles`, falling back to `$HOME/.cache/bnkfiles`.
fn cache_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from) {
        Some(path) if path.is_absolute() => path,
        _ => {
            let mut home = PathBuf::from(std::env::var_os("HOME")?);
            home.push(".cache");
            home
        }
    };
    Some(base.join("bnkfiles"))
}

/// FNV-1a over the path's bytes. Not a cryptographic hash and does not need to
/// be: a collision means two directories share a cache file, and the per-entry
/// fingerprints then reject every thumbnail in it, so the cost of the birthday
/// case is a rebuild.
fn hash_path(path: &Path) -> u64 {
    use std::os::unix::ffi::OsStrExt;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.as_os_str().as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use crate::files::FsKind;
    use crate::thumbcache::*;

    /// A cache entry whose payload is `w*h` grey pixels, so its length agrees
    /// with its dimensions the way the format requires.
    fn cached(name: &str, w: u16, h: u16) -> Cached {
        Cached {
            name: name.into(),
            mtime_secs: 1_700_000_000,
            size: 4096,
            edge: 160,
            width: w,
            height: h,
            pixels: pack_pixels(&vec![0xff80_8080; w as usize * h as usize], w, h).expect("pack"),
        }
    }

    fn entry(name: &str, mtime_secs: i64, size: u64) -> FsEntry {
        FsEntry {
            name: name.to_string(),
            path: PathBuf::from(name),
            kind: FsKind::File,
            size,
            mtime_secs,
            hidden: false,
        }
    }

    #[test]
    fn a_cache_round_trips_through_its_own_format() {
        let entries = vec![
            cached("a.png", 8, 6),
            cached("b with spaces.jpg", 4, 4),
            // A name that is not ASCII, and an empty payload.
            cached("caf\u{e9}.png", 2, 2),
        ];
        let bytes = encode(&entries);
        assert_eq!(decode(&bytes).as_deref(), Some(entries.as_slice()));
    }

    #[test]
    fn a_name_that_is_not_utf8_keeps_its_own_thumbnail() {
        // Two names that display alike. Each must come back as itself and answer
        // only for its own file, or one picture's thumbnail is drawn on the other.
        let name = |bytes: &[u8]| OsString::from_vec(bytes.to_vec());
        let (one, other) = (name(b"a\xff.png"), name(b"a\xfe.png"));
        let mut saved = cached("placeholder", 2, 2);
        saved.name = one.clone();
        let back = decode(&encode(std::slice::from_ref(&saved))).expect("decodes");
        assert_eq!(back, vec![saved.clone()]);

        let file = |name: &OsString| FsEntry {
            path: PathBuf::from(name),
            ..entry(&name.to_string_lossy(), saved.mtime_secs, saved.size)
        };
        assert!(back[0].matches(&file(&one), saved.edge));
        assert!(!back[0].matches(&file(&other), saved.edge));
    }

    #[test]
    fn an_empty_cache_round_trips_too() {
        let bytes = encode(&[]);
        assert_eq!(decode(&bytes), Some(Vec::new()));
    }

    #[test]
    fn truncation_at_every_offset_is_refused_rather_than_half_read() {
        // The torn-write case, exhaustively: a crash mid-write can leave the file
        // cut at any byte, and not one of those may parse as a shorter valid
        // cache. Salvaging part of a file whose length fields no longer agree is
        // how a cache turns into a crash.
        let entries = vec![cached("a.png", 5, 4), cached("b.png", 3, 3)];
        let bytes = encode(&entries);
        for cut in 0..bytes.len() {
            assert_eq!(
                decode(&bytes[..cut]),
                None,
                "a file truncated to {cut} bytes parsed as valid"
            );
        }
        // Only the whole thing reads back.
        assert!(decode(&bytes).is_some());
    }

    #[test]
    fn trailing_bytes_are_refused() {
        // Extra bytes mean the header's count and the body disagree, which means
        // the file is not what it says it is.
        let mut bytes = encode(&[cached("a.png", 2, 2)]);
        bytes.push(0);
        assert_eq!(decode(&bytes), None);
    }

    #[test]
    fn a_foreign_or_future_file_is_not_read() {
        let good = encode(&[cached("a.png", 2, 2)]);
        // Wrong magic: not ours.
        let mut wrong = good.clone();
        wrong[0] = b'X';
        assert_eq!(decode(&wrong), None);
        // A version this build does not know: discarded, not guessed at.
        let mut future = good.clone();
        future[4..8].copy_from_slice(&(VERSION + 1).to_le_bytes());
        assert_eq!(decode(&future), None);
        // Nothing at all, and less than a header.
        assert_eq!(decode(b""), None);
        assert_eq!(decode(b"BNKT"), None);
    }

    #[test]
    fn a_corrupt_length_cannot_read_past_the_end() {
        // The bug this whole parse is shaped to prevent: a length field that says
        // more than the file holds must fail, not panic and not read adjacent
        // memory.
        let entries = vec![cached("a.png", 4, 4)];
        let bytes = encode(&entries);
        for offset in 0..bytes.len().min(64) {
            let mut broken = bytes.clone();
            broken[offset] = 0xff;
            // Whatever this does, it does not panic; most such files are refused,
            // and any that still parse are internally consistent.
            let _ = decode(&broken);
        }
        // A payload length past the ceiling is refused outright. The length field
        // is the four bytes immediately before the payload itself.
        let mut huge = bytes.clone();
        let at = bytes.len() - entries[0].pixels.len() - 4;
        huge[at..at + 4].copy_from_slice(&(MAX_PAYLOAD as u32 + 1).to_le_bytes());
        assert_eq!(decode(&huge), None);
    }

    #[test]
    fn an_entry_only_matches_the_file_it_was_made_from() {
        let c = cached("photo.png", 2, 2);
        assert!(c.matches(&entry("photo.png", 1_700_000_000, 4096), 160));
        // Any part of the fingerprint changing is a miss: an edited photo must
        // show its new contents, not a cached picture of the old ones.
        assert!(!c.matches(&entry("photo.png", 1_700_000_001, 4096), 160));
        assert!(!c.matches(&entry("photo.png", 1_700_000_000, 4097), 160));
        assert!(!c.matches(&entry("other.png", 1_700_000_000, 4096), 160));
        // And a different cell size is a miss too: blowing a 160px thumbnail up
        // to 240 looks worse than decoding afresh.
        assert!(!c.matches(&entry("photo.png", 1_700_000_000, 4096), 240));
    }

    /// A cache of this test's own, so tests running side by side never share a
    /// directory and nothing here touches the real one.
    fn temp_cache(tag: &str) -> (PathBuf, Cache) {
        let dir = std::env::temp_dir().join(format!(
            "bnkfiles_thumbcache_{tag}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("cache dir");
        (dir.clone(), Cache { dir })
    }

    #[test]
    fn saving_and_loading_a_directorys_cache_round_trips_on_disk() {
        let (dir, cache) = temp_cache("roundtrip");
        let folder = Path::new("/some/photo/folder");
        let entries = vec![cached("a.png", 4, 4), cached("b.png", 6, 5)];

        assert!(cache.load(folder).is_empty(), "nothing cached yet");
        cache.save(folder, &entries).expect("save");
        assert_eq!(cache.load(folder), entries);

        // Two directories get two files, and neither reads the other's.
        let other = Path::new("/some/other/folder");
        assert!(cache.load(other).is_empty());
        assert_ne!(cache.path_for(folder), cache.path_for(other));

        // Saving nothing removes the file rather than leaving an empty one.
        cache.save(folder, &[]).expect("save empty");
        assert!(cache.load(folder).is_empty());
        assert!(!cache.path_for(folder).exists());

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_damaged_cache_file_loads_as_empty_rather_than_failing() {
        let (dir, cache) = temp_cache("damaged");
        let folder = Path::new("/a/folder");
        cache.save(folder, &[cached("a.png", 4, 4)]).expect("save");

        // Something else wrote nonsense over it.
        let path = cache.path_for(folder);
        std::fs::write(&path, b"not a bnkt file at all").expect("clobber");
        assert!(cache.load(folder).is_empty());

        // Truncated in place.
        let good = encode(&[cached("a.png", 4, 4)]);
        std::fs::write(&path, &good[..good.len() / 2]).expect("truncate");
        assert!(cache.load(folder).is_empty());

        // A directory where the file should be.
        std::fs::remove_file(&path).expect("remove");
        std::fs::create_dir_all(&path).expect("mkdir");
        assert!(cache.load(folder).is_empty());

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_missing_cache_directory_is_survivable() {
        // `rm -rf ~/.cache/bnkfiles` while the app is running: loading finds
        // nothing, saving recreates the directory, and neither is an error.
        let (dir, cache) = temp_cache("removed");
        let folder = Path::new("/a/folder");
        cache.save(folder, &[cached("a.png", 2, 2)]).expect("save");
        std::fs::remove_dir_all(&dir).expect("rm -rf the cache");

        assert!(cache.load(folder).is_empty());
        cache
            .save(folder, &[cached("a.png", 2, 2)])
            .expect("save recreates the directory");
        assert_eq!(cache.load(folder).len(), 1);

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn the_budget_drops_the_oldest_caches_first() {
        let (dir, cache) = temp_cache("budget");
        // Three directories' caches, each around 4 KB.
        let folders = [
            Path::new("/folder/one"),
            Path::new("/folder/two"),
            Path::new("/folder/three"),
        ];
        for folder in &folders {
            cache
                .save(folder, &[cached("a.png", 32, 32)])
                .expect("save");
            // Space the mtimes so "oldest" is unambiguous on a coarse clock.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(folders.iter().all(|f| !cache.load(f).is_empty()));

        // A budget with room for about two of them.
        cache.trim(9 * 1024);
        assert!(
            cache.load(folders[0]).is_empty(),
            "the oldest cache should have gone first"
        );
        assert!(
            !cache.load(folders[2]).is_empty(),
            "the newest cache should have survived"
        );

        // A budget of nothing empties it, and doing it twice is harmless.
        cache.trim(0);
        assert!(folders.iter().all(|f| cache.load(f).is_empty()));
        cache.trim(0);

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    /// A thumbnail-shaped image: gradients carrying block detail, varied enough
    /// that a round trip that drops or reorders pixels shows.
    fn synth_thumb(w: u16, h: u16) -> Vec<u32> {
        let mut pixels = Vec::with_capacity(w as usize * h as usize);
        for y in 0..h as u32 {
            for x in 0..w as u32 {
                // Detail at a scale of a few pixels, hashed so it is not a simple
                // ramp, but constant within each little block.
                let block = (x / 3).wrapping_mul(2_654_435_761) ^ (y / 3).wrapping_mul(40_503);
                let detail = (block >> 27) & 0x1f;
                let r = (((x * 255) / w as u32) + detail) & 0xff;
                let g = (((y * 255) / h as u32) + detail) & 0xff;
                let b = ((x + y) * 128 / (w as u32 + h as u32)) & 0xff;
                pixels.push(0xff00_0000 | (r << 16) | (g << 8) | b);
            }
        }
        pixels
    }

    #[test]
    fn a_thumbnail_survives_the_round_trip_through_the_payload() {
        let (w, h) = (160u16, 120u16);
        let pixels = synth_thumb(w, h);
        let packed = pack_pixels(&pixels, w, h).expect("pack");
        assert_eq!(packed.len(), w as usize * h as usize * 4);
        assert_eq!(unpack_pixels(&packed, w, h).as_deref(), Some(&pixels[..]));
    }

    #[test]
    fn unpacking_refuses_bytes_that_are_not_the_shape_they_claim() {
        // The payload's length and the dimensions beside it come from the same
        // file, and a corrupt one can disagree. Reshaping the pixels to fit would
        // draw garbage; refusing them redecodes the thumbnail.
        let packed = pack_pixels(&synth_thumb(8, 8), 8, 8).expect("pack");
        assert!(unpack_pixels(&packed, 8, 8).is_some());
        assert!(unpack_pixels(&packed, 8, 9).is_none(), "too few bytes");
        assert!(unpack_pixels(&packed, 4, 8).is_none(), "too many bytes");
        assert!(unpack_pixels(&packed, 0, 0).is_none());
        assert!(unpack_pixels(&[], 1, 1).is_none());
        // And a size whose product would overflow is refused, not wrapped.
        assert!(unpack_pixels(&packed, u16::MAX, u16::MAX).is_none());
    }

    #[test]
    fn packing_refuses_a_size_that_does_not_match_the_pixels() {
        // A short buffer must not be read past, whatever the caller claims.
        assert!(pack_pixels(&[0; 4], 100, 100).is_none());
        assert!(pack_pixels(&[], 0, 0).is_none());
        assert!(pack_pixels(&[0; 16], 4, 0).is_none());
        // Exactly enough is fine.
        assert!(pack_pixels(&[0xff00_0000; 16], 4, 4).is_some());
    }

    #[test]
    fn the_hash_is_stable_and_separates_directories() {
        // The same directory must find its cache again next time the app runs.
        assert_eq!(hash_path(Path::new("/a/b")), hash_path(Path::new("/a/b")));
        assert_ne!(hash_path(Path::new("/a/b")), hash_path(Path::new("/a/c")));
        // Including the cases a naive concatenation would confuse.
        assert_ne!(hash_path(Path::new("/ab/c")), hash_path(Path::new("/a/bc")));
    }

    /// A folder's cache grows with every visit. Past the reader's ceiling the
    /// whole file is refused, so the writer has to stop short of it.
    #[test]
    fn a_cache_cut_to_its_budget_keeps_the_leading_entries_and_still_loads() {
        let mut entries: Vec<Cached> = (0..10)
            .map(|i| cached(&format!("{i}.jpg"), 16, 16))
            .collect();
        let one = entries[0].encoded_len();
        fit(&mut entries, HEADER + 3 * one + one / 2);
        let names: Vec<&std::ffi::OsStr> = entries.iter().map(|e| e.name.as_os_str()).collect();
        assert_eq!(names, ["0.jpg", "1.jpg", "2.jpg"]);
        assert!(encode(&entries).len() <= HEADER + 3 * one + one / 2);
        assert_eq!(decode(&encode(&entries)), Some(entries));

        const { assert!(DIR_BUDGET as u64 <= MAX_FILE) };
    }

    #[test]
    fn the_writer_lands_every_queued_save_before_it_is_dropped() {
        let (dir, cache) = temp_cache("writer");
        let folder = PathBuf::from("/photos/2024");
        let writer = Writer::start(cache.clone(), false).expect("a thread");
        writer.save(folder.clone(), vec![cached("old.jpg", 8, 8)]);
        writer.save(folder.clone(), vec![cached("new.jpg", 8, 8)]);
        drop(writer);

        let loaded = cache.load(&folder);
        std::fs::remove_dir_all(&dir).expect("clean up");
        assert_eq!(
            loaded,
            vec![cached("new.jpg", 8, 8)],
            "saves apply in order"
        );
    }
}
