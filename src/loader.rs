//! Background image decoding: a bounded, prioritized worker pool.
//!
//! Decoding a large photo takes tens to hundreds of milliseconds; doing it on the
//! event-loop thread stalls input. So a pool of worker threads owns the decoding.
//! The main thread queues a [`Req`] (carrying the store key, the priority class, and
//! the folder generation); the workers pull the highest-priority runnable request,
//! decode it, and send the result back; an eventfd wakes the main loop's poll so the
//! result is picked up promptly.
//!
//! Three properties the pool guarantees:
//!
//! - **Terminal outcomes.** Every accepted request resolves to exactly one
//!   [`Outcome`] — `Decoded`, `Failed`, or `Skipped` (cancelled before decode) —
//!   never a silent drop. The main thread clears its in-flight set on the response,
//!   so a missing response would strand the key forever; the response always comes.
//! - **Priority, not insertion order.** A [`Class`] orders the queue, so the image
//!   the viewer is waiting on and the thumbnails on screen decode before off-screen
//!   margin work, even when the margin work was queued first.
//! - **Bounded memory.** A job reserves decode memory sized to the image's header
//!   dimensions *before* allocating the pixels, so N workers cannot each allocate a
//!   gigabyte at once. A single image larger than the whole budget runs alone rather
//!   than deadlocking.
//!
//! The keys carry through request → response, so a stale-size thumbnail result can
//! never land under the current key, and the main thread never has to recompute a
//! key from a `resp.index` whose meaning may have moved.

use std::collections::BinaryHeap;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use crate::platform::error::{Error, Result};
use crate::platform::ffi;
use crate::platform::pixel;
use crate::render::image::{self, Image, ImageKey, Limits};

/// Total decode-time memory the pool will commit at once, across all workers. This
/// bounds the *transient* peak while images are being decoded; the resident decoded
/// images are bounded separately by the store's eviction. Generous enough that
/// ordinary photos run fully parallel, tight enough that a folder of 24 MP images
/// (~96 MB each) cannot have every worker allocating at once.
const DECODE_MEMORY_BUDGET: usize = 512 * 1024 * 1024;

/// What to produce for a request: the full image (viewer) or a downscaled thumbnail
/// (grid).
#[derive(Clone, Copy)]
pub enum Job {
    Full,
    Thumb { size: u32 },
}

/// Scheduling priority. Ordered most-urgent first: the full image the viewer is
/// waiting on, then the thumbnails currently on screen, then the neighbours being
/// preloaded, then the off-screen margin. The queue pops in this order regardless of
/// when a request was made, so a fast scroll cannot leave the visible rows behind a
/// backlog of margin work.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Class {
    Target,
    Visible,
    Preload,
    Margin,
}

/// A decode request from the main thread. `key` is the store key the result belongs
/// under; it rides
/// through to the response so a stale result cannot be mis-keyed.
pub struct Req {
    pub key: ImageKey,
    pub index: usize,
    pub path: PathBuf,
    pub job: Job,
    pub generation: u64,
    pub class: Class,
}

/// How a request resolved. Always exactly one per accepted request.
pub enum Outcome {
    Decoded(Image),
    Failed(String),
    /// Cancelled before decoding: the request's generation was already stale when a
    /// worker reached it (the user had left the folder).
    Skipped,
}

/// A finished request, carrying back the key it was made under and the generation it
/// was made for, so the main thread can key the result and reject a stale one.
///
/// No index rides back. The request's index is a scheduling input (how far the
/// picture is from what the user is looking at); by the time the result lands, a
/// directory scan may have renumbered the gallery underneath it, and the key is the
/// only thing that still names the same file.
pub struct Response {
    pub key: ImageKey,
    pub generation: u64,
    pub job: Job,
    pub outcome: Outcome,
}

/// One queued request, its distance from the current focus (the index the user is
/// looking at), and a sequence number. The three order the heap: class first, then
/// nearest-to-focus, then oldest. `dist` is re-scored when the focus moves
/// ([`Loader::set_focus`]), which is what keeps a thumbnail scrolled off screen from
/// out-ranking one scrolled into view just because it was queued earlier.
struct Pending {
    req: Req,
    dist: u32,
    seq: u64,
}

impl PartialEq for Pending {
    fn eq(&self, other: &Self) -> bool {
        self.req.class == other.req.class && self.dist == other.dist && self.seq == other.seq
    }
}
impl Eq for Pending {}
impl Ord for Pending {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // BinaryHeap is a max-heap, so reverse every field: the smallest class (most
        // urgent), then the smallest distance (nearest the focus), then the smallest
        // seq (oldest) must compare as *greatest* to pop first.
        other
            .req
            .class
            .cmp(&self.req.class)
            .then_with(|| other.dist.cmp(&self.dist))
            .then_with(|| other.seq.cmp(&self.seq))
    }
}
impl PartialOrd for Pending {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// The distance of image `index` from `focus`, saturating to `u32`.
fn distance(index: usize, focus: usize) -> u32 {
    index.abs_diff(focus).min(u32::MAX as usize) as u32
}

/// The state every worker and the main thread share, behind one lock. The two waits
/// (for a request to run, and for decode memory to free up) get their own condvars:
/// one shared condvar with `notify_one` could wake a memory-waiter when a request
/// arrives, and it would re-sleep without picking the request up — a lost wakeup.
/// Separate condvars let each notify reach only the waiters that can act on it.
struct Shared {
    inner: Mutex<Inner>,
    /// A request was queued (or the generation advanced, or shutdown): wake workers
    /// waiting to run something.
    work: Condvar,
    /// Decode memory was released (or shutdown): wake workers waiting for room.
    memory: Condvar,
    res_tx: Sender<Response>,
    wake: OwnedFd,
}

struct Inner {
    heap: BinaryHeap<Pending>,
    /// The current folder generation; a request queued for an older one is cancelled
    /// (resolved `Skipped`) when a worker reaches it.
    generation: u64,
    /// The image index the user is looking at; queued requests are ordered by their
    /// distance from it, so the nearest run first.
    focus: usize,
    /// Decode memory not yet reserved, in bytes.
    mem_free: usize,
    /// Monotonic tiebreak for the heap.
    seq: u64,
    shutdown: bool,
}

pub struct Loader {
    shared: Arc<Shared>,
    res_rx: Receiver<Response>,
    /// A clone of the wake eventfd for the main loop's poll set.
    wake: OwnedFd,
    _workers: Vec<JoinHandle<()>>,
}

impl Loader {
    pub fn new() -> Result<Self> {
        let wake = ffi::eventfd()?;
        let poll_wake = wake
            .try_clone()
            .map_err(|e| Error::msg(format!("could not dup the decode wake fd: {e}")))?;
        let (res_tx, res_rx) = std::sync::mpsc::channel::<Response>();
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                heap: BinaryHeap::new(),
                generation: 0,
                focus: 0,
                mem_free: DECODE_MEMORY_BUDGET,
                seq: 0,
                shutdown: false,
            }),
            work: Condvar::new(),
            memory: Condvar::new(),
            res_tx,
            wake,
        });

        // One worker per core bar one for the main thread, at least one. More workers
        // than cores only adds contention; fewer than one never decodes.
        let workers = std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(1).max(1))
            .unwrap_or(1);
        let handles = (0..workers)
            .map(|_| {
                let shared = Arc::clone(&shared);
                std::thread::spawn(move || worker_loop(shared))
            })
            .collect();

        Ok(Self {
            shared,
            res_rx,
            wake: poll_wake,
            _workers: handles,
        })
    }

    /// The eventfd to add to the main loop's poll set.
    pub fn wake_fd(&self) -> RawFd {
        self.wake.as_raw_fd()
    }

    /// Queue a decode. Dropped silently only if every worker has already gone.
    pub fn request(&self, req: Req) {
        let Ok(mut inner) = self.shared.inner.lock() else {
            return;
        };
        let seq = inner.seq;
        inner.seq += 1;
        let dist = distance(req.index, inner.focus);
        inner.heap.push(Pending { req, dist, seq });
        drop(inner);
        self.shared.work.notify_one();
    }

    /// Advance the current generation (a new folder). Requests still queued for an
    /// older generation are cancelled when a worker reaches them.
    pub fn set_generation(&self, generation: u64) {
        if let Ok(mut inner) = self.shared.inner.lock() {
            inner.generation = generation;
        }
        // Wake workers so a queue full of now-stale requests is drained to `Skipped`
        // promptly rather than only when the next request arrives.
        self.shared.work.notify_all();
    }

    /// Move the focus (the index the user is looking at) and re-score every queued
    /// request's distance from it, so a scroll or a navigation re-prioritizes work
    /// already in the queue rather than only work queued afterwards. A no-op when the
    /// focus has not actually moved. No wake: this only reorders the pending set, it
    /// adds no runnable work.
    pub fn set_focus(&self, focus: usize) {
        let Ok(mut inner) = self.shared.inner.lock() else {
            return;
        };
        if inner.focus == focus {
            return;
        }
        inner.focus = focus;
        let rescored: Vec<Pending> = inner
            .heap
            .drain()
            .map(|mut p| {
                p.dist = distance(p.req.index, focus);
                p
            })
            .collect();
        inner.heap = BinaryHeap::from(rescored);
    }

    /// Take the next finished decode, if any. Non-blocking.
    pub fn try_recv(&self) -> Option<Response> {
        self.res_rx.try_recv().ok()
    }

    /// Reset the wake fd after servicing results, so a poll does not re-fire on
    /// notifications already dealt with. Call before draining `try_recv`.
    pub fn drain_wake(&self) {
        ffi::event_drain(self.wake.as_raw_fd());
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        // Signal shutdown and wake every worker. The threads are not joined: a worker
        // mid-decode finishes it (harmlessly, the receiver is gone) and then exits on
        // seeing the flag, and the `Arc<Shared>` keeps their shared state alive until
        // the last one does. A short-lived `Loader` (the perf lab) has idle workers
        // that exit at once.
        if let Ok(mut inner) = self.shared.inner.lock() {
            inner.shutdown = true;
        }
        self.shared.work.notify_all();
        self.shared.memory.notify_all();
    }
}

fn worker_loop(shared: Arc<Shared>) {
    while let Some(req) = next_request(&shared) {
        let resp = decode_request(&shared, req);
        // Best-effort: if the main thread is gone the send fails and we simply stop
        // signalling; the loop ends on the next `next_request` seeing shutdown.
        if shared.res_tx.send(resp).is_err() {
            return;
        }
        ffi::event_signal(shared.wake.as_raw_fd());
    }
}

/// Pop the highest-priority runnable request, resolving stale ones to `Skipped`
/// along the way, or `None` on shutdown.
fn next_request(shared: &Shared) -> Option<Req> {
    let mut inner = shared.inner.lock().ok()?;
    loop {
        if inner.shutdown {
            return None;
        }
        match inner.heap.pop() {
            Some(pending) => {
                if pending.req.generation < inner.generation {
                    // The user left this folder before we reached the request. Resolve
                    // it terminally so the main thread's in-flight set clears.
                    let resp = Response {
                        key: pending.req.key,
                        generation: pending.req.generation,
                        job: pending.req.job,
                        outcome: Outcome::Skipped,
                    };
                    drop(inner);
                    let _ = shared.res_tx.send(resp);
                    ffi::event_signal(shared.wake.as_raw_fd());
                    inner = shared.inner.lock().ok()?;
                    continue;
                }
                return Some(pending.req);
            }
            None => {
                inner = shared.work.wait(inner).ok()?;
            }
        }
    }
}

/// Decode one request under a memory reservation sized to what decoding it will
/// hold at its peak.
fn decode_request(shared: &Shared, req: Req) -> Response {
    // `read_encoded` is what stands between a directory listing and the pool: it
    // cannot block on a FIFO, and refuses a device or an oversized file before
    // reading it, so a hostile entry costs one failed request rather than a
    // worker.
    let outcome = match image::read_encoded(&req.path) {
        Ok(bytes) => {
            // Reserve before allocating. `decode_peak` reads only the header and
            // answers for the whole decode, intermediates included, not just the
            // finished pixels. The reserve is clamped to the whole budget so an
            // image larger than the budget runs alone instead of waiting forever
            // for room that never comes.
            let reserve = Image::decode_peak(&bytes)
                .unwrap_or(bytes.len())
                .clamp(1, DECODE_MEMORY_BUDGET);
            acquire_memory(shared, reserve);
            let decoded = Image::decode(&bytes, Limits::default());
            let outcome = match decoded {
                Ok(image) => match req.job {
                    Job::Full => Outcome::Decoded(image),
                    Job::Thumb { size } => Outcome::Decoded(thumbnail(&image, size)),
                },
                Err(e) => Outcome::Failed(e.to_string()),
            };
            release_memory(shared, reserve);
            outcome
        }
        Err(e) => Outcome::Failed(format!(
            "could not read {}: {e}",
            req.path
                .file_name()
                .unwrap_or(req.path.as_os_str())
                .to_string_lossy()
        )),
    };
    Response {
        key: req.key,
        generation: req.generation,
        job: req.job,
        outcome,
    }
}

/// Wait until `n` bytes of decode budget are free, then reserve them. On shutdown it
/// stops waiting and proceeds (the process is tearing down; oversubscribing briefly
/// is harmless and never deadlocks).
fn acquire_memory(shared: &Shared, n: usize) {
    let Ok(mut inner) = shared.inner.lock() else {
        return;
    };
    while inner.mem_free < n && !inner.shutdown {
        inner = match shared.memory.wait(inner) {
            Ok(g) => g,
            Err(_) => return,
        };
    }
    inner.mem_free = inner.mem_free.saturating_sub(n);
}

fn release_memory(shared: &Shared, n: usize) {
    if let Ok(mut inner) = shared.inner.lock() {
        inner.mem_free = (inner.mem_free + n).min(DECODE_MEMORY_BUDGET);
    }
    // A freed reservation may let workers waiting for room proceed. Only memory
    // waiters are woken (not idle request waiters), so an ordinary decode finishing
    // does not stampede the whole pool awake.
    shared.memory.notify_all();
}

/// A copy of `image` fitted inside `size` x `size`, never upscaled.
///
/// The resample is the leaf's area filter, which weighs every source pixel that
/// falls under a destination one. A bilinear tap reads four samples wherever it
/// lands, so shrinking a 4000px photo to a 160px cell would skip most of it and
/// alias what it kept: thumbnails would shimmer and sparkle where the original has
/// fine detail.
fn thumbnail(image: &Image, size: u32) -> Image {
    let scale = (size as f32 / image.width as f32)
        .min(size as f32 / image.height as f32)
        .min(1.0);
    let w = ((image.width as f32 * scale) as u32).max(1);
    let h = ((image.height as f32 * scale) as u32).max(1);
    Image {
        pixels: pixel::resample_argb(&image.pixels, image.width, image.height, w as i32, h as i32),
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use crate::loader::*;
    use crate::platform::ffi::PollSet;
    use std::os::unix::ffi::OsStrExt;
    use std::time::{Duration, Instant};

    extern "C" {
        fn mkfifo(path: *const std::ffi::c_char, mode: u32) -> std::ffi::c_int;
    }

    #[test]
    fn a_pipe_named_like_a_picture_fails_its_request_and_frees_the_worker() {
        // A worker that opens a FIFO for reading blocks until a writer comes, so
        // one `mkfifo x.jpg` per worker would end decoding for the session. Queue
        // more pipes than there are workers, then a real picture behind them. It
        // only decodes if no worker was lost on the way.
        let dir = std::env::temp_dir().join(format!(
            "bnkfiles_loader_fifo_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("make temp dir");
        let pipe = dir.join("x.jpg");
        let c = std::ffi::CString::new(pipe.as_os_str().as_bytes()).expect("path");
        // SAFETY: `c` is a NUL-terminated path that outlives the call.
        assert_eq!(unsafe { mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo");
        let picture = dir.join("real.png");
        std::fs::write(&picture, include_bytes!("render/testdata/tiny.png")).expect("file");

        let loader = Loader::new().expect("loader");
        let pipes = 2 * std::thread::available_parallelism().map_or(1, |n| n.get());
        let request = |path: &PathBuf| Req {
            key: ImageKey::full(path),
            index: 0,
            path: path.clone(),
            job: Job::Full,
            generation: 0,
            class: Class::Target,
        };
        for _ in 0..pipes {
            loader.request(request(&pipe));
        }
        loader.request(request(&picture));

        let mut poll = PollSet::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        let (mut refused, mut decoded) = (0, false);
        while (refused < pipes || !decoded) && Instant::now() < deadline {
            poll.clear();
            poll.add(loader.wake_fd());
            poll.wait(Some(Duration::from_millis(20))).expect("poll");
            loader.drain_wake();
            while let Some(resp) = loader.try_recv() {
                match (resp.key.path == pipe, resp.outcome) {
                    (true, Outcome::Failed(why)) => {
                        assert!(why.contains("not a regular file"), "{why}");
                        refused += 1;
                    }
                    (false, Outcome::Decoded(image)) => {
                        assert_eq!((image.width, image.height), (2, 2));
                        decoded = true;
                    }
                    _ => panic!("unexpected outcome for {:?}", resp.key),
                }
            }
        }
        assert_eq!(refused, pipes, "every pipe was refused, none waited on");
        assert!(decoded, "and the workers were all still there to decode");

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    fn pending(class: Class, index: usize, focus: usize, seq: u64) -> Pending {
        Pending {
            req: Req {
                key: ImageKey::full(PathBuf::new()),
                index,
                path: PathBuf::new(),
                job: Job::Full,
                generation: 0,
                class,
            },
            dist: distance(index, focus),
            seq,
        }
    }

    #[test]
    fn the_queue_pops_by_class_then_distance_then_fifo() {
        // Class dominates; within a class the nearest to the focus wins; equal
        // distances fall back to oldest-first.
        let focus = 10;
        let mut heap = BinaryHeap::new();
        heap.push(pending(Class::Margin, 0, focus, 0));
        heap.push(pending(Class::Visible, 30, focus, 1)); // dist 20
        heap.push(pending(Class::Target, 99, focus, 2));
        heap.push(pending(Class::Visible, 12, focus, 3)); // dist 2
        heap.push(pending(Class::Visible, 8, focus, 4)); // dist 2, but newer

        let order: Vec<_> =
            std::iter::from_fn(|| heap.pop().map(|p| (p.req.class, p.req.index))).collect();
        assert_eq!(
            order,
            vec![
                (Class::Target, 99),
                (Class::Visible, 12), // dist 2, older (seq 3) before the equal-dist seq 4
                (Class::Visible, 8),  // dist 2, newer
                (Class::Visible, 30), // dist 20, farthest visible last
                (Class::Margin, 0),
            ]
        );
    }

    #[test]
    fn re_scoring_the_distance_reorders_within_a_class() {
        // Two equally-far visible thumbnails; moving the focus onto one must make it
        // out-rank the other. Work already queued is re-prioritized, not only work
        // queued afterwards, so a scroll cannot starve the visible rows.
        let mut heap = BinaryHeap::new();
        heap.push(pending(Class::Visible, 5, 10, 0));
        heap.push(pending(Class::Visible, 40, 10, 1));
        let items: Vec<Pending> = heap
            .drain()
            .map(|mut p| {
                p.dist = distance(p.req.index, 40);
                p
            })
            .collect();
        heap = BinaryHeap::from(items);
        assert_eq!(heap.pop().map(|p| p.req.index), Some(40));
        assert_eq!(heap.pop().map(|p| p.req.index), Some(5));
    }
}
