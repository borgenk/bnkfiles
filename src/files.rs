//! The filesystem core: what a directory holds, how it is ordered, and what the
//! views take off it.
//!
//! A directory is read into [`FsEntry`] rows — every file type, not only the
//! decodable ones — ordered by an [`Order`]: directories first, then by the
//! column chosen, names in the order a person reads them. The reading happens on a worker thread
//! ([`crate::scan`]), which streams sorted batches the main thread folds together
//! with [`merge_sorted`]; this module owns the model, the ordering, and the pure
//! projections, so all of that is testable without a window or a thread.
//!
//! The views take projections off one listing rather than each reading the
//! directory for themselves, so no two of them can disagree about what a folder
//! holds. A projection runs every time a scan batch lands, so it hands back
//! indices where it can: copying the names and paths out of a 100k-entry listing
//! that often is the obvious way to make a big directory crawl.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use crate::mime;
use crate::order::Order;
use crate::platform::error::{Error, Result};

/// Whether `path`'s name says it is an image the viewer can decode.
///
/// By name, not by content: this is asked once per entry of every listing, and
/// sniffing a 100k-entry directory would mean opening every file in it. The
/// extension table lives in [`crate::mime`], so the decoders' formats are written
/// down once rather than in two lists that drift apart.
pub fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .and_then(mime::from_extension)
        .is_some_and(mime::is_decodable_image)
}

/// What a listing entry is. Dir-ness follows symlinks, so a symlinked folder is a
/// folder; a broken link has no target to ask and is described as the plain file it
/// looks like from the directory.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FsKind {
    Dir,
    File,
}

/// One entry in a directory listing. The model keeps everything — hidden entries,
/// files of every type — and the views filter; a projection can hide something the
/// model dropped, but it cannot show something the model never read.
#[derive(Clone, Debug)]
pub struct FsEntry {
    /// The file name, lossily, for ordering and display.
    pub name: String,
    pub path: PathBuf,
    pub kind: FsKind,
    /// Read by the detail list's columns and by the thumbnail cache's per-entry
    /// fingerprint. Filled here because it comes free with the one `metadata` call
    /// `describe` already makes: asking the filesystem a second time later, per
    /// entry, is what this model exists to avoid.
    pub size: u64,
    /// Modification time in unix seconds, negative before the epoch. Zero when the
    /// filesystem would not say.
    pub mtime_secs: i64,
    /// Dot-prefixed. Kept in the model, filtered by the views that hide them.
    pub hidden: bool,
}

impl FsEntry {
    pub fn is_dir(&self) -> bool {
        self.kind == FsKind::Dir
    }
}

/// Describe one directory entry: a single `metadata` call, made on the scan worker.
///
/// An entry that vanishes between the directory read and the stat still gets a row,
/// with the sizes it would not answer for left at zero: the alternative is a listing
/// that silently drops whatever was being deleted while it ran. A live directory
/// corrects itself on the next scan.
pub fn describe(entry: &std::fs::DirEntry) -> FsEntry {
    let path = entry.path();
    let name = file_name(&path);
    let meta = std::fs::metadata(&path).ok();
    FsEntry {
        kind: match meta.as_ref() {
            Some(m) if m.is_dir() => FsKind::Dir,
            _ => FsKind::File,
        },
        size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
        mtime_secs: meta.as_ref().and_then(mtime_secs).unwrap_or(0),
        hidden: name.starts_with('.'),
        name,
        path,
    }
}

/// A file's modification time as unix seconds. `SystemTime` has no infallible
/// conversion — a file dated before 1970 is unusual but not an error — so a time
/// the clock puts before the epoch comes back negative rather than panicking or
/// being thrown away.
fn mtime_secs(meta: &std::fs::Metadata) -> Option<i64> {
    let modified = meta.modified().ok()?;
    Some(match modified.duration_since(std::time::UNIX_EPOCH) {
        Ok(since) => since.as_secs().min(i64::MAX as u64) as i64,
        Err(before) => -(before.duration().as_secs().min(i64::MAX as u64) as i64),
    })
}

/// Fold a sorted batch into a sorted listing, in one linear pass.
///
/// The scan streams batches sorted within themselves, and the listing has to stay
/// sorted the whole time (the explorer draws it while it fills). Re-sorting the
/// whole listing per batch would be O(n log n) per batch over a listing that only
/// grows; merging two sorted runs is one pass. Ties keep the entry already in the
/// listing, so the merge is stable. `order` is the one the batches were sorted in.
pub fn merge_sorted(listing: &mut Vec<FsEntry>, batch: Vec<FsEntry>, order: Order) {
    if batch.is_empty() {
        return;
    }
    if listing.is_empty() {
        *listing = batch;
        return;
    }
    let mut merged = Vec::with_capacity(listing.len() + batch.len());
    let mut old = std::mem::take(listing).into_iter().peekable();
    let mut new = batch.into_iter().peekable();
    loop {
        let take_new = match (old.peek(), new.peek()) {
            (None, None) => break,
            (Some(_), None) => false,
            (None, Some(_)) => true,
            (Some(a), Some(b)) => order.cmp(b, a) == Ordering::Less,
        };
        let next = if take_new { new.next() } else { old.next() };
        if let Some(entry) = next {
            merged.push(entry);
        }
    }
    *listing = merged;
}

/// The gallery's projection: every decodable image in the listing that `shown`
/// lets by, in listing order (files follow directories, so the images come out
/// in natural order).
///
/// `shown` is the same test the explorer's rows are projected by (hidden
/// entries, the filter), so the two always agree about what is in a folder:
/// filtered apart, the viewer would page through pictures the grid behind it
/// does not show.
pub fn images_of(listing: &[FsEntry], shown: impl Fn(&FsEntry) -> bool) -> Vec<PathBuf> {
    listing
        .iter()
        .filter(|e| shown(e) && !e.is_dir() && is_image(&e.path))
        .map(|e| e.path.clone())
        .collect()
}

/// Put `path` in its place in an image list ordered like a listing's, unless it is
/// already there.
///
/// The one caller is the gallery adopting a finished scan: an image opened by name
/// whose extension the scan does not list (`bnkfiles diagram.xyz`) is not in the
/// projection, and dropping it would take the picture out from under the viewer
/// showing it. It is placed by name whatever the listing's order: it has no
/// entry to sort by size or date, and an odd place in the paging order beats
/// no place at all.
pub fn keep_image(images: &mut Vec<PathBuf>, path: &Path) {
    if images.iter().any(|p| p == path) {
        return;
    }
    let name = file_name(path);
    let slot = images.partition_point(|p| natural_cmp(&file_name(p), &name) == Ordering::Less);
    images.insert(slot, path.to_path_buf());
}

/// A path's file name, lossily, for ordering and display. A non-UTF-8 name is not
/// a reason to hide a picture, and every caller here wants the same answer, so the
/// lossy conversion happens once, here.
pub fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

/// The folder directly inside `above` on the way down to `below`: `below`
/// itself when it is one level down, `None` when it is not under `above` at
/// all. Where the cursor lands on the way up, so that climbing out of a tree
/// lands on the branch just left, even one that is not there any more.
pub fn child_toward(above: &Path, below: &Path) -> Option<PathBuf> {
    let first = below.strip_prefix(above).ok()?.components().next()?;
    Some(above.join(first))
}

/// Compare two names the way a person reads them: digit runs compare as numbers,
/// so `img2` sorts before `img10`, and the rest compares case-insensitively.
///
/// A byte-lexicographic sort is what a folder of photos most visibly does not
/// want: `IMG_10` before `IMG_9`, and `Zebra` before `apple`.
///
/// Allocation-free, because this is the innermost function of every listing: a
/// 100k-entry directory runs it a few million times across the batch sorts and
/// the merges.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a, b);
    loop {
        let (ca, cb) = match (x.chars().next(), y.chars().next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ca), Some(cb)) => (ca, cb),
        };
        if ca.is_ascii_digit() && cb.is_ascii_digit() {
            // Two digit runs: compare them as numbers, by length then digits, which
            // is the same answer parsing them would give without the overflow a
            // 40-digit file name would bring. Leading zeros only break the tie
            // (`01` after `1`), so the order stays total.
            let (na, nb) = (digit_run(x), digit_run(y));
            x = &x[na.len()..];
            y = &y[nb.len()..];
            let (va, vb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
            let order = va
                .len()
                .cmp(&vb.len())
                .then_with(|| va.cmp(vb))
                .then_with(|| na.len().cmp(&nb.len()));
            if order != Ordering::Equal {
                return order;
            }
        } else {
            let order = ca
                .to_ascii_lowercase()
                .cmp(&cb.to_ascii_lowercase())
                .then_with(|| ca.cmp(&cb));
            if order != Ordering::Equal {
                return order;
            }
            x = &x[ca.len_utf8()..];
            y = &y[cb.len_utf8()..];
        }
    }
}

/// The run of digits at the front of `s`, borrowed from it. Digits are ASCII, so a
/// byte scan lands on a character boundary.
fn digit_run(s: &str) -> &str {
    let end = s
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(s.len());
    &s[..end]
}

/// What a command-line path turns out to be.
#[derive(Debug)]
pub enum Opening {
    Dir(PathBuf),
    File(PathBuf),
}

/// Resolve the path the command line named, before anything else happens.
///
/// This runs before the Wayland connection, so a path that is not there ends the
/// process with one clear line about the path the user actually typed.
pub fn resolve_arg(path: &Path) -> Result<Opening> {
    // metadata, not symlink_metadata: a symlink to a directory opens as the
    // directory, the same way the listing treats one.
    let meta = std::fs::metadata(path)
        .map_err(|e| Error::msg(format!("cannot open {}: {e}", path.display())))?;
    // Absolute from here on. The path is compared against what a scan of its
    // folder yields, walked up to its parents, and remembered for the next launch,
    // none of which survive a path that only means something from this cwd.
    // Lexical, so a symlinked folder keeps the name the user reached it by.
    let path = std::path::absolute(path)
        .map_err(|e| Error::msg(format!("cannot open {}: {e}", path.display())))?;
    Ok(if meta.is_dir() {
        Opening::Dir(path)
    } else {
        Opening::File(path)
    })
}

#[cfg(test)]
mod tests {
    use crate::files::*;
    use std::fs;

    /// A unique temp directory for one test, removed by `cleanup`.
    fn temp_dir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "bnkfiles_{tag}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).expect("make temp dir");
        base
    }

    /// Read a directory into a sorted listing the way the scan worker does, but on
    /// this thread: the reference the streaming scanner is checked against.
    fn list_sorted(dir: &Path) -> Vec<FsEntry> {
        let mut entries: Vec<FsEntry> = fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| describe(&e))
            .collect();
        entries.sort_by(|a, b| Order::default().cmp(a, b));
        entries
    }

    fn entry(name: &str, kind: FsKind) -> FsEntry {
        FsEntry {
            name: name.to_string(),
            path: PathBuf::from(name),
            kind,
            size: 0,
            mtime_secs: 0,
            hidden: name.starts_with('.'),
        }
    }

    fn names(listing: &[FsEntry]) -> Vec<&str> {
        listing.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn directories_lead_and_names_read_naturally() {
        let mut listing = vec![
            entry("IMG_10.png", FsKind::File),
            entry("Zebra.png", FsKind::File),
            entry("IMG_9.png", FsKind::File),
            entry("zsub", FsKind::Dir),
            entry("apple.png", FsKind::File),
            entry("Alpha", FsKind::Dir),
            entry("IMG_2.png", FsKind::File),
        ];
        listing.sort_by(|a, b| Order::default().cmp(a, b));
        assert_eq!(
            names(&listing),
            vec![
                // Directories first, in natural order among themselves.
                "Alpha",
                "zsub",
                // Then the files: 9 before 10 (digits compare as numbers), and
                // apple before Zebra (case does not split the alphabet in two).
                "apple.png",
                "IMG_2.png",
                "IMG_9.png",
                "IMG_10.png",
                "Zebra.png",
            ]
        );
    }

    #[test]
    fn natural_order_is_total_on_leading_zeros() {
        // Equal values: the shorter (fewer leading zeros) sorts first, so no two
        // distinct names ever compare Equal and the sort stays deterministic.
        assert_eq!(natural_cmp("a1", "a01"), Ordering::Less);
        assert_eq!(natural_cmp("a01", "a1"), Ordering::Greater);
        assert_eq!(natural_cmp("a1", "a1"), Ordering::Equal);
        assert_eq!(natural_cmp("a2", "a10"), Ordering::Less);
    }

    #[test]
    fn merging_batches_in_any_order_gives_one_sorted_listing() {
        // The scan hands over sorted batches, but the batches themselves arrive in
        // whatever order the directory read produced. Whatever that order, the
        // listing they fold into is the fully sorted one.
        let all = || {
            vec![
                entry("b_dir", FsKind::Dir),
                entry("a_dir", FsKind::Dir),
                entry("c.png", FsKind::File),
                entry("a.png", FsKind::File),
                entry("b.png", FsKind::File),
                entry("d.png", FsKind::File),
            ]
        };
        let mut want = all();
        want.sort_by(|a, b| Order::default().cmp(a, b));

        for chunk in [1usize, 2, 3, 6] {
            let mut listing: Vec<FsEntry> = Vec::new();
            for batch in all().chunks(chunk) {
                let mut batch = batch.to_vec();
                batch.sort_by(|a, b| Order::default().cmp(a, b));
                merge_sorted(&mut listing, batch, Order::default());
            }
            assert_eq!(names(&listing), names(&want), "in batches of {chunk}");
        }
    }

    #[test]
    fn merging_an_empty_batch_leaves_the_listing_alone() {
        let mut listing = vec![entry("a.png", FsKind::File)];
        merge_sorted(&mut listing, Vec::new(), Order::default());
        assert_eq!(names(&listing), vec!["a.png"]);
        // And an empty listing simply adopts the batch.
        let mut listing: Vec<FsEntry> = Vec::new();
        merge_sorted(
            &mut listing,
            vec![entry("a.png", FsKind::File)],
            Order::default(),
        );
        assert_eq!(names(&listing), vec!["a.png"]);
    }

    #[test]
    fn a_listing_describes_every_kind_of_entry() {
        let base = temp_dir("describe");
        fs::create_dir_all(base.join("sub")).expect("subdir");
        fs::write(base.join("notes.txt"), b"hello").expect("file");
        fs::write(base.join(".hidden"), b"x").expect("dotfile");

        let listing = list_sorted(&base);
        assert_eq!(names(&listing), vec!["sub", ".hidden", "notes.txt"]);
        assert!(listing[0].is_dir());
        // The model keeps the dotfile; the views are what hide it.
        assert!(listing[1].hidden);
        assert!(!listing[2].hidden);
        assert_eq!(listing[2].size, 5);
        // A real file has a real timestamp, not the defensive zero.
        assert!(listing[2].mtime_secs > 0);

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_directory_named_like_an_image_is_still_a_directory() {
        // A folder called `holiday.png` passes the extension test; only asking the
        // filesystem what it is keeps it out of the gallery (where it would sit as
        // a permanently broken thumbnail).
        let base = temp_dir("looks_like_image");
        fs::create_dir_all(base.join("holiday.png")).expect("dir");
        fs::write(base.join("real.png"), b"x").expect("file");

        let listing = list_sorted(&base);
        let images = images_of(&listing, |e| !e.hidden);
        assert_eq!(images.len(), 1);
        assert_eq!(file_name(&images[0]), "real.png");

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn the_gallery_and_the_explorer_agree_about_hidden_pictures() {
        let base = temp_dir("hidden");
        fs::create_dir_all(base.join("zsub")).expect("subdir");
        fs::write(base.join("b.png"), b"x").expect("b");
        fs::write(base.join("a.jpg"), b"x").expect("a");
        fs::write(base.join("notes.txt"), b"x").expect("txt");
        fs::write(base.join(".hidden.png"), b"x").expect("hidden");

        let listing = list_sorted(&base);
        // Hidden off: the dot-file is in the listing but not in the gallery.
        let shown: Vec<String> = images_of(&listing, |e| !e.hidden)
            .iter()
            .map(|p| file_name(p))
            .collect();
        assert_eq!(shown, vec!["a.jpg", "b.png"]);
        // Hidden on: it joins, in name order rather than tacked on an end.
        let all: Vec<String> = images_of(&listing, |_| true)
            .iter()
            .map(|p| file_name(p))
            .collect();
        assert_eq!(all, vec![".hidden.png", "a.jpg", "b.png"]);
        // Directories and undecodable files are never pictures either way.
        assert!(!all.iter().any(|n| n == "zsub" || n == "notes.txt"));

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn an_unscanned_image_keeps_its_place_in_the_list() {
        let mut images = vec![
            PathBuf::from("/pics/a.png"),
            PathBuf::from("/pics/m.png"),
            PathBuf::from("/pics/z.png"),
        ];
        // A file the gallery projection did not list lands in name order, not at
        // an end: the arrows keep walking the folder in the order it reads.
        keep_image(&mut images, Path::new("/pics/n.xyz"));
        let names: Vec<String> = images.iter().map(|p| file_name(p)).collect();
        assert_eq!(names, vec!["a.png", "m.png", "n.xyz", "z.png"]);

        // Already present: no duplicate, wherever it sits.
        keep_image(&mut images, Path::new("/pics/n.xyz"));
        keep_image(&mut images, Path::new("/pics/a.png"));
        assert_eq!(images.len(), 4);
    }

    #[test]
    fn resolve_arg_tells_a_directory_from_a_file_and_refuses_neither() {
        let base = temp_dir("resolve");
        fs::write(base.join("photo.png"), b"x").expect("file");

        assert!(matches!(
            resolve_arg(&base).expect("the directory resolves"),
            Opening::Dir(_)
        ));
        assert!(matches!(
            resolve_arg(&base.join("photo.png")).expect("the file resolves"),
            Opening::File(_)
        ));
        // A path that is not there is an error here, where it can still be
        // reported against the path the user typed.
        let missing = base.join("nope.png");
        let err = resolve_arg(&missing).expect_err("a missing path fails");
        assert!(
            err.to_string().contains("nope.png"),
            "the error names the path: {err}"
        );

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn the_way_down_from_a_folder_starts_at_one_of_its_children() {
        let p = Path::new;
        assert_eq!(
            child_toward(p("/a"), p("/a/b")),
            Some(PathBuf::from("/a/b"))
        );
        assert_eq!(
            child_toward(p("/a"), p("/a/b/c/d")),
            Some(PathBuf::from("/a/b"))
        );
        assert_eq!(child_toward(p("/"), p("/a/b")), Some(PathBuf::from("/a")));
        assert_eq!(child_toward(p("/a"), p("/a")), None, "not below itself");
        assert_eq!(
            child_toward(p("/a"), p("/ab/c")),
            None,
            "by component, not text"
        );
    }
}
