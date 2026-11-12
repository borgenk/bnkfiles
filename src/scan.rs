//! Directory scanning, off the event loop.
//!
//! Reading a directory is a syscall per entry plus a stat per entry; on a home
//! folder that is microseconds, on a 100k-entry directory on a cold cache it is
//! long enough to drop frames. So a worker thread owns the reading, exactly the way
//! [`crate::loader`] owns decoding: the main thread asks for a directory, the worker
//! streams back sorted batches, and an eventfd wakes the loop's poll so the batches
//! are picked up promptly.
//!
//! One worker, not a pool. The loader has real parallelism to exploit (N images
//! decode independently); a scan is one directory read that the kernel serialises
//! anyway, and one directory is ever being looked at.
//!
//! **Generations.** The main thread bumps a shared counter when it points the
//! scanner somewhere new. The worker re-reads it between batches, so leaving a
//! directory abandons its scan mid-read rather than paying out the rest of a 100k
//! listing nobody is waiting for. An abandoned scan sends no [`Payload::Done`]; a
//! scan that is still current always sends exactly one terminal message, so the main
//! thread's "still scanning" state can never strand.
//!
//! ```text
//!   request_scan ──▶ [dir, generation] ──▶ worker ──▶ Batch, Batch, ... Done
//!        │                                              │
//!        └── generation.store(n) ─────────────▶ read between batches
//! ```

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::files::{self, FsEntry};
use crate::order::Order;
use crate::platform::error::{Error, Result};
use crate::platform::ffi;
use crate::unreadable::{Reason, Unreadable};

/// Entries per batch. Small enough that the first rows appear immediately and a
/// navigation is noticed promptly; large enough that a big directory is not a
/// thousand channel sends and merges.
const BATCH: usize = 512;

/// One message from the scan worker, tagged with the generation it was produced
/// for so the main thread can drop a batch from a directory it has already left.
pub struct ScanMsg {
    pub generation: u64,
    pub payload: Payload,
}

pub enum Payload {
    /// The directory's saved thumbnails, read before the directory itself so they
    /// can be drawn the moment the entries they belong to appear. Always first,
    /// and absent when there is no cache to read.
    Cached(Vec<crate::thumbcache::Cached>),
    /// A run of entries, sorted within the batch in the order the scan was
    /// asked for.
    Batch(Vec<FsEntry>),
    /// The directory was read to the end. Terminal.
    Done { total: usize },
    /// The directory could not be read, or could not be read to the end.
    /// Terminal. Any batches that came before it are a listing with entries
    /// missing, which is why this is said instead of `Done`: a short list that
    /// claimed to be complete would be believed.
    Failed(Unreadable),
}

/// A directory to read, and the generation it was asked for at.
struct ScanReq {
    dir: PathBuf,
    generation: u64,
    order: Order,
}

/// A deadline that a burst of notifications collapses into one.
///
/// A directory watch is not a request to rescan; it is a request to rescan
/// *eventually*. An unpack or a `git checkout` fires hundreds of events over a
/// second or two, and re-reading the directory for each one would spend the whole
/// burst scanning. Every event pushes the deadline out, so the rescan happens once
/// the directory has stopped moving.
pub struct Debounce {
    window: Duration,
    due: Option<Instant>,
}

impl Debounce {
    pub fn new(window: Duration) -> Self {
        Self { window, due: None }
    }

    /// Something happened: the rescan is due one window from now, replacing any
    /// deadline already set.
    pub fn touch(&mut self, now: Instant) {
        self.due = Some(now + self.window);
    }

    /// The deadline, for the event loop's next wake-up.
    pub fn deadline(&self) -> Option<Instant> {
        self.due
    }

    /// Whether the wait is over, clearing it if so. Returns true exactly once per
    /// burst, so a caller cannot rescan twice for one settling.
    pub fn fired(&mut self, now: Instant) -> bool {
        match self.due {
            Some(at) if now >= at => {
                self.due = None;
                true
            }
            _ => false,
        }
    }
}

pub struct Scanner {
    req_tx: Sender<ScanReq>,
    res_rx: Receiver<ScanMsg>,
    /// The generation the main thread is currently interested in. The worker reads
    /// it between batches; anything older is abandoned.
    generation: Arc<AtomicU64>,
    /// A clone of the wake eventfd for the main loop's poll set.
    wake: OwnedFd,
    _worker: JoinHandle<()>,
}

impl Scanner {
    /// `cache` is the thumbnail cache the worker warms each scan from, or `None`
    /// to run without one.
    pub fn new(cache: Option<crate::thumbcache::Cache>) -> Result<Self> {
        let wake = ffi::eventfd()?;
        let worker_wake = wake
            .try_clone()
            .map_err(|e| Error::msg(format!("could not dup the scan wake fd: {e}")))?;
        let (req_tx, req_rx) = std::sync::mpsc::channel::<ScanReq>();
        let (res_tx, res_rx) = std::sync::mpsc::channel::<ScanMsg>();
        let generation = Arc::new(AtomicU64::new(0));
        let worker_generation = Arc::clone(&generation);
        let worker = std::thread::spawn(move || {
            worker_loop(req_rx, res_tx, worker_generation, worker_wake, cache)
        });
        Ok(Self {
            req_tx,
            res_rx,
            generation,
            wake,
            _worker: worker,
        })
    }

    /// The eventfd to add to the main loop's poll set.
    pub fn wake_fd(&self) -> RawFd {
        self.wake.as_raw_fd()
    }

    /// Read `dir` at `generation`, abandoning whatever is being read now.
    ///
    /// The generation is published before the request is queued: a scan already
    /// running sees the new value at its next batch and stops there, rather than
    /// finishing a directory nobody is looking at.
    pub fn scan(&self, dir: PathBuf, generation: u64, order: Order) {
        self.generation.store(generation, Ordering::Release);
        // The send fails only if the worker thread is gone, which means the process
        // is on its way down and there is nothing useful to do about it.
        let _ = self.req_tx.send(ScanReq {
            dir,
            generation,
            order,
        });
    }

    /// Take the next scan message, if any. Non-blocking.
    pub fn try_recv(&self) -> Option<ScanMsg> {
        self.res_rx.try_recv().ok()
    }

    /// Reset the wake fd before draining, so a poll does not re-fire on
    /// notifications already dealt with.
    pub fn drain_wake(&self) {
        ffi::event_drain(self.wake.as_raw_fd());
    }
}

// The worker needs no explicit shutdown flag: dropping the `Scanner` drops the
// request sender, so an idle worker's `recv` ends and the thread exits, and drops
// the result receiver, so a worker mid-scan fails its next send and abandons the
// directory there. Both are checked every batch, which bounds how long a scan can
// outlive the app that asked for it.

fn worker_loop(
    req_rx: Receiver<ScanReq>,
    res_tx: Sender<ScanMsg>,
    generation: Arc<AtomicU64>,
    wake: OwnedFd,
    cache: Option<crate::thumbcache::Cache>,
) {
    while let Ok(req) = req_rx.recv() {
        // A request the user has already navigated away from: skip it before
        // its cache file is read, not after. Holding a key down queues one per
        // folder passed through.
        if !current(&req, &generation) {
            continue;
        }
        // The saved thumbnails first, so they are in hand before the entries they
        // belong to arrive. Reading them here rather than on the main thread is
        // the whole point: it is a file read, and the loop does not do those.
        if let Some(cache) = cache.as_ref() {
            let saved = cache.load(&req.dir);
            if !saved.is_empty() && !send(&res_tx, &wake, req.generation, Payload::Cached(saved)) {
                continue;
            }
        }
        scan_one(&req, &res_tx, &generation, &wake);
    }
}

/// Read one directory, streaming sorted batches until it ends, the generation moves
/// on, or the main thread goes away.
fn scan_one(req: &ScanReq, res_tx: &Sender<ScanMsg>, generation: &AtomicU64, wake: &OwnedFd) {
    match std::fs::read_dir(&req.dir) {
        Ok(entries) => stream(entries, req, res_tx, generation, wake),
        Err(e) => {
            let reason = match Reason::of(&e) {
                // ENOTDIR says something on the way is a file, which may be a
                // folder above this one rather than this one. Then this one is
                // simply not there.
                Reason::NotFolder if std::fs::metadata(&req.dir).is_err() => Reason::Missing,
                reason => reason,
            };
            // Only for a folder that is not there. The filesystem answered
            // promptly to say so; one that is not answering would take as long
            // again for every folder above, on the one worker every navigation
            // queues behind, and Up from it falls back to the parent anyway.
            let nearest = match reason {
                Reason::Missing | Reason::NotFolder => nearest_folder(&req.dir),
                _ => None,
            };
            let failure = Unreadable {
                reason,
                detail: e.to_string(),
                partial: false,
                nearest,
            };
            send(res_tx, wake, req.generation, Payload::Failed(failure));
        }
    }
}

/// The nearest folder above `dir` that is there. Asked here, on the worker,
/// for a folder that is not: where the explorer goes when the folder it was
/// showing has gone, and where Up goes from one that is not there. Each step
/// is a stat, and a path a few folders deep is a few.
fn nearest_folder(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .skip(1)
        .find(|above| std::fs::metadata(above).is_ok_and(|m| m.is_dir()))
        .map(Path::to_path_buf)
}

/// Send what `entries` yields as sorted batches, then say how the listing ended.
///
/// A listing can fail part way through: a network or FUSE filesystem drops out
/// with some entries delivered and the rest not. The entries that did arrive are
/// still sent, since they are real and the user may as well see them, but the
/// scan then ends in `Failed` rather than `Done`. Stepping over the error instead
/// would present a folder with files silently missing as the whole folder.
fn stream(
    entries: impl Iterator<Item = std::io::Result<std::fs::DirEntry>>,
    req: &ScanReq,
    res_tx: &Sender<ScanMsg>,
    generation: &AtomicU64,
    wake: &OwnedFd,
) {
    let mut batch = Vec::with_capacity(BATCH);
    let mut total = 0usize;
    let mut broken = None;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                broken = Some(e);
                break;
            }
        };
        batch.push(files::describe(&entry));
        total += 1;
        if batch.len() >= BATCH && !flush(res_tx, wake, req, generation, &mut batch) {
            return;
        }
    }
    if !batch.is_empty() && !flush(res_tx, wake, req, generation, &mut batch) {
        return;
    }
    if !current(req, generation) {
        return;
    }
    let ending = match broken {
        None => Payload::Done { total },
        Some(e) => Payload::Failed(Unreadable {
            reason: Reason::of(&e),
            detail: e.to_string(),
            partial: true,
            nearest: None,
        }),
    };
    send(res_tx, wake, req.generation, ending);
}

/// Sort and send the batch, reporting whether the scan should carry on.
fn flush(
    res_tx: &Sender<ScanMsg>,
    wake: &OwnedFd,
    req: &ScanReq,
    generation: &AtomicU64,
    batch: &mut Vec<FsEntry>,
) -> bool {
    if !current(req, generation) {
        return false;
    }
    let mut sorted = std::mem::replace(batch, Vec::with_capacity(BATCH));
    sorted.sort_by(|a, b| req.order.cmp(a, b));
    send(res_tx, wake, req.generation, Payload::Batch(sorted))
}

/// Whether this scan is still the one the main thread wants.
fn current(req: &ScanReq, generation: &AtomicU64) -> bool {
    generation.load(Ordering::Acquire) == req.generation
}

/// Hand a message over and wake the loop, reporting whether anyone is still there
/// to receive it.
fn send(res_tx: &Sender<ScanMsg>, wake: &OwnedFd, generation: u64, payload: Payload) -> bool {
    if res_tx
        .send(ScanMsg {
            generation,
            payload,
        })
        .is_err()
    {
        return false;
    }
    ffi::event_signal(wake.as_raw_fd());
    true
}

#[cfg(test)]
mod tests {
    use crate::order::Order;
    use crate::platform::ffi::PollSet;
    use crate::scan::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::time::Duration;

    /// A unique temp directory for one test, removed by the test that made it.
    fn temp_dir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "bnkfiles_scan_{tag}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).expect("make temp dir");
        base
    }

    /// The listing the sync path would produce: read, describe, sort. What the
    /// streamed-and-merged listing has to equal.
    fn sync_listing(dir: &Path) -> Vec<FsEntry> {
        let mut entries: Vec<FsEntry> = fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| files::describe(&e))
            .collect();
        entries.sort_by(|a, b| Order::default().cmp(a, b));
        entries
    }

    /// What one scan produced: the merged listing, and the terminal message.
    struct Scanned {
        listing: Vec<FsEntry>,
        done: Option<usize>,
        failed: Option<Unreadable>,
    }

    /// Drive a scan the way the event loop does — block in poll on the wake fd,
    /// drain, merge — until it reports a terminal message or the deadline passes.
    fn drive(scanner: &Scanner, generation: u64) -> Scanned {
        drive_in(scanner, generation, Order::default())
    }

    /// [`drive`], merging in `order`, the one the scan was asked for.
    fn drive_in(scanner: &Scanner, generation: u64, order: Order) -> Scanned {
        let mut out = Scanned {
            listing: Vec::new(),
            done: None,
            failed: None,
        };
        let mut poll = PollSet::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while out.done.is_none() && out.failed.is_none() && std::time::Instant::now() < deadline {
            poll.clear();
            poll.add(scanner.wake_fd());
            poll.wait(Some(Duration::from_millis(50))).expect("poll");
            scanner.drain_wake();
            while let Some(msg) = scanner.try_recv() {
                if msg.generation != generation {
                    continue;
                }
                match msg.payload {
                    // These tests run the scanner without a cache, so none arrive.
                    Payload::Cached(_) => {}
                    Payload::Batch(batch) => files::merge_sorted(&mut out.listing, batch, order),
                    Payload::Done { total } => out.done = Some(total),
                    Payload::Failed(e) => out.failed = Some(e),
                }
            }
        }
        out
    }

    #[test]
    fn a_streamed_scan_equals_the_listing_read_in_one_go() {
        // Enough entries to cross the batch boundary several times, so the merge is
        // actually exercised rather than one batch being handed straight over.
        let base = temp_dir("streamed");
        for i in 0..(BATCH * 2 + 37) {
            fs::write(base.join(format!("file_{i}.png")), b"x").expect("file");
        }
        for i in 0..8 {
            fs::create_dir_all(base.join(format!("dir_{i}"))).expect("dir");
        }
        let want = sync_listing(&base);

        let scanner = Scanner::new(None).expect("scanner");
        scanner.scan(base.clone(), 1, Order::default());
        let got = drive(&scanner, 1);

        assert_eq!(got.failed, None);
        assert_eq!(got.done, Some(want.len()), "Done reports every entry read");
        let (got_names, want_names): (Vec<&str>, Vec<&str>) = (
            got.listing.iter().map(|e| e.name.as_str()).collect(),
            want.iter().map(|e| e.name.as_str()).collect(),
        );
        assert_eq!(got_names, want_names);

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_scan_streams_in_the_order_it_was_asked_for() {
        // Across several batches, so each is sorted by the worker in that order
        // and the merge keeps it: nothing is sorted again on the receiving end.
        let base = temp_dir("ordered");
        for i in 0..(BATCH * 2 + 11) {
            let bytes = vec![b'x'; (i * 7919) % 1000];
            fs::write(base.join(format!("file_{i}")), bytes).expect("file");
        }
        fs::create_dir_all(base.join("a_dir")).expect("dir");
        let biggest_first = Order {
            key: crate::order::SortKey::Size,
            descending: true,
        };
        let scanner = Scanner::new(None).expect("scanner");
        scanner.scan(base.clone(), 1, biggest_first);
        let got = drive_in(&scanner, 1, biggest_first);

        assert_eq!(got.done, Some(BATCH * 2 + 12));
        assert!(got.listing[0].is_dir(), "the folder still leads");
        assert!(
            got.listing
                .windows(2)
                .all(|w| biggest_first.cmp(&w[0], &w[1]) != std::cmp::Ordering::Greater),
            "every neighbouring pair is in order"
        );
        assert!(got.listing[1].size >= got.listing[got.listing.len() - 1].size);

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn an_empty_directory_still_reports_done() {
        // The terminal message is what clears "still scanning"; a directory with
        // nothing in it produces no batches, so Done is the only thing that comes.
        let base = temp_dir("empty");
        let scanner = Scanner::new(None).expect("scanner");
        scanner.scan(base.clone(), 1, Order::default());
        let got = drive(&scanner, 1);
        assert_eq!(got.done, Some(0));
        assert!(got.listing.is_empty());
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_directory_that_is_not_there_fails_once() {
        let base = temp_dir("missing");
        let gone = base.join("no-such-directory");
        let scanner = Scanner::new(None).expect("scanner");
        scanner.scan(gone.clone(), 1, Order::default());
        let got = drive(&scanner, 1);

        let failed = got.failed.expect("the scan reports the failure");
        assert_eq!(failed.reason, Reason::Missing);
        assert!(!failed.partial);
        assert_eq!(
            failed.nearest.as_deref(),
            Some(base.as_path()),
            "and names the nearest folder that is there"
        );
        assert_eq!(got.done, None, "a failed scan does not also report Done");

        // And nothing else follows it: the failure is terminal.
        std::thread::sleep(Duration::from_millis(50));
        scanner.drain_wake();
        assert!(scanner.try_recv().is_none());

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_superseded_scan_is_abandoned_and_never_reports_done() {
        // Two directories, the first big enough that the scan is still running when
        // the second is asked for. The abandoned scan must not send Done: the main
        // thread keys "this directory has finished" off it.
        let big = temp_dir("superseded_big");
        for i in 0..(BATCH * 20) {
            fs::write(big.join(format!("f_{i:06}.png")), b"x").expect("file");
        }
        let small = temp_dir("superseded_small");
        fs::write(small.join("only.png"), b"x").expect("file");

        let scanner = Scanner::new(None).expect("scanner");
        scanner.scan(big.clone(), 1, Order::default());
        // Supersede it at once. Whether the first scan has started, is mid-read, or
        // (on a very fast machine) already finished, generation 1 is now stale.
        scanner.scan(small.clone(), 2, Order::default());

        let got = drive(&scanner, 2);
        assert_eq!(got.done, Some(1), "the current scan completes normally");
        assert_eq!(got.listing.len(), 1);

        // Drain whatever generation 1 managed to send. Batches are allowed (they
        // were in flight); a Done for it is not.
        scanner.drain_wake();
        while let Some(msg) = scanner.try_recv() {
            if msg.generation == 1 {
                assert!(
                    !matches!(msg.payload, Payload::Done { .. }),
                    "an abandoned scan reported Done"
                );
            }
        }

        fs::remove_dir_all(&big).expect("cleanup");
        fs::remove_dir_all(&small).expect("cleanup");
    }

    #[test]
    fn a_burst_of_changes_asks_for_exactly_one_rescan() {
        // The live-refresh path end to end, with real inotify and real files:
        // fifty creations in a directory being watched must coalesce into one
        // rescan, fired only after the directory has stopped moving.
        use crate::platform::inotify::Inotify;

        let base = temp_dir("debounce");
        let watch = Inotify::new().expect("inotify");
        watch.add_dir(&base).expect("watch the directory");

        let window = Duration::from_millis(60);
        let mut debounce = Debounce::new(window);
        let mut buf = vec![0u8; 8 * 1024];
        let mut rescans = 0;
        let mut events = 0;

        // The burst. Draining as it goes is what the event loop does.
        let started = Instant::now();
        for i in 0..50 {
            fs::write(base.join(format!("f_{i:03}.png")), b"x").expect("file");
            watch
                .for_each_event(&mut buf, |_| {
                    events += 1;
                    debounce.touch(Instant::now());
                })
                .expect("drain");
            if debounce.fired(Instant::now()) {
                rescans += 1;
            }
        }
        assert!(events > 0, "the watch reported nothing at all");
        assert_eq!(
            rescans, 0,
            "a rescan fired during the burst, before it had settled"
        );
        assert!(
            started.elapsed() < window,
            "the burst itself took longer than the window, so this proves nothing"
        );

        // Let it settle, draining the tail of the queue as the loop would.
        std::thread::sleep(window * 2);
        watch
            .for_each_event(&mut buf, |_| debounce.touch(Instant::now()))
            .expect("drain");
        std::thread::sleep(window * 2);
        if debounce.fired(Instant::now()) {
            rescans += 1;
        }
        assert_eq!(rescans, 1, "the settled burst asked for one rescan");
        // And it stays fired-once: nothing is owed until something else happens.
        assert!(!debounce.fired(Instant::now()));

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn the_debounce_waits_out_the_window_and_fires_once() {
        // The rule on its own, against a clock the test controls rather than the
        // scheduler's: every touch pushes the deadline out, and it fires exactly
        // once per settling.
        let window = Duration::from_millis(100);
        let mut d = Debounce::new(window);
        let t0 = Instant::now();
        assert!(!d.fired(t0), "nothing has happened yet");
        assert_eq!(d.deadline(), None);

        d.touch(t0);
        assert_eq!(d.deadline(), Some(t0 + window));
        assert!(!d.fired(t0 + window / 2), "still inside the window");

        // A second event pushes the deadline out rather than keeping the first.
        d.touch(t0 + window / 2);
        assert!(
            !d.fired(t0 + window),
            "the first deadline should have been replaced"
        );
        assert!(d.fired(t0 + window + window / 2));
        assert!(!d.fired(t0 + window * 10), "it fires once, not forever");
    }

    #[test]
    fn one_scanner_serves_one_directory_after_another() {
        // The worker outlives a request: a second scan on the same scanner reads the
        // second directory, not a cached first one.
        let a = temp_dir("serial_a");
        fs::write(a.join("a.png"), b"x").expect("file");
        let b = temp_dir("serial_b");
        fs::write(b.join("b1.png"), b"x").expect("file");
        fs::write(b.join("b2.png"), b"x").expect("file");

        let scanner = Scanner::new(None).expect("scanner");
        scanner.scan(a.clone(), 1, Order::default());
        assert_eq!(drive(&scanner, 1).done, Some(1));
        scanner.scan(b.clone(), 2, Order::default());
        let second = drive(&scanner, 2);
        assert_eq!(second.done, Some(2));
        assert_eq!(
            second
                .listing
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            vec!["b1.png", "b2.png"]
        );

        fs::remove_dir_all(&a).expect("cleanup");
        fs::remove_dir_all(&b).expect("cleanup");
    }

    #[test]
    fn a_listing_that_breaks_off_sends_what_it_read_and_then_fails() {
        // What a flaky network filesystem produces: entries, then an error. The
        // entries are real and are sent; the ending must be `Failed`, never a
        // `Done` that passes the short list off as the folder.
        let dir = temp_dir("broken");
        for name in ["a.txt", "b.txt", "c.txt"] {
            fs::write(dir.join(name), "x").expect("file");
        }
        let req = ScanReq {
            dir: dir.clone(),
            generation: 7,
            order: Order::default(),
        };
        let generation = AtomicU64::new(7);
        let wake = ffi::eventfd().expect("eventfd");
        let (res_tx, res_rx) = std::sync::mpsc::channel::<ScanMsg>();
        let entries = fs::read_dir(&dir)
            .expect("list")
            .chain(std::iter::once(Err(std::io::Error::from_raw_os_error(5))));

        stream(entries, &req, &res_tx, &generation, &wake);
        drop(res_tx);

        let mut listed = 0;
        let mut endings = Vec::new();
        for msg in res_rx.iter() {
            match msg.payload {
                Payload::Cached(_) => {}
                Payload::Batch(batch) => listed += batch.len(),
                Payload::Done { .. } => endings.push(None),
                Payload::Failed(why) => endings.push(Some(why)),
            }
        }
        assert_eq!(listed, 3, "the entries read before the error still arrive");
        assert_eq!(endings.len(), 1, "exactly one terminal message");
        let Some(Some(failure)) = endings.pop() else {
            panic!("it is a failure, not Done");
        };
        assert!(failure.partial, "and says entries are missing: {failure:?}");
        assert_eq!(failure.reason, Reason::Failed, "EIO");

        fs::remove_dir_all(&dir).expect("cleanup");
    }
    #[test]
    fn the_nearest_folder_is_found_past_a_parent_that_is_gone_or_a_file() {
        let base = temp_dir("missing_deeper");
        let scanner = Scanner::new(None).expect("scanner");
        scanner.scan(base.join("gone/deeper"), 1, Order::default());
        let failure = drive(&scanner, 1).failed.expect("a failure");
        assert_eq!(failure.reason, Reason::Missing);
        assert_eq!(
            failure.nearest.as_deref(),
            Some(base.as_path()),
            "two levels up, past the one that is gone too"
        );

        // A file where the folder should be.
        fs::write(base.join("file"), "x").expect("file");
        scanner.scan(base.join("file"), 2, Order::default());
        let failure = drive(&scanner, 2).failed.expect("a failure");
        assert_eq!(failure.reason, Reason::NotFolder);
        assert_eq!(failure.nearest.as_deref(), Some(base.as_path()));

        // A file where a folder above it should be: it is that one that is
        // not a folder, and this one is simply not there.
        scanner.scan(base.join("file/inside"), 3, Order::default());
        let failure = drive(&scanner, 3).failed.expect("a failure");
        assert_eq!(failure.reason, Reason::Missing);
        assert_eq!(failure.nearest.as_deref(), Some(base.as_path()));

        // A folder that is there and cannot be read names no nearest one:
        // finding it is only worth the stats for a folder that is not there.
        let locked = base.join("locked");
        fs::create_dir(&locked).expect("dir");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod");
        scanner.scan(locked.clone(), 4, Order::default());
        let failure = drive(&scanner, 4).failed;
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("chmod back");
        // Root reads anything, so the denial is only there for anyone else.
        if let Some(failure) = failure {
            assert_eq!(failure.reason, Reason::Denied);
            assert_eq!(failure.nearest, None);
        }

        fs::remove_dir_all(&base).expect("cleanup");
    }
}
