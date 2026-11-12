//! What the sidebar needs to know about its places, asked off the event loop:
//! whether each one's folder is there, and how full each mounted volume is.
//!
//! A stat and a `statvfs` are filesystem calls, and on a network mount that
//! has gone away they can block for as long as the server takes to answer, or
//! forever. So they never run on the event loop: each place is asked on a
//! short-lived thread of its own, which hands its answer back through a
//! channel and wakes the loop through an eventfd like the scanner does. A
//! place still being asked about is not asked again, so a hung mount strands
//! one thread, not one per request, and holds back no other place's row.

use std::collections::HashSet;
use std::ffi::CString;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};

use crate::platform::error::{Error, Result};
use crate::platform::ffi;

/// How much of a volume is in use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fill {
    pub total: u64,
    pub available: u64,
}

impl Fill {
    /// The used share, from 0.0 to 1.0. An empty-sized volume (a pseudo
    /// filesystem that reports nothing) counts as empty, not as full.
    pub fn used(self) -> f32 {
        if self.total == 0 {
            return 0.0;
        }
        let used = self.total.saturating_sub(self.available);
        (used as f64 / self.total as f64) as f32
    }
}

/// glibc's `struct statvfs` on a 64-bit target, where every field but the
/// spare words is 64 bits wide (`unsigned long` and the block counts alike).
/// bnkfiles builds for 64-bit Linux only. The trailing spare words are the
/// kernel ABI's padding; they are never read.
#[repr(C)]
#[derive(Default)]
struct StatVfs {
    f_bsize: u64,
    f_frsize: u64,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_favail: u64,
    f_fsid: u64,
    f_flag: u64,
    f_namemax: u64,
    f_spare: [std::ffi::c_int; 6],
}

extern "C" {
    fn statvfs(path: *const std::ffi::c_char, buf: *mut StatVfs) -> std::ffi::c_int;
}

/// How full the volume holding `path` is, or `None` if it cannot be asked.
/// Blocks for as long as the filesystem takes: call it off the event loop.
pub fn of(path: &Path) -> Option<Fill> {
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut buf = StatVfs::default();
    // SAFETY: `c_path` is NUL-terminated and outlives the call; `buf` is a live
    // local of the layout the call fills.
    let rc = unsafe { statvfs(c_path.as_ptr(), &mut buf) };
    if rc != 0 {
        return None;
    }
    let unit = if buf.f_frsize > 0 {
        buf.f_frsize
    } else {
        buf.f_bsize
    };
    Some(Fill {
        total: buf.f_blocks.saturating_mul(unit),
        available: buf.f_bavail.saturating_mul(unit),
    })
}

/// A place to ask about, and whether to measure its volume: only a mounted
/// volume's row has a bar to fill.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ask {
    pub path: PathBuf,
    pub measure: bool,
}

/// What was found out about one place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    pub path: PathBuf,
    /// A folder is there.
    pub there: bool,
    /// How full its volume is, if it was to be measured and could be.
    pub fill: Option<Fill>,
}

/// What `ask` finds out, the slow way: call it off the event loop.
fn answer(ask: Ask) -> Answer {
    let there = std::fs::metadata(&ask.path).is_ok_and(|m| m.is_dir());
    let fill = if ask.measure && there {
        of(&ask.path)
    } else {
        None
    };
    Answer {
        path: ask.path,
        there,
        fill,
    }
}

pub struct Usage {
    tx: Sender<Answer>,
    rx: Receiver<Answer>,
    wake: OwnedFd,
    /// Places asked about and not yet answered. One that hangs stays here, so
    /// asking again does not strand another thread on it.
    pending: HashSet<PathBuf>,
}

impl Usage {
    pub fn new() -> Result<Self> {
        let wake = ffi::eventfd()?;
        let (tx, rx) = std::sync::mpsc::channel();
        Ok(Self {
            tx,
            rx,
            wake,
            pending: HashSet::new(),
        })
    }

    /// The eventfd to add to the main loop's poll set.
    pub fn wake_fd(&self) -> RawFd {
        self.wake.as_raw_fd()
    }

    /// Ask about each of `asks`, skipping any place still being asked. Each
    /// is asked on a thread of its own and answered as soon as it is known,
    /// so a place on a hung mount holds back nothing but its own row. One that
    /// cannot be asked is left as it was last known, which is all a sidebar
    /// row is worth, and asked again next time.
    pub fn request(&mut self, asks: Vec<Ask>) {
        for ask in asks {
            if !self.pending.insert(ask.path.clone()) {
                continue;
            }
            let path = ask.path.clone();
            if let Err(e) = self.spawn(ask) {
                self.pending.remove(&path);
                eprintln!("bnkfiles: {e}");
            }
        }
    }

    /// Start the thread that answers `ask`.
    fn spawn(&self, ask: Ask) -> Result<()> {
        let tx = self.tx.clone();
        let wake = self
            .wake
            .try_clone()
            .map_err(|e| Error::msg(format!("usage wake: {e}")))?;
        std::thread::Builder::new()
            .name("bnkfiles-usage".into())
            .spawn(move || {
                // The receiver is gone only if the app is on its way out.
                if tx.send(answer(ask)).is_ok() {
                    ffi::event_signal(wake.as_raw_fd());
                }
            })
            .map(drop)
            .map_err(|e| Error::msg(format!("usage thread: {e}")))
    }

    /// The answers that have come in since last asked.
    pub fn take(&mut self) -> Vec<Answer> {
        ffi::event_drain(self.wake.as_raw_fd());
        let answers: Vec<_> = self.rx.try_iter().collect();
        for answer in &answers {
            self.pending.remove(&answer.path);
        }
        answers
    }
}

#[cfg(test)]
mod tests {
    use crate::usage::*;

    #[test]
    fn the_root_has_a_size_and_no_more_free_than_that() {
        let fill = of(Path::new("/")).expect("statvfs on /");
        assert!(fill.total > 0);
        assert!(fill.available <= fill.total);
        assert!((0.0..=1.0).contains(&fill.used()));
    }

    #[test]
    fn a_path_that_is_not_there_has_no_answer() {
        assert_eq!(of(Path::new("/no/such/volume/here")), None);
        assert_eq!(
            of(Path::new("bad\0path")),
            None,
            "a NUL is refused, not passed on"
        );
    }

    #[test]
    fn a_volume_with_no_size_reads_as_empty() {
        let none = Fill {
            total: 0,
            available: 0,
        };
        assert_eq!(none.used(), 0.0);
        let half = Fill {
            total: 100,
            available: 50,
        };
        assert_eq!(half.used(), 0.5);
    }

    fn ask(path: &str, measure: bool) -> Ask {
        Ask {
            path: PathBuf::from(path),
            measure,
        }
    }

    #[test]
    fn a_place_is_there_when_a_folder_is_and_only_a_volume_is_measured() {
        let root = answer(ask("/", true));
        assert!(root.there && root.fill.is_some(), "{root:?}");
        assert_eq!(answer(ask("/", false)).fill, None, "a pin has no bar");
        let gone = answer(ask("/no/such/folder", true));
        assert!(!gone.there && gone.fill.is_none());
        // A file where the folder was is not a place to go.
        let file = std::env::temp_dir().join(format!("bnkfiles_usage_{}", std::process::id()));
        std::fs::write(&file, "x").expect("file");
        assert!(
            !answer(Ask {
                path: file.clone(),
                measure: false
            })
            .there
        );
        std::fs::remove_file(&file).expect("cleanup");
    }

    #[test]
    fn each_volume_is_answered_and_one_being_asked_is_not_asked_again() {
        let mut usage = Usage::new().expect("eventfd");
        usage.request(vec![ask("/", true), ask("/no/such", true)]);
        // Asked again before answering: nothing new to ask.
        usage.request(vec![ask("/", true)]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut answers = Vec::new();
        while answers.len() < 2 {
            answers.extend(usage.take());
            assert!(std::time::Instant::now() < deadline, "no answer");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        answers.extend(usage.take());
        assert_eq!(
            answers.len(),
            2,
            "the second request asked nothing: {answers:?}"
        );
        assert!(answers
            .iter()
            .any(|a| a.path == Path::new("/") && a.there && a.fill.is_some()));
        assert!(answers
            .iter()
            .any(|a| a.path == Path::new("/no/such") && !a.there && a.fill.is_none()));
        // Answered, so it can be asked again.
        usage.request(vec![ask("/", true)]);
        assert!(usage.pending.contains(Path::new("/")));
    }
}
