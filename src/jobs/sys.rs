//! The filesystem syscalls std has no door to, each behind a safe wrapper.
//!
//! Declared here rather than in `platform/ffi.rs`: moving files is the file
//! operations' business, not the platform layer's, so the syscalls only they
//! need live beside them.
//!
//! Everything in here is mechanism. What a failure *means* (whether a lost
//! attribute fails a move, whether a name being taken is a conflict or a retry)
//! is decided by the caller, which knows what the user asked for.

use std::ffi::{c_char, c_int, c_long, c_void, CStr, CString, OsString};
use std::fs::{File, Metadata};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

const EPERM: c_int = 1;
const ENXIO: c_int = 6;
const EXDEV: c_int = 18;
const EINVAL: c_int = 22;
const ERANGE: c_int = 34;
const ENOSYS: c_int = 38;
const ENODATA: c_int = 61;
const EOPNOTSUPP: c_int = 95;

/// Refuse to open a symlink in the final path component.
///
/// One of the handful of open flags whose value depends on the architecture, so
/// it is spelled per target rather than guessed: an unlisted target fails to
/// build here instead of silently passing some other flag.
#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
pub(crate) const O_NOFOLLOW: c_int = 0o400_000;
#[cfg(target_arch = "aarch64")]
pub(crate) const O_NOFOLLOW: c_int = 0o100_000;

/// Fail unless the path names a directory. Architecture-dependent like
/// [`O_NOFOLLOW`].
#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
pub(crate) const O_DIRECTORY: c_int = 0o200_000;
#[cfg(target_arch = "aarch64")]
pub(crate) const O_DIRECTORY: c_int = 0o40_000;

/// Return from `open` at once rather than waiting for the other end of a FIFO.
pub(crate) const O_NONBLOCK: c_int = 0o4_000;

/// Never let an opened terminal become this process's controlling one.
pub(crate) const O_NOCTTY: c_int = 0o400;

/// `renameat2` has no wrapper in every libc this builds against, so it goes
/// through `syscall(2)` by number.
#[cfg(target_arch = "x86_64")]
const SYS_RENAMEAT2: c_long = 316;
#[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
const SYS_RENAMEAT2: c_long = 276;

const AT_FDCWD: c_long = -100;
const RENAME_NOREPLACE: c_long = 1;

const SEEK_DATA: c_int = 3;
const SEEK_HOLE: c_int = 4;

/// `_IOW(0x94, 9, int)`: share one file's extents with another, where the
/// filesystem can.
const FICLONE: std::ffi::c_ulong = ioc_write(0x94, 9, 4);

/// The `_IOW` encoding: direction 1 (write) in the top two bits, then the size,
/// then the type and number.
const fn ioc_write(ty: u32, nr: u32, size: u32) -> std::ffi::c_ulong {
    ((1 << 30) | (size << 16) | (ty << 8) | nr) as std::ffi::c_ulong
}

#[repr(C)]
struct Timespec {
    sec: i64,
    nsec: i64,
}

extern "C" {
    fn syscall(num: c_long, ...) -> c_long;
    fn copy_file_range(
        fd_in: c_int,
        off_in: *mut i64,
        fd_out: c_int,
        off_out: *mut i64,
        len: usize,
        flags: std::ffi::c_uint,
    ) -> isize;
    fn ioctl(fd: c_int, request: std::ffi::c_ulong, ...) -> c_int;
    fn futimens(fd: c_int, times: *const Timespec) -> c_int;
    fn lseek64(fd: c_int, offset: i64, whence: c_int) -> i64;
    fn flistxattr(fd: c_int, list: *mut c_char, size: usize) -> isize;
    fn fgetxattr(fd: c_int, name: *const c_char, value: *mut c_void, size: usize) -> isize;
    fn fsetxattr(
        fd: c_int,
        name: *const c_char,
        value: *const c_void,
        size: usize,
        flags: c_int,
    ) -> c_int;
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

/// Whether an error is `EXDEV`: the two paths are on different filesystems.
pub(crate) fn is_cross_device(err: &io::Error) -> bool {
    err.raw_os_error() == Some(EXDEV)
}

/// Rename `src` to `dst` only if nothing is at `dst`, as one atomic step.
///
/// Checking that a name is free and then renaming onto it leaves a window in
/// which something else can take the name and be silently replaced.
/// `RENAME_NOREPLACE` closes it: the kernel either moves the entry or reports
/// `AlreadyExists`, with nothing in between.
///
/// A filesystem that cannot do it answers `EINVAL` (an old kernel, `ENOSYS`), and
/// for those the check-then-rename is all there is, window included.
pub(crate) fn rename_noreplace(src: &Path, dst: &Path) -> io::Result<()> {
    let (from, to) = (c_path(src)?, c_path(dst)?);
    // SAFETY: both pointers are NUL-terminated strings that outlive the call, and
    // the remaining arguments are plain integers in the order renameat2 takes them.
    let rc = unsafe {
        syscall(
            SYS_RENAMEAT2,
            AT_FDCWD,
            from.as_ptr(),
            AT_FDCWD,
            to.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if rc == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if !matches!(err.raw_os_error(), Some(EINVAL) | Some(ENOSYS)) {
        return Err(err);
    }
    if std::fs::symlink_metadata(dst).is_ok() {
        return Err(io::Error::from(io::ErrorKind::AlreadyExists));
    }
    std::fs::rename(src, dst)
}

/// Try to reflink the whole file, reporting whether it worked.
pub(crate) fn clone_file(src: &File, dst: &File) -> bool {
    // SAFETY: both descriptors are open for the call. FICLONE reads the source
    // descriptor as its argument and writes nothing back through the pointer.
    let rc = unsafe { ioctl(dst.as_raw_fd(), FICLONE, src.as_raw_fd()) };
    rc == 0
}

/// Whether an error means "this kernel or filesystem cannot do that", as opposed
/// to the copy having genuinely failed.
pub(crate) fn unsupported(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(EINVAL) | Some(ENOSYS) | Some(EXDEV) | Some(EOPNOTSUPP) | Some(EPERM)
    )
}

/// Have the kernel move up to `len` bytes at `offset` from `src` to the same
/// offset in `dst`, returning how many it moved. Zero means `src` ends there.
///
/// The offsets are passed explicitly, so neither file's own position is read or
/// moved and a caller can switch to another way of copying part way through.
pub(crate) fn copy_range(src: &File, dst: &File, offset: u64, len: usize) -> io::Result<usize> {
    let mut off_in = offset as i64;
    let mut off_out = offset as i64;
    // SAFETY: both descriptors are open for the duration of the call, and the two
    // offsets are live locals the kernel reads and advances.
    let n = unsafe {
        copy_file_range(
            src.as_raw_fd(),
            &mut off_in,
            dst.as_raw_fd(),
            &mut off_out,
            len,
            0,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

/// Where the next stored bytes begin at or after `from`, or `None` when only a
/// hole is left before the end of the file.
pub(crate) fn next_data(file: &File, from: u64) -> io::Result<Option<u64>> {
    // SAFETY: lseek takes no pointers; the descriptor is open for the call.
    let at = unsafe { lseek64(file.as_raw_fd(), from as i64, SEEK_DATA) };
    if at >= 0 {
        return Ok(Some(at as u64));
    }
    let err = io::Error::last_os_error();
    // ENXIO is how SEEK_DATA says "nothing but hole from here on".
    if err.raw_os_error() == Some(ENXIO) {
        return Ok(None);
    }
    Err(err)
}

/// Where the next hole begins at or after `from`. The end of the file counts as
/// one, so there is always an answer for an offset inside the file.
pub(crate) fn next_hole(file: &File, from: u64) -> io::Result<u64> {
    // SAFETY: lseek takes no pointers; the descriptor is open for the call.
    let at = unsafe { lseek64(file.as_raw_fd(), from as i64, SEEK_HOLE) };
    if at < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(at as u64)
}

/// Give `file` the access and modification times `meta` describes.
///
/// The access time is set to the source's too. A copy is not a read of the
/// original in any sense the user cares about, and leaving atime at "now" while
/// mtime goes back years makes for a confusing pair. Best-effort: a filesystem
/// that will not take the times still has the bytes.
pub(crate) fn set_times(file: &File, meta: &Metadata) {
    let times = [
        Timespec {
            sec: meta.atime(),
            nsec: meta.atime_nsec(),
        },
        Timespec {
            sec: meta.mtime(),
            nsec: meta.mtime_nsec(),
        },
    ];
    // SAFETY: `times` is a live array of exactly the two entries futimens reads.
    unsafe {
        futimens(file.as_raw_fd(), times.as_ptr());
    }
}

/// How many times to re-ask when an attribute grows between sizing it and
/// reading it. Something rewriting attributes that fast is not going to settle.
const XATTR_RETRIES: usize = 4;

/// Call `read` with a buffer of the size it says it needs.
///
/// The xattr calls report the size needed when handed an empty buffer, then fill
/// one of that size; `ERANGE` means the value grew in between, so ask again.
fn sized_read(read: impl Fn(*mut u8, usize) -> isize) -> io::Result<Vec<u8>> {
    for _ in 0..XATTR_RETRIES {
        let need = read(std::ptr::null_mut(), 0);
        if need < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0u8; need as usize];
        let got = read(buf.as_mut_ptr(), buf.len());
        if got >= 0 {
            buf.truncate(got as usize);
            return Ok(buf);
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(ERANGE) {
            return Err(err);
        }
    }
    Err(io::Error::from_raw_os_error(ERANGE))
}

/// The names of every extended attribute on `file`. A filesystem with no
/// attributes at all is an empty list, not an error.
fn list_xattrs(file: &File) -> io::Result<Vec<CString>> {
    let fd = file.as_raw_fd();
    // SAFETY: the pointer and length describe `sized_read`'s own buffer (or are
    // null and zero for the sizing call), and the descriptor is open.
    let raw = match sized_read(|buf, len| unsafe { flistxattr(fd, buf.cast(), len) }) {
        Ok(raw) => raw,
        Err(e) if matches!(e.raw_os_error(), Some(EOPNOTSUPP) | Some(ENOSYS)) => Vec::new(),
        Err(e) => return Err(e),
    };
    // The list is the names end to end, each NUL-terminated.
    Ok(raw
        .split(|&b| b == 0)
        .filter(|name| !name.is_empty())
        .filter_map(|name| CString::new(name).ok())
        .collect())
}

/// The value of one extended attribute, or `None` if `file` does not have it.
pub(crate) fn get_xattr(file: &File, name: &CStr) -> io::Result<Option<Vec<u8>>> {
    let fd = file.as_raw_fd();
    // SAFETY: `name` is NUL-terminated and outlives the call; the pointer and
    // length describe `sized_read`'s own buffer (or are null and zero).
    match sized_read(|buf, len| unsafe { fgetxattr(fd, name.as_ptr(), buf.cast(), len) }) {
        Ok(value) => Ok(Some(value)),
        Err(e) if e.raw_os_error() == Some(ENODATA) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Set one extended attribute on `file`, replacing any value it already has.
pub(crate) fn set_xattr(file: &File, name: &CStr, value: &[u8]) -> io::Result<()> {
    // SAFETY: `name` is NUL-terminated, `value` is a live slice of the length
    // passed, and the descriptor is open. Flags of zero mean create-or-replace.
    let rc = unsafe {
        fsetxattr(
            file.as_raw_fd(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Copy every extended attribute from `src` to `dst`, returning the names of the
/// ones that could not be carried.
///
/// Through the descriptors rather than by path, so what is read and written is
/// the pair of files already open and not whatever the paths name by now.
///
/// One attribute failing does not stop the rest: `security.capability` needs
/// `CAP_SETFCAP` and `trusted.*` needs `CAP_SYS_ADMIN`, so an unprivileged copy is
/// expected to lose those, exactly as `cp` does, and a destination filesystem
/// with no attributes loses all of them. Which losses matter is the caller's
/// call, which is why they are returned rather than judged here. `Err` means the
/// source's attributes could not even be listed.
pub(crate) fn copy_xattrs(src: &File, dst: &File) -> io::Result<Vec<OsString>> {
    let mut lost = Vec::new();
    for name in list_xattrs(src)? {
        let carried = match get_xattr(src, &name) {
            Ok(Some(value)) => set_xattr(dst, &name, &value).is_ok(),
            // Removed since it was listed, so there is nothing to lose.
            Ok(None) => true,
            Err(_) => false,
        };
        if !carried {
            lost.push(OsString::from_vec(name.into_bytes()));
        }
    }
    Ok(lost)
}

#[cfg(test)]
mod tests {
    use crate::jobs::sys::*;
    use std::fs;
    use std::path::PathBuf;

    /// A unique temp directory for one test, removed by the test that made it.
    fn temp_dir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "bnkfiles_sys_{tag}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).expect("make temp dir");
        base
    }

    #[test]
    fn a_rename_that_would_replace_something_is_refused_and_changes_nothing() {
        // The whole reason for the call: a plain rename would have replaced
        // `taken` without a word.
        let base = temp_dir("noreplace");
        let (from, taken, free) = (base.join("from"), base.join("taken"), base.join("free"));
        fs::write(&from, "incoming").expect("file");
        fs::write(&taken, "resident").expect("file");

        let err = rename_noreplace(&from, &taken).expect_err("the name is taken");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&taken).expect("read"), "resident");
        assert_eq!(fs::read_to_string(&from).expect("read"), "incoming");

        // A dangling symlink is still something in the way.
        let dangling = base.join("dangling");
        std::os::unix::fs::symlink(base.join("nowhere"), &dangling).expect("symlink");
        assert_eq!(
            rename_noreplace(&from, &dangling)
                .expect_err("a link is in the way")
                .kind(),
            io::ErrorKind::AlreadyExists
        );

        rename_noreplace(&from, &free).expect("a free name is taken");
        assert_eq!(fs::read_to_string(&free).expect("read"), "incoming");
        assert!(!from.exists());

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn data_and_holes_are_found_where_they_are() {
        // The walk the sparse copy rests on: a hole, then data, then a hole to the
        // end. A filesystem that stores no holes reports the whole file as data,
        // which is a correct answer too, so only the invariants are pinned.
        let base = temp_dir("seek");
        let path = base.join("sparse");
        let file = File::create(&path).expect("create");
        file.set_len(8 * 1024 * 1024).expect("size");
        std::os::unix::fs::FileExt::write_all_at(&file, b"data", 4 * 1024 * 1024).expect("write");
        let file = File::open(&path).expect("open");

        let data = next_data(&file, 0)
            .expect("seek data")
            .expect("there is data");
        assert!(data <= 4 * 1024 * 1024, "the data starts at or before it");
        let hole = next_hole(&file, data).expect("seek hole");
        assert!(
            hole >= 4 * 1024 * 1024 + 4,
            "the hole starts after the data"
        );
        assert!(hole <= 8 * 1024 * 1024);
        if hole < 8 * 1024 * 1024 {
            assert_eq!(
                next_data(&file, hole).expect("seek data"),
                None,
                "nothing but hole after the last data"
            );
        }

        fs::remove_dir_all(&base).expect("cleanup");
    }
}
