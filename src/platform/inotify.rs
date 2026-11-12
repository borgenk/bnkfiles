//! A thin, safe wrapper over Linux inotify: watch a directory and read the events
//! the kernel queues about it.
//!
//! The explorer watches the directory it is showing. Any change to the names in it
//! means the listing is stale, so events carry no entry name: which watch fired,
//! and whether the watch itself is still alive, is all the caller acts on.
//!
//! The fd is non-blocking and pollable, so the caller waits on it in the same
//! poll as the compositor socket and drains it with [`Inotify::for_each_event`]
//! only when it signals readable.

use std::ffi::{c_char, c_int, c_void, CString};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::platform::error::{Error, Result};

// The subset of event bits the watcher acts on. A directory watch reports these
// for the entries inside it (CLOSE_WRITE, the moves, CREATE, DELETE) and for the
// watched directory itself (DELETE_SELF, MOVE_SELF).
const IN_CLOSE_WRITE: u32 = 0x0000_0008;
const IN_MOVED_FROM: u32 = 0x0000_0040;
const IN_MOVED_TO: u32 = 0x0000_0080;
const IN_CREATE: u32 = 0x0000_0100;
const IN_DELETE: u32 = 0x0000_0200;
const IN_DELETE_SELF: u32 = 0x0000_0400;
const IN_MOVE_SELF: u32 = 0x0000_0800;

/// The kernel dropped the watch: the directory was deleted, or its filesystem
/// unmounted. Sent unasked, whatever the mask.
const IN_IGNORED: u32 = 0x0000_8000;
/// The queue overflowed and events were lost. Sent unasked, about no watch.
const IN_Q_OVERFLOW: u32 = 0x0000_4000;

/// The mask a directory watch is armed with: any change to the set of names in
/// the directory, plus the directory's own disappearance. IN_MODIFY is left out
/// on purpose: CLOSE_WRITE and MOVED_TO are the two signals a completed write or
/// an atomic-rename save produce, and reacting to every write of a growing file
/// would be noise.
pub const DIR_MASK: u32 = IN_CLOSE_WRITE
    | IN_MOVED_FROM
    | IN_MOVED_TO
    | IN_CREATE
    | IN_DELETE
    | IN_DELETE_SELF
    | IN_MOVE_SELF;

// O_NONBLOCK | O_CLOEXEC, spelled as inotify_init1 wants them: the fd never
// blocks a drain and does not leak across an exec.
const IN_NONBLOCK: c_int = 0o4000;
const IN_CLOEXEC: c_int = 0o2000000;

extern "C" {
    fn inotify_init1(flags: c_int) -> c_int;
    fn inotify_add_watch(fd: c_int, pathname: *const c_char, mask: u32) -> c_int;
    fn inotify_rm_watch(fd: c_int, wd: c_int) -> c_int;
    fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
}

/// A watch descriptor: the kernel's handle for one `add_watch`, used to match an
/// event back to the directory it came from and to remove the watch later.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Wd(c_int);

/// What one queued event says, as far as a directory listing cares.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// Something about the watched directory changed.
    Changed(Wd),
    /// The kernel dropped the watch, so nothing more will come from it. The
    /// directory it named may be back under the same path; only a fresh watch
    /// would know.
    Ended(Wd),
    /// Events were lost. Whatever is being watched may have changed unseen.
    Overflowed,
}

/// An open inotify instance. Owns the fd (closed on drop) and nothing else; the
/// mapping from watch descriptor back to a directory lives in the caller.
pub struct Inotify {
    fd: OwnedFd,
}

impl Inotify {
    /// Open a fresh inotify instance. Fails only if the process is out of fds or
    /// inotify instances, in which case the caller runs without live watching.
    pub fn new() -> Result<Self> {
        // SAFETY: a plain syscall with no pointer arguments.
        let raw = unsafe { inotify_init1(IN_NONBLOCK | IN_CLOEXEC) };
        if raw < 0 {
            return Err(Error::msg(format!(
                "inotify_init1 failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        // SAFETY: raw is a fresh, owned fd this call just created.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        Ok(Self { fd })
    }

    /// The raw fd, so an event loop can poll it alongside the compositor socket.
    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Watch `dir` for [`DIR_MASK`]. Re-adding an already watched directory just
    /// returns its existing descriptor, so the caller can be idempotent.
    pub fn add_dir(&self, dir: &Path) -> Result<Wd> {
        let c = CString::new(dir.as_os_str().as_bytes())
            .map_err(|_| Error::msg("watch path contains an interior NUL byte"))?;
        // SAFETY: c is a valid NUL-terminated string that outlives the call.
        let wd = unsafe { inotify_add_watch(self.fd.as_raw_fd(), c.as_ptr(), DIR_MASK) };
        if wd < 0 {
            return Err(Error::msg(format!(
                "inotify_add_watch({}) failed: {}",
                dir.display(),
                std::io::Error::last_os_error()
            )));
        }
        Ok(Wd(wd))
    }

    /// Stop watching a directory. A watch the kernel already dropped (the
    /// directory went away, so the descriptor is stale) is not an error here.
    pub fn rm(&self, wd: Wd) {
        // SAFETY: a plain syscall; a stale descriptor just returns -1 (EINVAL),
        // which is the state we wanted anyway, so the result is ignored.
        unsafe {
            inotify_rm_watch(self.fd.as_raw_fd(), wd.0);
        }
    }

    /// Drain every queued event into `f`, using `buf` as reusable scratch. Reads
    /// until the fd would block, so a burst is consumed in one drain. The kernel
    /// never splits an event across reads, so each read yields whole records.
    pub fn for_each_event<F: FnMut(Event)>(&self, buf: &mut [u8], mut f: F) -> Result<()> {
        loop {
            // SAFETY: buf is a valid, writable slice of the given length; read
            // writes at most that many bytes and returns the count.
            let n = unsafe {
                read(
                    self.fd.as_raw_fd(),
                    buf.as_mut_ptr().cast::<c_void>(),
                    buf.len(),
                )
            };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                match e.kind() {
                    // Queue empty: the drain is done.
                    std::io::ErrorKind::WouldBlock => return Ok(()),
                    // A signal interrupted the read; retry it.
                    std::io::ErrorKind::Interrupted => continue,
                    _ => return Err(Error::msg(format!("inotify read failed: {e}"))),
                }
            }
            if n == 0 {
                return Ok(());
            }
            parse_events(buf.get(..n as usize).unwrap_or_default(), &mut f);
        }
    }
}

/// Walk one read's worth of packed inotify_event records. Each record is a fixed
/// 16-byte header (wd, mask, cookie, name length) followed by the NUL-padded
/// name, which is skipped; the next record begins right after it.
fn parse_events<F: FnMut(Event)>(bytes: &[u8], f: &mut F) {
    const HEADER: usize = 16;
    let mut rest = bytes;
    while let Some((header, after)) = rest.split_first_chunk::<HEADER>() {
        let [w0, w1, w2, w3, m0, m1, m2, m3, _, _, _, _, l0, l1, l2, l3] = *header;
        let wd = Wd(c_int::from_ne_bytes([w0, w1, w2, w3]));
        let mask = u32::from_ne_bytes([m0, m1, m2, m3]);
        let name_len = u32::from_ne_bytes([l0, l1, l2, l3]) as usize;
        // A name running past the buffer is a record the kernel never writes.
        let Some(next) = after.get(name_len..) else {
            return;
        };
        f(if mask & IN_Q_OVERFLOW != 0 {
            Event::Overflowed
        } else if mask & IN_IGNORED != 0 {
            Event::Ended(wd)
        } else {
            Event::Changed(wd)
        });
        rest = next;
    }
}

#[cfg(test)]
mod tests {
    use crate::platform::inotify::*;
    use std::fs;
    use std::path::PathBuf;

    /// A unique scratch directory under the system temp dir, per test name.
    fn tmp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("bnkfiles-inotify-{}-{}", std::process::id(), name));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Read events until `wanted` arrives or the attempt budget runs out. inotify
    /// delivery is asynchronous, so a single drain may see an empty queue; a short
    /// bounded poll avoids both a hang and a flake.
    fn drain_until(ino: &Inotify, wanted: Event) -> Vec<Event> {
        let mut buf = [0u8; 4096];
        let mut out = Vec::new();
        for _ in 0..200 {
            ino.for_each_event(&mut buf, |e| out.push(e)).unwrap();
            if out.contains(&wanted) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        out
    }

    #[test]
    fn reports_a_file_created_in_a_watched_dir() {
        let dir = tmp_dir("create");
        let ino = Inotify::new().unwrap();
        let wd = ino.add_dir(&dir).unwrap();

        fs::write(dir.join("note.md"), b"hi").unwrap();

        let events = drain_until(&ino, Event::Changed(wd));
        assert!(events.contains(&Event::Changed(wd)), "got {events:?}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reports_an_atomic_rename_over_the_target() {
        // How most programs save: write a sibling temp, rename it over the target.
        // The target's inode changes, and the directory watch must still fire.
        let dir = tmp_dir("rename");
        let target = dir.join("note.md");
        let tmp = dir.join(".note.md.tmp");
        fs::write(&target, b"old").unwrap();
        fs::write(&tmp, b"new").unwrap();

        let ino = Inotify::new().unwrap();
        let wd = ino.add_dir(&dir).unwrap();
        fs::rename(&tmp, &target).unwrap();

        let events = drain_until(&ino, Event::Changed(wd));
        assert!(events.contains(&Event::Changed(wd)), "got {events:?}");
        fs::remove_dir_all(&dir).ok();
    }

    /// Deleting the watched directory ends the watch, and a directory recreated at
    /// the same path is a different one the old watch says nothing about. The
    /// caller has to be told, or it goes on believing it is watching.
    #[test]
    fn deleting_the_watched_directory_ends_the_watch() {
        let dir = tmp_dir("gone");
        let ino = Inotify::new().unwrap();
        let wd = ino.add_dir(&dir).unwrap();

        fs::remove_dir_all(&dir).unwrap();

        let events = drain_until(&ino, Event::Ended(wd));
        assert!(events.contains(&Event::Ended(wd)), "got {events:?}");
    }

    #[test]
    fn a_removed_watch_stops_reporting() {
        let dir = tmp_dir("rm");
        let ino = Inotify::new().unwrap();
        let wd = ino.add_dir(&dir).unwrap();
        ino.rm(wd);

        // The removal itself queues the watch's ending; nothing follows it.
        let mut buf = [0u8; 4096];
        ino.for_each_event(&mut buf, |_| {}).unwrap();

        fs::write(dir.join("after.md"), b"x").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut later = Vec::new();
        ino.for_each_event(&mut buf, |e| later.push(e)).unwrap();
        assert!(later.is_empty(), "removed watch still reported {later:?}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_classifies_packed_records_and_skips_their_names() {
        // Header is wd(4) mask(4) cookie(4) len(4), then the NUL-padded name.
        fn record(out: &mut Vec<u8>, wd: i32, mask: u32, name: &str) {
            let padded = if name.is_empty() {
                0
            } else {
                (name.len() + 1 + 3) & !3 // NUL then align to 4
            };
            out.extend_from_slice(&wd.to_ne_bytes());
            out.extend_from_slice(&mask.to_ne_bytes());
            out.extend_from_slice(&0u32.to_ne_bytes()); // cookie
            out.extend_from_slice(&(padded as u32).to_ne_bytes());
            let start = out.len();
            out.extend_from_slice(name.as_bytes());
            out.resize(start + padded, 0);
        }
        let mut bytes = Vec::new();
        record(&mut bytes, 1, IN_CREATE, "alpha.md");
        record(&mut bytes, 2, IN_DELETE, "beta");
        record(&mut bytes, 2, IN_IGNORED, "");
        record(&mut bytes, -1, IN_Q_OVERFLOW, "");

        let mut seen = Vec::new();
        parse_events(&bytes, &mut |e| seen.push(e));
        assert_eq!(
            seen,
            vec![
                Event::Changed(Wd(1)),
                Event::Changed(Wd(2)),
                Event::Ended(Wd(2)),
                Event::Overflowed,
            ]
        );

        // A record cut short, header or name, ends the walk without reading past.
        let mut seen = Vec::new();
        parse_events(&bytes[..bytes.len() - 3], &mut |e| seen.push(e));
        assert_eq!(seen.len(), 3);
        let mut seen = Vec::new();
        parse_events(&bytes[..20], &mut |e| seen.push(e));
        assert!(seen.is_empty(), "a name running past the buffer");
    }
}
