//! Deleting to the XDG trash, per the freedesktop Desktop Trash Can Specification.
//!
//! Delete means trash: a mutation is reversible wherever the platform allows,
//! and the platform allows it here. A trashed file keeps its name, its
//! contents, and a note saying where it came from, so any file manager on the
//! machine — this one or another — can put it back.
//!
//! ```text
//!   $XDG_DATA_HOME/Trash/
//!     files/holiday.jpg        the file itself, moved (never copied)
//!     info/holiday.jpg.trashinfo
//!         [Trash Info]
//!         Path=/home/me/pictures/holiday.jpg
//!         DeletionDate=2026-07-18T14:22:05
//! ```
//!
//! Two rules from the spec shape the implementation more than the rest:
//!
//! - **The info file is written first, and atomically.** It is created with
//!   `O_EXCL`, which reserves the name against another process trashing a file
//!   called the same thing at the same moment. Checking whether a name is free and
//!   then taking it is the race the spec exists to rule out. If the move that
//!   follows fails, the reservation is released again, or a failed trash would
//!   burn the name forever and leave a note about a file that is not there.
//! - **A file is moved, never copied.** So a file on another filesystem cannot go
//!   into the home trash: it goes into a trash directory on its own mount
//!   (`$topdir/.Trash/$uid` if an administrator made one and it passes its checks,
//!   otherwise `$topdir/.Trash-$uid`), and its recorded path is relative to that
//!   mount rather than absolute.
//!
//! The trash root and the uid are held rather than read from the environment at
//! the point of use, so the whole thing can be exercised against a temp directory
//! instead of against whatever this machine's home trash happens to contain.

use std::ffi::{c_uint, OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::jobs::sys;
use crate::localtime;

/// The sticky bit. On a shared `$topdir/.Trash` it is what stops one user from
/// removing another's trashed files, and the spec makes checking for it mandatory.
const S_ISVTX: u32 = 0o1000;

/// Trash directories are the owner's business and nobody else's.
const TRASH_MODE: u32 = 0o700;

/// Where trashed files go, and who is doing the trashing.
pub struct Trash {
    /// The home trash: `$XDG_DATA_HOME/Trash`.
    home: PathBuf,
    uid: u32,
}

/// The entry being trashed, pinned to where it really is.
///
/// Names stay as the bytes the filesystem has them in, start to finish. A name
/// that is not UTF-8 is still a file somebody wants back, and converting it for
/// convenience would trash it under a different name and record a path that never
/// existed.
struct Located {
    /// The directory holding the entry, absolute and with every symlink resolved.
    parent: PathBuf,
    name: OsString,
    /// `parent` joined with `name`: the path to move and to record.
    path: PathBuf,
}

impl Trash {
    /// The trash for the user running this process, or `None` on a machine with
    /// neither `$XDG_DATA_HOME` nor `$HOME` (where there is nowhere to put it).
    pub fn for_user() -> Option<Self> {
        let base = match std::env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
            Some(path) if path.is_absolute() => path,
            _ => {
                let mut home = PathBuf::from(std::env::var_os("HOME")?);
                home.push(".local/share");
                home
            }
        };
        // SAFETY: getuid cannot fail and touches no memory we own.
        let uid = unsafe { getuid() };
        Some(Self {
            home: base.join("Trash"),
            uid,
        })
    }

    /// A trash rooted somewhere specific.
    ///
    /// Test-only, and deliberately so: the running program has exactly one
    /// trash and finds it from the environment, while a test must never write
    /// into the one belonging to whoever is running it.
    #[cfg(test)]
    pub fn at(home: PathBuf, uid: u32) -> Self {
        Self { home, uid }
    }

    /// Move `path` to the trash, returning where it ended up.
    ///
    /// `now_secs` is the deletion time as a unix timestamp, passed in rather than
    /// read here so the recorded date is pinned by tests rather than by the clock.
    pub fn put(&self, path: &Path, now_secs: i64) -> io::Result<PathBuf> {
        let item = locate(path)?;
        let (dir, recorded) = self.destination(&item)?;
        fs::create_dir_all(dir.join("files"))?;
        fs::create_dir_all(dir.join("info"))?;
        let contents = info_contents(&recorded, now_secs);

        // Reserve a name, then fill in the note and move the file under it.
        for n in 1..u32::MAX {
            let candidate = if n == 1 {
                item.name.clone()
            } else {
                disambiguate(&item.name, n)
            };
            let Some(info_path) = reserve(&dir, &candidate)? else {
                continue;
            };
            let target = dir.join("files").join(&candidate);
            // The move refuses to replace. A name free in `info/` says nothing
            // about `files/`: an entry can sit there with no note (another
            // program's crash, a note deleted by hand), and renaming onto it
            // would destroy it with no way back.
            let moved = fs::write(&info_path, &contents)
                .and_then(|()| sys::rename_noreplace(&item.path, &target));
            match moved {
                Ok(()) => return Ok(target),
                Err(e) => {
                    // Give the name back. Leaving it taken would mean a note
                    // about a file that never arrived, and one fewer name for
                    // the next attempt.
                    let _ = fs::remove_file(&info_path);
                    if e.kind() != io::ErrorKind::AlreadyExists {
                        return Err(e);
                    }
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no free name left in the trash",
        ))
    }

    /// Which trash directory `item` belongs in, and the location string to record.
    ///
    /// The home trash if the entry is already on that filesystem, since trashing is
    /// a rename and a rename cannot cross a mount. Otherwise a trash directory on
    /// the entry's own mount, with the location recorded relative to it: the spec
    /// only supports absolute paths in the home trash, because a mount can appear
    /// at a different place next time and a relative path still means the same
    /// file when it does.
    ///
    /// The filesystem that counts is the one the *directory holding the entry* is
    /// on. A rename moves an entry from one directory to another, so that is the
    /// device that has to match; it is also the right answer for a symlink, which
    /// is trashed as itself wherever its target lives.
    fn destination(&self, item: &Located) -> io::Result<(PathBuf, String)> {
        fs::create_dir_all(&self.home)?;
        let dev = fs::metadata(&item.parent)?.dev();
        if fs::metadata(&self.home)?.dev() == dev {
            return Ok((self.home.clone(), percent_encode(&item.path)));
        }
        let topdir = find_topdir(&item.parent, dev);
        let dir = topdir_trash(&topdir, self.uid)?;
        let relative = item.path.strip_prefix(&topdir).unwrap_or(&item.path);
        Ok((dir, percent_encode(relative)))
    }
}

/// Work out where `path` really is, without resolving the entry itself.
///
/// The path is made absolute, since a relative one recorded in a note means
/// nothing to whoever restores it from another directory, and the directory part
/// is resolved through any symlinks, since both the recorded location and the
/// search for the mount point must describe where the file actually is. The last
/// component is deliberately left alone: trashing a symlink trashes the link, and
/// resolving it would trash what it points at.
fn locate(path: &Path) -> io::Result<Located> {
    let nameless = || io::Error::new(io::ErrorKind::InvalidInput, "no name to trash");
    let absolute = std::path::absolute(path)?;
    let name = absolute.file_name().ok_or_else(nameless)?.to_os_string();
    let parent = fs::canonicalize(absolute.parent().ok_or_else(nameless)?)?;
    Ok(Located {
        path: parent.join(&name),
        parent,
        name,
    })
}

/// Claim `name` in `info/`, returning the reserved file, or `None` if the name is
/// taken.
///
/// `create_new` is `O_CREAT | O_EXCL`: it either makes the file or fails, so
/// two processes racing for one name cannot both believe they got it.
fn reserve(dir: &Path, name: &OsStr) -> io::Result<Option<PathBuf>> {
    let mut info_name = name.to_os_string();
    info_name.push(".trashinfo");
    let info_path = dir.join("info").join(info_name);
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&info_path)
    {
        Ok(_) => Ok(Some(info_path)),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(None),
        Err(e) => Err(e),
    }
}

/// The body of a `.trashinfo` file.
///
/// The date is local wall-clock time with no zone marker, which is what the spec
/// asks for. (Its own worked example contradicts the format it specifies one line
/// above, omitting the dashes; every real implementation follows the rule and not
/// the example, so this does too.)
fn info_contents(recorded_path: &str, now_secs: i64) -> String {
    let local = now_secs + localtime::local_offset(now_secs);
    let (y, mo, d, h, mi, s) = localtime::civil_from_unix(local);
    format!(
        "[Trash Info]\nPath={recorded_path}\nDeletionDate={y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}\n"
    )
}

/// A path with everything a URL may not carry raw percent-encoded, and the
/// separators left as themselves, which is how the spec wants `Path=` written.
///
/// Encoded from the path's bytes rather than from a string made of them. A name
/// that is not UTF-8 has no faithful string form, but its bytes escape like any
/// others, and unescaping gives back exactly the name that was trashed.
fn percent_encode(path: &Path) -> String {
    let mut out = String::new();
    for &byte in path.as_os_str().as_bytes() {
        match byte {
            // Unreserved, plus the separators a path needs to keep as themselves.
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The `n`th name for something already in the trash: `holiday.jpg` becomes
/// `holiday.2.jpg`.
///
/// The number goes before the *first* dot, not the last, which is what gio does:
/// `archive.tar.gz` becomes `archive.2.tar.gz` and keeps its compound extension
/// intact rather than being cut between the two halves of it.
fn disambiguate(name: &OsStr, n: u32) -> OsString {
    let bytes = name.as_bytes();
    let number = format!(".{n}");
    let (stem, rest) = match bytes.iter().position(|&b| b == b'.') {
        // A leading dot is the hidden marker, not an extension boundary; splitting
        // there would turn `.bashrc` into `.2.bashrc`.
        Some(0) | None => (bytes, &[][..]),
        Some(dot) => bytes.split_at(dot),
    };
    OsString::from_vec([stem, number.as_bytes(), rest].concat())
}

/// The mount point of the filesystem `dir` is on: walk up until the device number
/// changes.
///
/// `dir` must already be resolved (see [`locate`]). Walking the parents of a path
/// as it was typed follows its spelling, not the filesystem: a directory reached
/// through a symlink onto another disk has a textual parent on the first disk,
/// and the walk would stop at the link and call it a mount point.
fn find_topdir(dir: &Path, dev: u64) -> PathBuf {
    let mut current = dir;
    while let Some(parent) = current.parent() {
        match fs::metadata(parent) {
            Ok(meta) if meta.dev() == dev => current = parent,
            // The parent is on another filesystem (or cannot be examined), so
            // `current` is as far up as this one goes.
            _ => break,
        }
    }
    current.to_path_buf()
}

/// The trash directory to use on a mount that is not the home one.
///
/// `$topdir/.Trash` is an administrator's doing and is only trusted if it passes
/// both of the spec's checks. It must be a real directory rather than a symlink,
/// because following one would put trashed files somewhere the administrator did
/// not choose, and it must carry the sticky bit, because without it any user can
/// delete another user's trashed files out of a shared directory. Failing either
/// check means falling back to the private `.Trash-$uid`, never to using it
/// anyway — even when the per-user subdirectory inside it already exists.
fn topdir_trash(topdir: &Path, uid: u32) -> io::Result<PathBuf> {
    let shared = topdir.join(".Trash");
    if let Ok(meta) = fs::symlink_metadata(&shared) {
        let usable = meta.is_dir() && !meta.file_type().is_symlink() && meta.mode() & S_ISVTX != 0;
        if usable {
            let mine = shared.join(uid.to_string());
            if fs::create_dir_all(&mine).is_ok() {
                return Ok(mine);
            }
        }
    }
    let private = topdir.join(format!(".Trash-{uid}"));
    fs::create_dir_all(&private)?;
    fs::set_permissions(&private, PermissionsExt::from_mode(TRASH_MODE))?;
    Ok(private)
}

// Declared here rather than in `platform/ffi.rs`: only the trash asks who the
// user is, so the call lives with it.
extern "C" {
    fn getuid() -> c_uint;
}

#[cfg(test)]
mod tests {
    use crate::trash::*;

    /// A unique temp directory for one test, removed by the test that made it.
    /// Resolved, so a path under it is already what the trash would record.
    fn temp_dir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "bnkfiles_trash_{tag}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).expect("make temp dir");
        fs::canonicalize(&base).expect("resolve temp dir")
    }

    /// A trash rooted inside `base`, so no test ever touches the real one.
    fn trash_in(base: &Path) -> Trash {
        Trash::at(base.join("Trash"), 1000)
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    /// The `Path=` a note records.
    fn recorded(info: &Path) -> String {
        read(info)
            .lines()
            .find_map(|l| l.strip_prefix("Path="))
            .expect("a recorded path")
            .to_string()
    }

    #[test]
    fn a_trashed_file_moves_with_a_note_saying_where_it_came_from() {
        let base = temp_dir("put");
        let trash = trash_in(&base);
        let file = base.join("holiday.jpg");
        fs::write(&file, "pixels").expect("file");

        let landed = trash.put(&file, 1_770_000_000).expect("trash it");

        assert!(!file.exists(), "the original is gone from where it was");
        assert_eq!(read(&landed), "pixels", "and is intact in the trash");
        assert_eq!(
            landed,
            base.join("Trash/files/holiday.jpg"),
            "under its own name, in files/"
        );

        let info = read(&base.join("Trash/info/holiday.jpg.trashinfo"));
        let mut lines = info.lines();
        assert_eq!(lines.next(), Some("[Trash Info]"), "the exact header");
        // Absolute, because this is the home trash, and escaped: what the exact
        // escaping is gets pinned on its own below.
        let recorded = lines.next().and_then(|l| l.strip_prefix("Path="));
        assert_eq!(recorded, Some(percent_encode(&file).as_str()));
        assert!(
            recorded.is_some_and(|p| p.starts_with('/')),
            "the home trash records an absolute path"
        );
        let date = lines.next().expect("a deletion date");
        assert!(
            date.starts_with("DeletionDate="),
            "unexpected third line: {date}"
        );
        assert!(info.ends_with('\n'), "the file ends with a newline");

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn the_deletion_date_is_local_wall_clock_time_in_the_spec_s_format() {
        // The format the spec's prose requires, not the one its example shows.
        let contents = info_contents("/tmp/x", 1_770_000_000);
        let date = contents
            .lines()
            .find_map(|l| l.strip_prefix("DeletionDate="))
            .expect("a deletion date");

        let want = {
            let local = 1_770_000_000 + localtime::local_offset(1_770_000_000);
            let (y, mo, d, h, mi, s) = localtime::civil_from_unix(local);
            format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}")
        };
        assert_eq!(date, want);
        // Shape, independent of this machine's zone: dashes in the date, colons in
        // the time, a T between them, and no zone suffix.
        assert_eq!(date.len(), 19, "{date} is not YYYY-MM-DDThh:mm:ss");
        assert_eq!(date.as_bytes()[4], b'-');
        assert_eq!(date.as_bytes()[7], b'-');
        assert_eq!(date.as_bytes()[10], b'T');
        assert_eq!(date.as_bytes()[13], b':');
        assert_eq!(date.as_bytes()[16], b':');
    }

    #[test]
    fn the_recorded_path_is_escaped_as_a_url_but_keeps_its_separators() {
        // Restoring reads this back, so the structure of the path has to survive
        // while the characters a URL cannot carry raw do not.
        let contents = info_contents(&percent_encode(Path::new("/home/me/a b#c.txt")), 0);
        let path = contents
            .lines()
            .find_map(|l| l.strip_prefix("Path="))
            .expect("a path");
        assert_eq!(path, "/home/me/a%20b%23c.txt");
    }

    #[test]
    fn a_second_file_of_the_same_name_does_not_overwrite_the_first() {
        let base = temp_dir("collide");
        let trash = trash_in(&base);
        for body in ["first", "second", "third"] {
            let file = base.join("notes.txt");
            fs::write(&file, body).expect("file");
            trash.put(&file, 1_770_000_000).expect("trash it");
        }

        assert_eq!(read(&base.join("Trash/files/notes.txt")), "first");
        assert_eq!(read(&base.join("Trash/files/notes.2.txt")), "second");
        assert_eq!(read(&base.join("Trash/files/notes.3.txt")), "third");
        // The info file's name is the whole disambiguated name plus the extension,
        // so a restore can pair them up by name alone.
        assert!(base.join("Trash/info/notes.2.txt.trashinfo").exists());
        assert!(base.join("Trash/info/notes.3.txt.trashinfo").exists());

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn disambiguation_goes_before_the_first_dot() {
        // gio's rule, so a compound extension survives intact.
        assert_eq!(disambiguate(OsStr::new("notes.txt"), 2), "notes.2.txt");
        assert_eq!(
            disambiguate(OsStr::new("archive.tar.gz"), 7),
            "archive.7.tar.gz"
        );
        assert_eq!(disambiguate(OsStr::new("README"), 2), "README.2");
        // A leading dot marks a hidden file; it is not an extension boundary.
        assert_eq!(disambiguate(OsStr::new(".bashrc"), 2), ".bashrc.2");
        // Bytes that are not UTF-8 pass through untouched either side of the number.
        assert_eq!(
            disambiguate(OsStr::from_bytes(b"caf\xe9.txt"), 2),
            OsStr::from_bytes(b"caf\xe9.2.txt")
        );
    }

    #[test]
    fn a_trashed_directory_goes_whole_with_one_note() {
        // The spec is explicit that info/ has no subdirectories: a directory is
        // moved with its contents and described by a single info file.
        let base = temp_dir("dir");
        let trash = trash_in(&base);
        let dir = base.join("project");
        fs::create_dir_all(dir.join("src")).expect("dirs");
        fs::write(dir.join("src/main.rs"), "fn main() {}").expect("file");

        trash.put(&dir, 1_770_000_000).expect("trash it");

        assert!(!dir.exists());
        assert_eq!(
            read(&base.join("Trash/files/project/src/main.rs")),
            "fn main() {}",
            "the contents came with it"
        );
        assert!(base.join("Trash/info/project.trashinfo").exists());
        assert!(
            !base.join("Trash/info/project").exists(),
            "info/ has no subdirectories"
        );

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_symlink_is_trashed_as_itself_and_its_target_is_left_alone() {
        let base = temp_dir("symlink");
        let trash = trash_in(&base);
        let target = base.join("target.txt");
        fs::write(&target, "still here").expect("file");
        let link = base.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        trash.put(&link, 1_770_000_000).expect("trash it");

        assert_eq!(read(&target), "still here", "the target is untouched");
        let landed = base.join("Trash/files/link.txt");
        assert!(
            fs::symlink_metadata(&landed)
                .expect("landed")
                .file_type()
                .is_symlink(),
            "the link is still a link"
        );

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_failed_move_gives_the_reserved_name_back() {
        // Otherwise a failure leaves a note about a file that never arrived, and
        // the next attempt at the same name is pushed to a numbered one forever.
        let base = temp_dir("rollback");
        let trash = trash_in(&base);
        let gone = base.join("not-there.txt");

        assert!(
            trash.put(&gone, 1_770_000_000).is_err(),
            "trashing something that is not there must fail"
        );
        assert!(
            !base.join("Trash/info/not-there.txt.trashinfo").exists(),
            "no note was left behind"
        );

        // And a name freed this way is reusable: the real file gets the plain name
        // rather than a numbered one.
        fs::write(&gone, "now it exists").expect("file");
        let landed = trash.put(&gone, 1_770_000_000).expect("trash it");
        assert_eq!(landed, base.join("Trash/files/not-there.txt"));

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_shared_trash_is_used_only_when_it_passes_both_checks() {
        // The spec's fail-closed rule. A `.Trash` that is not sticky, or that is a
        // symlink, must be ignored in favour of the private `.Trash-$uid` even
        // though it exists and looks usable.
        let base = temp_dir("topdir");
        let topdir = base.join("mount");
        fs::create_dir_all(&topdir).expect("topdir");

        // No .Trash at all: the private one, and owner-only.
        let dir = topdir_trash(&topdir, 1000).expect("private trash");
        assert_eq!(dir, topdir.join(".Trash-1000"));
        assert_eq!(
            fs::metadata(&dir).expect("meta").mode() & 0o777,
            TRASH_MODE,
            "a private trash directory is nobody else's business"
        );

        // Present but not sticky: still refused.
        let shared = topdir.join(".Trash");
        fs::create_dir_all(shared.join("1000")).expect("shared");
        fs::set_permissions(&shared, PermissionsExt::from_mode(0o777)).expect("chmod");
        assert_eq!(
            topdir_trash(&topdir, 1000).expect("falls back"),
            topdir.join(".Trash-1000"),
            "a shared trash without the sticky bit is not used, even with a uid dir in it"
        );

        // Sticky: now it is the one to use.
        fs::set_permissions(&shared, PermissionsExt::from_mode(0o1777)).expect("chmod");
        assert_eq!(
            topdir_trash(&topdir, 1000).expect("shared trash"),
            shared.join("1000")
        );

        // A symlink wearing the sticky bit of whatever it points at: refused,
        // because following it would trash files somewhere nobody chose.
        fs::remove_dir_all(&shared).expect("rm");
        std::os::unix::fs::symlink(topdir.join("elsewhere"), &shared).expect("symlink");
        fs::create_dir_all(topdir.join("elsewhere")).expect("elsewhere");
        fs::set_permissions(topdir.join("elsewhere"), PermissionsExt::from_mode(0o1777))
            .expect("chmod");
        assert_eq!(
            topdir_trash(&topdir, 1000).expect("falls back"),
            topdir.join(".Trash-1000"),
            "a symlinked shared trash is refused"
        );

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn the_mount_point_of_a_temp_file_contains_it() {
        // The walk must terminate and must land on a directory the file is
        // actually under, with nothing above it on the same filesystem.
        let base = temp_dir("mount_walk");
        let dev = fs::metadata(&base).expect("meta").dev();

        let topdir = find_topdir(&base, dev);
        assert!(
            base.starts_with(&topdir),
            "{} is not under {}",
            base.display(),
            topdir.display()
        );
        assert_eq!(fs::metadata(&topdir).expect("meta").dev(), dev);
        if let Some(above) = topdir.parent() {
            assert_ne!(
                fs::metadata(above).expect("meta").dev(),
                dev,
                "the walk stopped short of the mount point"
            );
        }

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_directory_reached_through_a_symlink_is_placed_on_its_own_filesystem() {
        // The bug: walking the parents of the path as typed stops at the link,
        // on the wrong filesystem, and the trash is made there. This needs two
        // filesystems to show, so it uses the one the test binary is on when that
        // differs from the temp directory's, and only ever reads from it.
        let base = temp_dir("mount_link");
        let exe = std::env::current_exe().expect("test binary");
        let elsewhere = fs::canonicalize(exe.parent().expect("its directory")).expect("resolve");
        let dev = fs::metadata(&elsewhere).expect("meta").dev();
        if dev == fs::metadata(&base).expect("meta").dev() {
            fs::remove_dir_all(&base).expect("cleanup");
            return;
        }
        let link = base.join("link");
        std::os::unix::fs::symlink(&elsewhere, &link).expect("symlink");
        let name = exe.file_name().expect("name");

        let item = locate(&link.join(name)).expect("locate");
        assert_eq!(item.parent, elsewhere, "the directory is the real one");
        let topdir = find_topdir(&item.parent, dev);
        assert_eq!(
            fs::metadata(&topdir).expect("meta").dev(),
            dev,
            "the mount point is on the file's filesystem, not the link's"
        );
        assert!(elsewhere.starts_with(&topdir));
        assert!(!topdir.starts_with(&base));

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_name_that_is_not_utf8_is_trashed_under_its_own_bytes() {
        // Converted lossily, the file would land under a name with replacement
        // characters in it and the note would record a path that never existed,
        // so nothing could put it back.
        let base = temp_dir("non_utf8");
        let trash = trash_in(&base);
        let name = OsStr::from_bytes(b"caf\xe9.txt");
        let file = base.join(name);
        if fs::write(&file, "body").is_err() {
            // A filesystem that insists on UTF-8 names has no such file to trash.
            fs::remove_dir_all(&base).expect("cleanup");
            return;
        }

        let landed = trash.put(&file, 1_770_000_000).expect("trash it");
        assert_eq!(landed, base.join("Trash/files").join(name));
        assert_eq!(read(&landed), "body");
        let mut info_name = name.to_os_string();
        info_name.push(".trashinfo");
        let path = recorded(&base.join("Trash/info").join(info_name));
        assert!(
            path.ends_with("/caf%E9.txt"),
            "the byte is escaped as itself: {path}"
        );

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_relative_path_is_recorded_as_an_absolute_one() {
        // The home trash must hold absolute paths: a relative one means something
        // different to whoever restores it from another directory. The path is
        // made relative by climbing out of the working directory, which leaves
        // the process's working directory (shared by every test) alone.
        let base = temp_dir("relative");
        let trash = trash_in(&base);
        let file = base.join("notes.txt");
        fs::write(&file, "body").expect("file");
        let cwd = fs::canonicalize(std::env::current_dir().expect("cwd")).expect("resolve");
        let mut relative = PathBuf::new();
        for _ in cwd.components().skip(1) {
            relative.push("..");
        }
        relative.push(file.strip_prefix("/").expect("absolute"));
        assert!(relative.is_relative());

        trash.put(&relative, 1_770_000_000).expect("trash it");
        assert_eq!(
            recorded(&base.join("Trash/info/notes.txt.trashinfo")),
            percent_encode(&file)
        );

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn the_recorded_path_is_where_the_file_really_was() {
        // Reached through a symlinked directory, the file is recorded under the
        // directory it is actually in. The entry itself is never resolved: a
        // trashed symlink is recorded as the link, under its own name.
        let base = temp_dir("real_path");
        let trash = trash_in(&base);
        fs::create_dir_all(base.join("real")).expect("dir");
        fs::write(base.join("real/f.txt"), "f").expect("file");
        fs::write(base.join("real/target.txt"), "target").expect("file");
        std::os::unix::fs::symlink(base.join("real"), base.join("alias")).expect("symlink");
        std::os::unix::fs::symlink("target.txt", base.join("real/link.txt")).expect("symlink");

        trash
            .put(&base.join("alias/f.txt"), 1_770_000_000)
            .expect("trash it");
        assert_eq!(
            recorded(&base.join("Trash/info/f.txt.trashinfo")),
            percent_encode(&base.join("real/f.txt"))
        );

        trash
            .put(&base.join("alias/link.txt"), 1_770_000_000)
            .expect("trash it");
        assert_eq!(
            recorded(&base.join("Trash/info/link.txt.trashinfo")),
            percent_encode(&base.join("real/link.txt"))
        );
        assert_eq!(read(&base.join("real/target.txt")), "target");

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn an_entry_in_the_trash_with_no_note_is_not_overwritten() {
        // `files/` can hold something `info/` knows nothing about. The name looks
        // free, and a plain rename onto it would replace the orphan for good.
        let base = temp_dir("orphan");
        let trash = trash_in(&base);
        fs::create_dir_all(base.join("Trash/files")).expect("files");
        fs::write(base.join("Trash/files/notes.txt"), "orphan").expect("orphan");
        let file = base.join("notes.txt");
        fs::write(&file, "incoming").expect("file");

        let landed = trash.put(&file, 1_770_000_000).expect("trash it");
        assert_eq!(read(&base.join("Trash/files/notes.txt")), "orphan");
        assert_eq!(landed, base.join("Trash/files/notes.2.txt"));
        assert_eq!(read(&landed), "incoming");
        assert!(base.join("Trash/info/notes.2.txt.trashinfo").exists());
        assert!(
            !base.join("Trash/info/notes.txt.trashinfo").exists(),
            "the name that turned out to be taken was given back"
        );

        fs::remove_dir_all(&base).expect("cleanup");
    }
}
