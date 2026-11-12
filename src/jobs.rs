//! File operations as cancellable, progress-reporting jobs.
//!
//! Copying a folder of photographs is seconds to minutes of work; doing it on the
//! event-loop thread would freeze the window for the duration. So each operation
//! runs on its own worker thread and reports back over a channel plus an eventfd,
//! the way [`crate::loader`] owns decoding and [`crate::scan`] owns directory
//! reading. The main thread starts jobs, draws their progress, answers their
//! questions, and can cancel them; it never moves a byte itself.
//!
//! Three properties, the first two inherited from the loader's discipline:
//!
//! - **Terminal outcomes.** Every started job resolves to exactly one
//!   [`JobPayload::Done`], [`JobPayload::Failed`], or [`JobPayload::Cancelled`],
//!   so the main thread's notion of "a job is running" can never strand. The one
//!   place a worker can block indefinitely is a conflict prompt, and that unblocks
//!   when the reply channel is dropped.
//! - **Bounded queue.** Progress is chatty and lossy by nature: it goes over a
//!   bounded channel with a non-blocking send, and a full queue drops the update
//!   rather than stalling the copy. Terminal messages and conflict questions block,
//!   because losing one would strand something. Drops are counted, because a
//!   non-zero count means the loop is not draining and that is a bug signal, not a
//!   number to tune away.
//! - **No torn destinations.** A file is written under a hidden `.bnkfiles-part-*`
//!   name beside its destination and renamed into place only once it is whole. A
//!   job that is cancelled, fails, or dies with the process leaves the part file
//!   behind at worst, never a truncated file under the real name, and whatever
//!   was under that name before is still there. Nothing at a destination is
//!   removed until its replacement has arrived.
//!
//! ```text
//!   submit(Request) ──▶ queue ──▶ worker thread ──▶ Progress, Progress, ... Done
//!        │                            │
//!        │                            └─ Conflict ──▶ blocks on the reply channel
//!        └── cancel() ─────────────▶ flag, checked between files and mid-copy
//! ```
//!
//! One job runs at a time and the rest wait. Two copies on one disk finish no
//! sooner than one after the other, and a single progress line is something a user
//! can actually read.

use std::collections::{HashSet, VecDeque};
use std::ffi::OsString;
use std::fs::{self, File, Metadata};
use std::io::ErrorKind;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::files;
use crate::platform::error::{Error, Result};
use crate::platform::ffi;
use crate::trash;

pub(crate) mod sys;

/// Messages in flight from the worker. Deep enough that a burst of progress across
/// many small files rides out a frame, shallow enough that a main thread which has
/// stopped draining is noticed rather than buffering megabytes of stale updates.
const QUEUE_DEPTH: usize = 64;

/// How long shutdown waits for a message before looking again at whether the
/// worker has ended.
const SHUTDOWN_POLL: Duration = Duration::from_millis(20);

/// How often a running job reports progress. A copy moves bytes far faster than a
/// display can show them changing, and a message per block would be most of the
/// work; this is under a frame at 60 Hz, so the bar still looks continuous.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(50);

/// Bytes per `copy_file_range` call. Large enough that the syscall overhead
/// disappears against the copy, small enough that cancelling is felt promptly and
/// progress moves smoothly on a slow disk.
const CHUNK: usize = 64 * 1024 * 1024;

/// The read/write fallback's buffer.
const RW_BUF: usize = 128 * 1024;

/// What the hidden name of a destination still being written starts with. The
/// rest is the process, the job and a counter: short and of a fixed shape, so a
/// source whose own name is already at the filesystem's length limit can still be
/// copied, which a part name built from the destination's name could not manage.
const PART_PREFIX: &str = ".bnkfiles-part-";

/// What the hidden name of an entry that has stepped aside for its replacement
/// starts with. Told apart from a part file on purpose: a part file left behind
/// is debris, and one of these is somebody's data.
const ASIDE_PREFIX: &str = ".bnkfiles-replaced-";

/// A part file is the owner's alone until the copy is finished and it is given
/// the source's mode. Made with the default mode instead, a private file would
/// sit readable by others for as long as the copy took.
const PART_MODE: u32 = 0o600;

/// The permission bits of a mode, without the file type above them.
const MODE_BITS: u32 = 0o7777;

/// The attribute namespace ordinary users own, and the one collections keep
/// their metadata in. Losing one of these in a move is losing user data.
const USER_XATTR_PREFIX: &[u8] = b"user.";

/// What an operation is, for the status line. The work itself is described by
/// [`Request`], which carries the paths each kind needs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JobKind {
    Copy,
    Move,
    Rename,
    Trash,
    Delete,
}

impl JobKind {
    /// The verb the status line shows while the job runs.
    pub fn verb(self) -> &'static str {
        match self {
            JobKind::Copy => "copying",
            JobKind::Move => "moving",
            JobKind::Rename => "renaming",
            JobKind::Trash => "trashing",
            JobKind::Delete => "deleting",
        }
    }

    /// The verb for saying it happened, once it has.
    pub fn past(self) -> &'static str {
        match self {
            JobKind::Copy => "copied",
            JobKind::Move => "moved",
            JobKind::Rename => "renamed",
            JobKind::Trash => "trashed",
            JobKind::Delete => "deleted",
        }
    }
}

/// Whether jobs may change anything on disk, decided once when [`Jobs`] is built.
/// A blocked `Jobs` refuses every request before it is queued, so nothing that
/// goes through it can rename, trash, delete, copy, or move a file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Writes {
    Allowed,
    Blocked,
}

/// A piece of work to do. An enum rather than a struct of optional fields, so a
/// rename cannot be constructed without a name and a copy cannot be constructed
/// without somewhere to copy to.
pub enum Request {
    // Nothing in the app submits Copy or Move: a paste or a drop would, and
    // neither is wired to this queue.
    #[allow(dead_code)]
    Copy {
        sources: Vec<PathBuf>,
        dest_dir: PathBuf,
    },
    #[allow(dead_code)]
    Move {
        sources: Vec<PathBuf>,
        dest_dir: PathBuf,
    },
    Rename {
        path: PathBuf,
        name: String,
    },
    Trash {
        paths: Vec<PathBuf>,
    },
    Delete {
        paths: Vec<PathBuf>,
    },
}

impl Request {
    pub fn kind(&self) -> JobKind {
        match self {
            Request::Copy { .. } => JobKind::Copy,
            Request::Move { .. } => JobKind::Move,
            Request::Rename { .. } => JobKind::Rename,
            Request::Trash { .. } => JobKind::Trash,
            Request::Delete { .. } => JobKind::Delete,
        }
    }
}

/// One message from a worker, tagged with the job it belongs to so a message from
/// a job that has just been cancelled cannot be drawn as the next one's progress.
pub struct JobMsg {
    pub id: u64,
    pub kind: JobKind,
    pub payload: JobPayload,
}

pub enum JobPayload {
    /// How far along the job is. `total_bytes` is zero while the tree is still
    /// being measured, which is the status line's cue to say so rather than to
    /// divide by it.
    Progress {
        done_bytes: u64,
        total_bytes: u64,
        current: String,
    },
    /// Something is already at this path. The worker is blocked on the reply
    /// channel until [`Jobs::answer`] says what to do about it.
    Conflict { path: PathBuf },
    /// Terminal: the work finished, having got through `items` of the sources it
    /// was given. Fewer than were asked for means the rest are described by the
    /// failure that comes with them.
    Done { items: usize },
    /// Terminal: the work stopped on an error, which is described here.
    Failed(String),
    /// Terminal: the user cancelled, or the window went away mid-prompt.
    Cancelled,
}

/// What the event loop hears about: the questions a job needs answering and the
/// way it ended.
///
/// Deliberately narrower than [`JobPayload`], which is what travels on the wire.
/// Progress is folded into the running job's state as it goes past and read back
/// through [`Jobs::progress`], so a caller matching on this has no impossible case
/// to write an arm for.
pub enum JobEvent {
    /// Something is already at this path, and the worker is blocked until
    /// [`Jobs::answer`] says what to do about it.
    Conflict { path: PathBuf },
    /// The work finished, having got through `items` of the sources it was given.
    Done { items: usize },
    /// It stopped on an error, described here.
    Failed(String),
    /// The user cancelled, or the window went away mid-prompt.
    Cancelled,
}

/// What to do about a destination that already exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConflictReply {
    Skip,
    /// Replace what is there with the incoming entry. Two directories are never
    /// in conflict to begin with: they merge, and it is their contents that are
    /// asked about one by one.
    Overwrite,
    /// Keep both: the incoming file lands under a free name beside the old one.
    RenameNew,
    /// Do this for every remaining conflict in the job, without asking again.
    ApplyToAll(Box<ConflictReply>),
}

/// A conflict reply with the "and for all the rest" wrapping taken off: the one
/// thing to do about the one destination in hand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decision {
    Skip,
    Overwrite,
    KeepBoth,
}

/// Whether a reply was only for the conflict it answered or stands for the rest
/// of the job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    ThisOne,
    AllRemaining,
}

impl ConflictReply {
    /// What the reply says to do, and how far it reaches. However deeply an
    /// `ApplyToAll` is nested, it means the same thing.
    fn settle(self) -> (Decision, Scope) {
        let mut reply = self;
        let mut scope = Scope::ThisOne;
        loop {
            match reply {
                ConflictReply::Skip => return (Decision::Skip, scope),
                ConflictReply::Overwrite => return (Decision::Overwrite, scope),
                ConflictReply::RenameNew => return (Decision::KeepBoth, scope),
                ConflictReply::ApplyToAll(inner) => {
                    scope = Scope::AllRemaining;
                    reply = *inner;
                }
            }
        }
    }
}

/// The running job, as the main thread sees it.
struct Active {
    id: u64,
    kind: JobKind,
    cancel: Arc<AtomicBool>,
    /// How a conflict prompt is answered. Taken on cancel, so a worker blocked on
    /// it sees the channel close and unwinds instead of waiting for an answer that
    /// is never coming.
    reply_tx: Option<Sender<ConflictReply>>,
    thread: JoinHandle<()>,
    done_bytes: u64,
    total_bytes: u64,
    current: String,
    /// The path a conflict prompt is up for, while one is.
    conflict: Option<PathBuf>,
}

/// A snapshot of the running job for the status line.
pub struct Progress<'a> {
    pub kind: JobKind,
    pub done_bytes: u64,
    pub total_bytes: u64,
    pub current: &'a str,
    pub queued: usize,
}

impl Progress<'_> {
    /// Completion in percent, or `None` while the tree is still being measured.
    pub fn percent(&self) -> Option<u32> {
        (self.total_bytes > 0).then(|| {
            ((self.done_bytes.min(self.total_bytes) as f64 / self.total_bytes as f64) * 100.0)
                as u32
        })
    }
}

pub struct Jobs {
    /// A clone of the wake eventfd for the main loop's poll set.
    wake: OwnedFd,
    /// Handed to each worker, kept here so the channel outlives any one of them.
    tx: SyncSender<JobMsg>,
    rx: Receiver<JobMsg>,
    worker_wake: OwnedFd,
    queue: VecDeque<Request>,
    active: Option<Active>,
    next_id: u64,
    /// Progress updates dropped because the queue was full. Non-zero means the
    /// loop stopped draining; it is shown in the debug overlay rather than being
    /// silently absorbed.
    pub dropped: u64,
    /// Whether progress has moved since the caller last asked. The status line
    /// repaints off this rather than on every turn of the loop, so an idle window
    /// with no job running stays idle.
    moved: bool,
    /// Checked before anything is queued, so a refused request never reaches a
    /// worker.
    writes: Writes,
}

impl Jobs {
    pub fn new(writes: Writes) -> Result<Self> {
        let wake = ffi::eventfd()?;
        let worker_wake = wake
            .try_clone()
            .map_err(|e| Error::msg(format!("could not dup the job wake fd: {e}")))?;
        let (tx, rx) = std::sync::mpsc::sync_channel::<JobMsg>(QUEUE_DEPTH);
        Ok(Self {
            wake,
            tx,
            rx,
            worker_wake,
            queue: VecDeque::new(),
            active: None,
            next_id: 1,
            dropped: 0,
            moved: false,
            writes,
        })
    }

    /// The eventfd to add to the main loop's poll set.
    pub fn wake_fd(&self) -> RawFd {
        self.wake.as_raw_fd()
    }

    /// Whether a job of `kind` would be accepted. A caller about to prompt for a
    /// name or a confirmation asks first, rather than collecting an answer that
    /// cannot be acted on.
    pub fn permit(&self, kind: JobKind) -> Result<()> {
        match self.writes {
            Writes::Allowed => Ok(()),
            Writes::Blocked => Err(Error::msg(format!(
                "read-only: {} is turned off",
                kind.verb()
            ))),
        }
    }

    /// Queue a piece of work, starting it if nothing else is running. Refused, with
    /// nothing queued, when writes are blocked.
    pub fn submit(&mut self, req: Request) -> Result<()> {
        self.permit(req.kind())?;
        self.queue.push_back(req);
        self.start_next();
        Ok(())
    }

    /// Whether anything is running or waiting to.
    pub fn is_busy(&self) -> bool {
        self.active.is_some() || !self.queue.is_empty()
    }

    /// The running job, for the status line.
    pub fn progress(&self) -> Option<Progress<'_>> {
        let active = self.active.as_ref()?;
        Some(Progress {
            kind: active.kind,
            done_bytes: active.done_bytes,
            total_bytes: active.total_bytes,
            current: &active.current,
            queued: self.queue.len(),
        })
    }

    /// Whether progress has moved since this was last asked, clearing it.
    pub fn took_progress(&mut self) -> bool {
        std::mem::take(&mut self.moved)
    }

    /// Answer the conflict prompt that is up, releasing the blocked worker.
    pub fn answer(&mut self, reply: ConflictReply) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        active.conflict = None;
        if let Some(tx) = active.reply_tx.as_ref() {
            // A send that fails means the worker has already gone, which the
            // terminal message on its way here will account for.
            let _ = tx.send(reply);
        }
    }

    /// Stop the running job and drop everything queued behind it.
    ///
    /// The flag is what a working job notices; dropping the reply channel is what
    /// a job blocked on a conflict prompt notices. Both resolve to a terminal
    /// `Cancelled`, so the main thread still hears the job end.
    pub fn cancel(&mut self) {
        self.queue.clear();
        let Some(active) = self.active.as_mut() else {
            return;
        };
        active.cancel.store(true, Ordering::Relaxed);
        active.reply_tx = None;
        active.conflict = None;
    }

    /// Reset the wake fd before draining, so a poll does not re-fire on
    /// notifications already dealt with.
    pub fn drain_wake(&self) {
        ffi::event_drain(self.wake.as_raw_fd());
    }

    /// Take the next event, folding progress into the running job's state and
    /// starting the next job when this one ends.
    pub fn try_recv(&mut self) -> Option<(JobKind, JobEvent)> {
        loop {
            let msg = self.rx.try_recv().ok()?;
            // A message from a job that is no longer the active one is from a
            // cancelled predecessor still unwinding. Its progress means nothing now.
            let current = self.active.as_ref().map(|a| a.id);
            if current != Some(msg.id) {
                continue;
            }
            match msg.payload {
                JobPayload::Progress {
                    done_bytes,
                    total_bytes,
                    current,
                } => {
                    if let Some(active) = self.active.as_mut() {
                        active.done_bytes = done_bytes;
                        active.total_bytes = total_bytes;
                        active.current = current;
                    }
                    self.moved = true;
                    // Progress is state, not an event. Keep draining so a backlog
                    // does not take one turn of the loop per update.
                    continue;
                }
                JobPayload::Conflict { path } => {
                    if let Some(active) = self.active.as_mut() {
                        active.conflict = Some(path.clone());
                    }
                    return Some((msg.kind, JobEvent::Conflict { path }));
                }
                terminal => {
                    self.active = None;
                    self.start_next();
                    let event = match terminal {
                        JobPayload::Done { items } => JobEvent::Done { items },
                        JobPayload::Failed(e) => JobEvent::Failed(e),
                        // Progress and Conflict are both handled above, so the
                        // only thing left that is not Done or Failed is a cancel.
                        _ => JobEvent::Cancelled,
                    };
                    return Some((msg.kind, event));
                }
            }
        }
    }

    /// Start the job at the head of the queue, if nothing is running.
    fn start_next(&mut self) {
        if self.active.is_some() {
            return;
        }
        let Some(req) = self.queue.pop_front() else {
            return;
        };
        let id = self.next_id;
        self.next_id += 1;
        let kind = req.kind();
        let cancel = Arc::new(AtomicBool::new(false));
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<ConflictReply>();
        let mut ctx = Ctx {
            id,
            kind,
            tx: self.tx.clone(),
            wake: match self.worker_wake.try_clone() {
                Ok(fd) => fd,
                Err(_) => return,
            },
            cancel: Arc::clone(&cancel),
            reply_rx,
            standing: None,
            done_bytes: 0,
            total_bytes: 0,
            current: String::new(),
            last_report: Instant::now(),
            dropped: 0,
            items_done: 0,
            temp_seq: 0,
        };
        let thread = std::thread::spawn(move || {
            let payload = match run(req, &mut ctx) {
                Ok(()) => JobPayload::Done {
                    items: ctx.items_done,
                },
                Err(Stop::Cancelled) => JobPayload::Cancelled,
                Err(Stop::Failed(e)) => JobPayload::Failed(e),
            };
            ctx.send_blocking(payload);
        });
        self.active = Some(Active {
            id,
            kind,
            cancel,
            reply_tx: Some(reply_tx),
            thread,
            done_bytes: 0,
            total_bytes: 0,
            current: String::new(),
            conflict: None,
        });
    }
}

impl Drop for Jobs {
    fn drop(&mut self) {
        // The process exits once the window is gone, and a worker killed there
        // leaves its part file behind and its clean-up undone. So tell it to stop
        // and wait: it finishes the write it is in, sees the flag, and unwinds.
        self.cancel();
        let Some(active) = self.active.take() else {
            return;
        };
        // Its last message must arrive, and the queue may be full of progress
        // nobody will read, so keep taking messages until it is through.
        while !active.thread.is_finished() {
            let _ = self.rx.recv_timeout(SHUTDOWN_POLL);
        }
        let _ = active.thread.join();
    }
}

/// Why a job stopped early. Cancellation is not a failure and must not be shown as
/// one: the user asked for it.
#[derive(Debug)]
enum Stop {
    Cancelled,
    Failed(String),
}

type Step<T> = std::result::Result<T, Stop>;

/// Everything a worker needs to report back and to be told to stop.
struct Ctx {
    id: u64,
    kind: JobKind,
    tx: SyncSender<JobMsg>,
    wake: OwnedFd,
    cancel: Arc<AtomicBool>,
    reply_rx: Receiver<ConflictReply>,
    /// The answer an `ApplyToAll` reply left standing, used instead of asking.
    standing: Option<Decision>,
    done_bytes: u64,
    total_bytes: u64,
    current: String,
    last_report: Instant,
    dropped: u64,
    /// Sources completed, for the message the status line ends on.
    items_done: usize,
    /// How many hidden temporary names this job has handed out. Kept with the
    /// job rather than process-wide: the process and job ids in the name already
    /// tell one job's names from another's.
    temp_seq: u64,
}

impl Ctx {
    /// A hidden name nothing else in this process will be given, for a part file
    /// or an entry stepping aside. Whoever uses it still has to create it in a
    /// way that fails if it is taken: a process that died earlier with the same
    /// pid may have left one behind.
    fn temp_name(&mut self, prefix: &str) -> String {
        self.temp_seq += 1;
        format!(
            "{prefix}{}-{}-{}",
            std::process::id(),
            self.id,
            self.temp_seq
        )
    }

    /// Whether the job has been told to stop.
    fn check(&self) -> Step<()> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(Stop::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Count `n` more bytes done, reporting if it is time to.
    fn advance(&mut self, n: u64) -> Step<()> {
        self.done_bytes = self.done_bytes.saturating_add(n);
        self.check()?;
        let now = Instant::now();
        if now.duration_since(self.last_report) >= PROGRESS_INTERVAL {
            self.last_report = now;
            self.report();
        }
        Ok(())
    }

    /// Say which file the job has reached, and report at once: the name changing
    /// is the part of the progress line a user actually reads.
    fn set_current(&mut self, name: &str) -> Step<()> {
        self.check()?;
        self.current.clear();
        self.current.push_str(name);
        self.last_report = Instant::now();
        self.report();
        Ok(())
    }

    /// Send a progress update, dropping it if the queue is full.
    fn report(&mut self) {
        let payload = JobPayload::Progress {
            done_bytes: self.done_bytes,
            total_bytes: self.total_bytes,
            current: self.current.clone(),
        };
        match self.tx.try_send(JobMsg {
            id: self.id,
            kind: self.kind,
            payload,
        }) {
            Ok(()) => ffi::event_signal(self.wake.as_raw_fd()),
            // The loop is not draining. The next update supersedes this one, so
            // dropping it loses nothing but the count of how often it happened.
            Err(TrySendError::Full(_)) => self.dropped += 1,
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// Send a message that must arrive, waiting for room if the queue is full.
    fn send_blocking(&self, payload: JobPayload) {
        if self
            .tx
            .send(JobMsg {
                id: self.id,
                kind: self.kind,
                payload,
            })
            .is_ok()
        {
            ffi::event_signal(self.wake.as_raw_fd());
        }
    }

    /// Ask what to do about `path` already existing, or answer from the standing
    /// reply if the user has already said "do this for all of them".
    fn resolve_conflict(&mut self, path: &Path) -> Step<Decision> {
        self.check()?;
        if let Some(standing) = self.standing {
            return Ok(standing);
        }
        self.send_blocking(JobPayload::Conflict {
            path: path.to_path_buf(),
        });
        match self.reply_rx.recv() {
            Ok(reply) => {
                let (decision, scope) = reply.settle();
                if scope == Scope::AllRemaining {
                    self.standing = Some(decision);
                }
                Ok(decision)
            }
            // The reply channel closed: the job was cancelled, or the window went
            // away, while the prompt was up.
            Err(_) => Err(Stop::Cancelled),
        }
    }
}

/// Do the work the request describes.
fn run(req: Request, ctx: &mut Ctx) -> Step<()> {
    match req {
        Request::Copy { sources, dest_dir } => copy_into(&sources, &dest_dir, ctx),
        Request::Move { sources, dest_dir } => move_into(&sources, &dest_dir, ctx),
        Request::Rename { path, name } => {
            rename_one(&path, &name)?;
            ctx.items_done = 1;
            Ok(())
        }
        Request::Trash { paths } => trash_all(&paths, ctx),
        Request::Delete { paths } => delete_all(&paths, ctx),
    }
}

/// Run `each` over every source, carrying on past a failure in one of them.
///
/// Copying a hundred files where one is unreadable should copy the other
/// ninety-nine and then say what went wrong, rather than stopping at the first
/// problem and leaving the user to work out how far it got. Cancellation is the
/// opposite case and propagates at once: the user asked for it, so nothing after
/// it should happen.
fn for_each_source(
    sources: &[PathBuf],
    ctx: &mut Ctx,
    mut each: impl FnMut(&Path, &mut Ctx) -> Step<()>,
) -> Step<()> {
    let mut failures: Vec<String> = Vec::new();
    for src in sources {
        match each(src, ctx) {
            Ok(()) => ctx.items_done += 1,
            Err(Stop::Cancelled) => return Err(Stop::Cancelled),
            Err(Stop::Failed(e)) => failures.push(e),
        }
    }
    all_or(failures)
}

/// `Ok` if nothing failed, otherwise one failure that stands for all of them: the
/// first in full, and a count of the rest.
fn all_or(failures: Vec<String>) -> Step<()> {
    let count = failures.len();
    match failures.into_iter().next() {
        None => Ok(()),
        Some(first) if count == 1 => Err(Stop::Failed(first)),
        Some(first) => Err(Stop::Failed(format!(
            "{first}, and {} more failed",
            count - 1
        ))),
    }
}

/// A failure of `path`, named the way the status line shows it.
fn failed(path: &Path, err: &std::io::Error) -> Stop {
    Stop::Failed(format!("{}: {err}", files::file_name(path)))
}

/// What is at `path` itself: a symlink is described, not followed.
fn stat(path: &Path) -> Step<Metadata> {
    fs::symlink_metadata(path).map_err(|e| failed(path, &e))
}

/// The directory `path` is an entry of. A bare relative name has an empty parent,
/// which names nothing; the directory it means is the current one.
fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Whether `src` is already an entry of `dest_dir`, however either is spelled.
///
/// Compared by the directories' identity rather than their text, so a
/// destination reached through a symlink or a `..` is still recognised as the
/// place the source already is. Putting an entry "into" the directory it lives in
/// makes the destination and the source one and the same file, and every way of
/// resolving that as an ordinary conflict destroys it.
fn lives_in(src: &Path, dest_dir: &Path) -> bool {
    match (fs::metadata(parent_dir(src)), fs::metadata(dest_dir)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// Refuse to put a directory inside itself.
///
/// The walk would read a tree that grows as fast as it is read. The (dev, ino)
/// guard in the copy only knows the source's directories, and the ones the copy
/// makes are new each time, so nothing else would stop it short of the path
/// length limit.
///
/// Both sides are resolved before they are compared, so a destination reached
/// through a symlink does not get past the check and one spelled with a `..`
/// that leads back out of the source is not caught by it. The text of the paths
/// is only what is compared when they cannot be resolved.
fn guard_containment(sources: &[PathBuf], dest_dir: &Path, verb: &str) -> Step<()> {
    let real_dest = fs::canonicalize(dest_dir);
    for src in sources {
        // Only a real directory can contain anything. A symlink to an ancestor of
        // the destination is copied as the link it is, which is harmless.
        if !fs::symlink_metadata(src).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        let inside = match (&real_dest, fs::canonicalize(src)) {
            (Ok(dest), Ok(src)) => dest.starts_with(src),
            _ => dest_dir.starts_with(src),
        };
        if inside {
            return Err(Stop::Failed(format!(
                "cannot {verb} {} into itself",
                files::file_name(src)
            )));
        }
    }
    Ok(())
}

/// The names in `dir`, or the failure that stopped the listing.
///
/// `shown` is the path to name in that failure, which is not always the one
/// read: a removal lists a directory through its open descriptor.
fn child_names(dir: &Path, shown: &Path) -> Step<Vec<OsString>> {
    let entries = fs::read_dir(dir).map_err(|e| failed(shown, &e))?;
    collect_names(entries, shown)
}

/// Every name the listing yields, or the first error it yields instead.
///
/// A network or FUSE filesystem can fail part way through a listing. Skipping the
/// entry that failed would hand back a shorter list that looks complete, and the
/// caller of a cross-device move goes on to delete the source on the strength of
/// it, so a listing that did not finish is a failure of the whole directory.
fn collect_names(
    entries: impl Iterator<Item = std::io::Result<fs::DirEntry>>,
    shown: &Path,
) -> Step<Vec<OsString>> {
    entries
        .map(|entry| match entry {
            Ok(entry) => Ok(entry.file_name()),
            Err(e) => Err(Stop::Failed(format!(
                "{} could not be listed in full: {e}",
                files::file_name(shown)
            ))),
        })
        .collect()
}

// ---- where things land ---------------------------------------------------

/// Where an incoming entry goes, once any conflict over the name is settled.
enum Landing {
    /// Nothing was at this path when it was looked at. Taking it must still fail
    /// rather than replace, in case something has appeared since.
    Fresh(PathBuf),
    /// Keep both: the first free numbered name beside this path.
    Numbered(PathBuf),
    /// The user chose to replace what is at this path.
    Replace(PathBuf),
}

impl Landing {
    fn path(&self) -> &Path {
        match self {
            Landing::Fresh(path) | Landing::Numbered(path) | Landing::Replace(path) => path,
        }
    }
}

/// The numbered names beside `path`: `photo.jpg` gives `photo (2).jpg`, then
/// `photo (3).jpg`. The number goes before the extension so the copy still opens
/// in the same application as the original.
///
/// An iterator rather than a search for the first free one: whoever takes a name
/// tries each in turn with a call that fails if it is taken, so there is no
/// moment between finding a name free and using it.
fn numbered_names(path: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    let stem = path.file_stem().unwrap_or_default();
    let ext = path.extension();
    (2..u32::MAX).map(move |n| {
        let mut name = OsString::from(stem);
        name.push(format!(" ({n})"));
        if let Some(ext) = ext {
            name.push(".");
            name.push(ext);
        }
        path.with_file_name(name)
    })
}

/// Move the entry at `from` to where `landing` says, never replacing anything
/// the user did not agree to replace.
///
/// A rename onto an existing file swaps it atomically: there is no moment at
/// which the name holds neither the old file nor the new one, which is what makes
/// replacing safe to attempt without removing anything first.
fn rename_into(from: &Path, landing: &Landing) -> std::io::Result<()> {
    match landing {
        Landing::Fresh(dst) => sys::rename_noreplace(from, dst),
        Landing::Numbered(dst) => {
            for candidate in numbered_names(dst) {
                match sys::rename_noreplace(from, &candidate) {
                    Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
                    done => return done,
                }
            }
            Err(ErrorKind::AlreadyExists.into())
        }
        Landing::Replace(dst) => fs::rename(from, dst),
    }
}

/// What went wrong taking the name `landing` describes.
fn landing_error(landing: &Landing, err: &std::io::Error) -> Stop {
    if err.kind() == ErrorKind::AlreadyExists {
        return Stop::Failed(format!(
            "{} appeared at the destination in the meantime",
            files::file_name(landing.path())
        ));
    }
    failed(landing.path(), err)
}

/// Something moved out of the way so its name can be taken, kept until the
/// replacement is known to be whole.
struct Displaced {
    aside: PathBuf,
    home: PathBuf,
}

impl Displaced {
    /// The replacement is in place, so the old entry can go.
    ///
    /// Not cancellable, and deliberately: by now the user's answer has been
    /// carried out, and stopping half way would leave a hidden, partly removed
    /// tree beside the new one.
    fn discard(self) -> Step<()> {
        remove_any(&self.aside).map_err(|e| {
            Stop::Failed(format!(
                "{} was replaced, but the old one could not be removed and is at {}: {e}",
                files::file_name(&self.home),
                self.aside.display()
            ))
        })
    }

    /// The replacement did not happen, so the old entry goes back under its name.
    /// Returns the reason it did not, for the caller to pass on.
    fn restore(self, why: Stop) -> Stop {
        match sys::rename_noreplace(&self.aside, &self.home) {
            Ok(()) => why,
            Err(e) => Stop::Failed(format!(
                "{} could not be replaced, and could not be put back either ({e}); it is at {}",
                files::file_name(&self.home),
                self.aside.display()
            )),
        }
    }
}

/// Remove whatever is at `path`, a directory with everything in it. For the
/// clean-up steps that must finish whether or not the job has been cancelled.
fn remove_any(path: &Path) -> std::io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Replace what is at `dst` with whatever `put` puts there, losing nothing if
/// `put` fails.
///
/// Used where a rename cannot do the swap in one step: a directory giving way to
/// a file, or a file to a directory. Removing the old entry first is the obvious
/// way and the wrong one, since a failure after that point (a full disk, a
/// cancel) leaves nothing under the name at all. So the old entry is renamed to a
/// hidden name beside itself, and only removed once the new one is in place.
///
/// `put` must leave `dst` empty if it fails, so the old entry has a name to go
/// back to.
fn replacing<T>(dst: &Path, ctx: &mut Ctx, put: impl FnOnce(&mut Ctx) -> Step<T>) -> Step<T> {
    let old = loop {
        let aside = parent_dir(dst).join(ctx.temp_name(ASIDE_PREFIX));
        match sys::rename_noreplace(dst, &aside) {
            Ok(()) => {
                break Displaced {
                    aside,
                    home: dst.to_path_buf(),
                }
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(failed(dst, &e)),
        }
    };
    match put(ctx) {
        Ok(value) => old.discard().map(|()| value),
        Err(why) => Err(old.restore(why)),
    }
}

// ---- copy ----------------------------------------------------------------

/// Why bytes are being copied, which decides how much a copy has to guarantee.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Purpose {
    /// A copy the user asked for. The source stays, so the copy is best-effort
    /// about everything but the bytes: an attribute the destination cannot hold
    /// is dropped the way `cp` drops it, and nothing is flushed to disk.
    Duplicate,
    /// The copying half of a move between filesystems. The source is removed
    /// afterwards, so anything the copy does not carry is gone for good. It must
    /// keep every `user.*` attribute (the namespace collections store their
    /// metadata in) or fail before the source is touched, and it is flushed to
    /// disk before the source is unlinked, so a power cut cannot leave the data
    /// in neither place.
    Relocate,
}

/// What a copy carries down the tree with it.
struct Walk {
    purpose: Purpose,
    /// The source the walk started from, resolved. Nothing that holds it may be
    /// replaced by a piece of it (see [`guard_replacing`]).
    root: PathBuf,
    /// The (device, inode) of every source directory currently entered. Symlinks
    /// are copied as symlinks rather than followed, so a link pointing at an
    /// ancestor cannot start a loop; this is the backstop for the cycles a symlink
    /// is not needed for, such as a bind mount of a directory inside itself.
    seen: HashSet<(u64, u64)>,
    /// Entries inside a directory that could not be copied. One socket in a tree
    /// of ten thousand files should cost the copy that socket, not everything
    /// after it, so they are gathered here and reported once the tree is done.
    failures: Vec<String>,
}

impl Walk {
    fn new(purpose: Purpose, src: &Path) -> Self {
        Self {
            purpose,
            root: fs::canonicalize(src).unwrap_or_else(|_| src.to_path_buf()),
            seen: HashSet::new(),
            failures: Vec::new(),
        }
    }

    /// Whether the whole tree made it across.
    fn finish(self) -> Step<()> {
        all_or(self.failures)
    }
}

/// What a source entry is, for deciding how to copy it.
enum Kind {
    File,
    Dir,
    Symlink,
    /// A FIFO, socket or device node, described for the message that refuses it.
    Special(&'static str),
}

fn kind_of(meta: &Metadata) -> Kind {
    let ty = meta.file_type();
    if ty.is_symlink() {
        Kind::Symlink
    } else if ty.is_dir() {
        Kind::Dir
    } else if ty.is_file() {
        Kind::File
    } else if ty.is_fifo() {
        Kind::Special("a named pipe")
    } else if ty.is_socket() {
        Kind::Special("a socket")
    } else {
        Kind::Special("a device")
    }
}

/// Refuse the things that are not data.
///
/// A FIFO has no bytes to copy, only a reader to wait for: opening one blocks
/// until something writes to it, which for a copy is forever. A device node
/// "copies" as whatever reading the device yields, which is not the node. Neither
/// is opened; they are reported and left where they are. Moving one within a
/// filesystem never comes here, since a rename does not care what it renames.
fn refuse_special(src: &Path, meta: &Metadata) -> Step<()> {
    match kind_of(meta) {
        Kind::Special(what) => Err(Stop::Failed(format!(
            "{} is {what}, which cannot be copied",
            files::file_name(src)
        ))),
        _ => Ok(()),
    }
}

/// Copy every source into `dest_dir`, measuring the whole tree first so the
/// progress bar means something from the first byte.
fn copy_into(sources: &[PathBuf], dest_dir: &Path, ctx: &mut Ctx) -> Step<()> {
    guard_containment(sources, dest_dir, "copy")?;
    ctx.total_bytes = measure(sources, ctx)?;
    for_each_source(sources, ctx, |src, ctx| {
        let Some(name) = src.file_name() else {
            return Ok(());
        };
        let dst = dest_dir.join(name);
        let mut walk = Walk::new(Purpose::Duplicate, src);
        if lives_in(src, dest_dir) {
            // Copying something into the folder it is already in. The name is
            // taken by the source itself, so there is no conflict to ask about
            // (overwriting would be copying the file onto itself): the copy lands
            // beside it under the next free name, as a duplicate.
            let meta = stat(src)?;
            copy_onto(src, &meta, Landing::Numbered(dst), ctx, &mut walk)?;
        } else {
            copy_tree(src, &dst, ctx, &mut walk)?;
        }
        walk.finish()
    })
}

/// Copy one path to `dst`, asking about whatever is already there.
///
/// Two real directories merge: the incoming one's entries go into the existing
/// one, each asked about in turn if it collides. Anything else already at `dst`
/// is a conflict, including a symlink to a directory. That one is judged as the
/// link it is and never entered, or the copy would be writing its children
/// through the link to somewhere outside the destination.
fn copy_tree(src: &Path, dst: &Path, ctx: &mut Ctx, walk: &mut Walk) -> Step<()> {
    ctx.check()?;
    let meta = stat(src)?;
    refuse_special(src, &meta)?;
    let landing = match fs::symlink_metadata(dst) {
        Err(_) => Landing::Fresh(dst.to_path_buf()),
        Ok(there) if meta.is_dir() && there.is_dir() => {
            return enter_dir(src, &meta, ctx, walk, |ctx, walk| {
                fill_dir(src, dst, &meta, ctx, walk)
            });
        }
        Ok(_) => match ctx.resolve_conflict(dst)? {
            Decision::Skip => {
                // Skipped bytes still count, or the bar would never reach the end.
                let skipped = measure(std::slice::from_ref(&src.to_path_buf()), ctx)?;
                return ctx.advance(skipped);
            }
            Decision::Overwrite => {
                guard_replacing(dst, walk)?;
                Landing::Replace(dst.to_path_buf())
            }
            Decision::KeepBoth => Landing::Numbered(dst.to_path_buf()),
        },
    };
    copy_onto(src, &meta, landing, ctx, walk)
}

/// Refuse to replace an entry that the source being copied is inside of, or is.
///
/// Copying `b/d/d` into `b` merges it into `b/d`, and an entry in it named `d`
/// then collides with `b/d/d`: the source itself. Replacing that would move the
/// source aside and delete it while it is still being read. The destination
/// never being inside the source ([`guard_containment`]) does not cover it; this
/// is the same mistake the other way up.
fn guard_replacing(dst: &Path, walk: &Walk) -> Step<()> {
    let Ok(parent) = fs::canonicalize(parent_dir(dst)) else {
        return Ok(());
    };
    let real = match dst.file_name() {
        Some(name) => parent.join(name),
        None => parent,
    };
    if walk.root.starts_with(&real) {
        return Err(Stop::Failed(format!(
            "{} holds what is being copied, so it cannot be replaced by it",
            files::file_name(dst)
        )));
    }
    Ok(())
}

/// Copy `src` to a landing that has already been decided.
fn copy_onto(
    src: &Path,
    meta: &Metadata,
    landing: Landing,
    ctx: &mut Ctx,
    walk: &mut Walk,
) -> Step<()> {
    refuse_special(src, meta)?;
    match kind_of(meta) {
        Kind::Dir => enter_dir(src, meta, ctx, walk, |ctx, walk| {
            copy_dir(src, meta, landing, ctx, walk)
        }),
        Kind::Symlink => copy_symlink(src, &landing, ctx),
        Kind::File | Kind::Special(_) => copy_file(src, &landing, ctx, walk.purpose),
    }
}

/// Run `body` inside the source directory `src`, unless the walk is already
/// inside it further up, which is a cycle.
fn enter_dir(
    src: &Path,
    meta: &Metadata,
    ctx: &mut Ctx,
    walk: &mut Walk,
    body: impl FnOnce(&mut Ctx, &mut Walk) -> Step<()>,
) -> Step<()> {
    let key = (meta.dev(), meta.ino());
    if !walk.seen.insert(key) {
        return Ok(());
    }
    let done = ctx
        .set_current(&files::file_name(src))
        .and_then(|()| body(ctx, walk));
    walk.seen.remove(&key);
    done
}

/// Make the directory `landing` describes and copy `src`'s entries into it.
fn copy_dir(
    src: &Path,
    meta: &Metadata,
    landing: Landing,
    ctx: &mut Ctx,
    walk: &mut Walk,
) -> Step<()> {
    match landing {
        Landing::Fresh(dst) => {
            fs::create_dir(&dst).map_err(|e| failed(&dst, &e))?;
            fill_dir(src, &dst, meta, ctx, walk)
        }
        Landing::Numbered(dst) => {
            // `create_dir` fails on a taken name, so trying each in turn is the
            // check and the claim in one step.
            let made =
                numbered_names(&dst).find_map(|candidate| match fs::create_dir(&candidate) {
                    Ok(()) => Some(Ok(candidate)),
                    Err(e) if e.kind() == ErrorKind::AlreadyExists => None,
                    Err(e) => Some(Err(failed(&candidate, &e))),
                });
            match made {
                Some(made) => fill_dir(src, &made?, meta, ctx, walk),
                None => Err(Stop::Failed(format!(
                    "no free name left beside {}",
                    files::file_name(&dst)
                ))),
            }
        }
        // Two directories merge before a landing is ever chosen, so what is being
        // replaced here is a file or a symlink.
        Landing::Replace(dst) => replacing(&dst, ctx, |ctx| {
            fs::create_dir(&dst).map_err(|e| failed(&dst, &e))?;
            let before = walk.failures.len();
            let filled = fill_dir(src, &dst, meta, ctx, walk)
                .and_then(|()| all_or(walk.failures.split_off(before)));
            if filled.is_err() {
                // An incomplete directory is not what the user agreed to trade
                // the old entry for. This one was made a moment ago under a name
                // that had just been vacated, so everything in it is this copy's.
                let _ = remove_any(&dst);
            }
            filled
        }),
    }
}

/// Copy `src`'s entries into the existing directory `dst`, then give `dst` the
/// source's attributes.
fn fill_dir(src: &Path, dst: &Path, meta: &Metadata, ctx: &mut Ctx, walk: &mut Walk) -> Step<()> {
    for name in child_names(src, src)? {
        match copy_tree(&src.join(&name), &dst.join(&name), ctx, walk) {
            Ok(()) => {}
            Err(Stop::Cancelled) => return Err(Stop::Cancelled),
            Err(Stop::Failed(e)) => walk.failures.push(e),
        }
    }
    finish_dir(src, dst, meta, walk.purpose)
}

/// Attributes, mode and times for a directory whose contents were just written.
///
/// After the contents, because writing them is what moved the times in the first
/// place; and the attributes before the mode, because a `user.*` attribute needs
/// write permission on the directory and the source's mode may not grant it.
fn finish_dir(src: &Path, dst: &Path, meta: &Metadata, purpose: Purpose) -> Step<()> {
    let opened = File::open(src).and_then(|from| Ok((from, File::open(dst)?)));
    let carried = match opened {
        Ok((from, to)) => carry_xattrs(&from, &to, src, purpose).map(|()| Some(to)),
        Err(e) if purpose == Purpose::Relocate => Err(failed(dst, &e)),
        Err(_) => Ok(None),
    };
    let _ = fs::set_permissions(dst, fs::Permissions::from_mode(meta.mode() & MODE_BITS));
    if let Some(to) = carried? {
        sys::set_times(&to, meta);
        if purpose == Purpose::Relocate {
            // The entries just written into this directory are only durable once
            // the directory itself is.
            to.sync_all().map_err(|e| failed(dst, &e))?;
        }
    }
    Ok(())
}

/// Carry `src`'s extended attributes over to `dst`, as far as `purpose` needs.
///
/// A plain copy drops what the destination cannot hold, as `cp` does: the source
/// still has it. A move cannot shrug the same way, because its source is about to
/// be removed and a `user.*` attribute lost here is lost everywhere. So for a
/// move, one that cannot be carried fails the item while the source is still
/// whole. Attributes outside `user.*` (`security.*`, `trusted.*`, ACLs the
/// destination does not support) need privileges or filesystem support an
/// ordinary move is not expected to have, and never fail anything.
fn carry_xattrs(from: &File, to: &File, src: &Path, purpose: Purpose) -> Step<()> {
    let lost = match sys::copy_xattrs(from, to) {
        Ok(lost) => lost,
        Err(_) if purpose == Purpose::Duplicate => return Ok(()),
        Err(e) => {
            return Err(Stop::Failed(format!(
                "{}: its attributes could not be read, so it was not moved: {e}",
                files::file_name(src)
            )))
        }
    };
    let precious = lost
        .iter()
        .find(|name| name.as_bytes().starts_with(USER_XATTR_PREFIX));
    match precious {
        Some(name) if purpose == Purpose::Relocate => Err(Stop::Failed(format!(
            "{}: the destination cannot keep its {} attribute, so it was not moved",
            files::file_name(src),
            name.to_string_lossy()
        ))),
        _ => Ok(()),
    }
}

/// Recreate a symlink where `landing` says, pointing wherever `src` pointed.
///
/// Made under a hidden name and renamed into place like a file is, so replacing
/// an existing entry is one atomic step rather than a remove followed by a
/// create with nothing under the name in between.
fn copy_symlink(src: &Path, landing: &Landing, ctx: &mut Ctx) -> Step<()> {
    let target = fs::read_link(src).map_err(|e| failed(src, &e))?;
    let (part, ()) = Part::stage(parent_dir(landing.path()), ctx, |path| {
        std::os::unix::fs::symlink(&target, path)
    })?;
    part.publish(landing, ctx)
}

/// A destination being written under a hidden name, removed unless it is
/// published.
///
/// The whole point of the part file: every early return between creating it and
/// renaming it into place — an error, a cancel, a panic — takes the half-written
/// bytes with it, so no destination is ever visible under its real name until it
/// is whole, and whatever was under that name before is still there.
struct Part {
    path: PathBuf,
    published: bool,
}

impl Part {
    /// Make something under a fresh hidden name in `dir`.
    ///
    /// `make` must fail with `AlreadyExists` on a taken name rather than reuse
    /// it, which is what lets a leftover from a dead process with the same pid be
    /// stepped over instead of written into.
    fn stage<T>(
        dir: &Path,
        ctx: &mut Ctx,
        mut make: impl FnMut(&Path) -> std::io::Result<T>,
    ) -> Step<(Self, T)> {
        loop {
            let path = dir.join(ctx.temp_name(PART_PREFIX));
            match make(&path) {
                Ok(made) => {
                    let part = Self {
                        path,
                        published: false,
                    };
                    return Ok((part, made));
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(failed(dir, &e)),
            }
        }
    }

    /// Put the finished entry under its real name.
    fn publish(mut self, landing: &Landing, ctx: &mut Ctx) -> Step<()> {
        let staged = self.path.clone();
        let over_a_directory = match landing {
            Landing::Replace(dst) => fs::symlink_metadata(dst).is_ok_and(|m| m.is_dir()),
            _ => false,
        };
        if over_a_directory {
            // A rename will not put a file where a directory is, so the directory
            // steps aside first and comes back if the rename fails.
            let dst = landing.path();
            replacing(dst, ctx, |_| {
                sys::rename_noreplace(&staged, dst).map_err(|e| landing_error(landing, &e))
            })?;
        } else {
            rename_into(&staged, landing).map_err(|e| landing_error(landing, &e))?;
        }
        self.published = true;
        Ok(())
    }
}

impl Drop for Part {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Open `src` for copying, insisting that it is a regular file.
///
/// The check is made on the open descriptor, not on the path: between a `stat`
/// and an `open` the entry can be swapped for a FIFO, and then the open is what
/// hangs. So the open itself is made unable to block (`O_NONBLOCK` returns from a
/// FIFO with no writer at once) or to follow a symlink swapped in, and what was
/// actually opened is examined afterwards. For a regular file the flag changes
/// nothing about how it reads.
fn open_regular(src: &Path) -> Step<(File, Metadata)> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(sys::O_NONBLOCK | sys::O_NOFOLLOW | sys::O_NOCTTY)
        .open(src)
        .map_err(|e| failed(src, &e))?;
    let meta = file.metadata().map_err(|e| failed(src, &e))?;
    if !meta.is_file() {
        return Err(Stop::Failed(format!(
            "{} is not a regular file, so it cannot be copied",
            files::file_name(src)
        )));
    }
    Ok((file, meta))
}

/// Start a part file in `dir`.
///
/// Owner-only from the first byte, and never an existing file: the source may be
/// private, and until the copy gives it the source's mode the part file is all
/// there is to protect its contents with. `create_new` is `O_EXCL`, which also
/// refuses a symlink planted under the name; `O_NOFOLLOW` says so twice.
fn create_part(dir: &Path, ctx: &mut Ctx) -> Step<(Part, File)> {
    Part::stage(dir, ctx, |path| {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(PART_MODE)
            .custom_flags(sys::O_NOFOLLOW)
            .open(path)
    })
}

/// Copy one regular file, preserving its mode, timestamps and attributes.
fn copy_file(src: &Path, landing: &Landing, ctx: &mut Ctx, purpose: Purpose) -> Step<()> {
    ctx.set_current(&files::file_name(src))?;
    let (reader, meta) = open_regular(src)?;
    let (part, writer) = create_part(parent_dir(landing.path()), ctx)?;

    copy_bytes(&reader, &writer, meta.len(), ctx)?;
    // Attributes while the file is still owner-writable, then mode and times, all
    // before the rename, so the file is never visible with the wrong ones.
    carry_xattrs(&reader, &writer, src, purpose)?;
    let _ = writer.set_permissions(fs::Permissions::from_mode(meta.mode() & MODE_BITS));
    sys::set_times(&writer, &meta);
    if purpose == Purpose::Relocate {
        writer.sync_all().map_err(|e| failed(src, &e))?;
    }
    drop(writer);

    part.publish(landing, ctx)
}

/// How the bytes of one extent are being moved.
enum Mover {
    /// `copy_file_range`: the kernel moves them without a round trip through
    /// user space, which on a server-side-copy filesystem (NFS) is also a message
    /// rather than a transfer.
    Kernel,
    /// Read and write through this buffer. Works everywhere.
    Buffer(Vec<u8>),
}

/// Copy `len` bytes from `src` to `dst`, cheapest mechanism first, keeping holes
/// as holes.
///
/// 1. `FICLONE` — a reflink. On btrfs and xfs the copy is a metadata operation
///    that shares the extents, so a gigabyte costs nothing and no bytes move.
/// 2. Otherwise the file is walked extent by extent with `SEEK_DATA` and
///    `SEEK_HOLE`, and only the stored bytes are copied. A length-driven loop
///    reads a hole as zeros and writes them out, which turns a sparse disk image
///    occupying a few kilobytes into one occupying its full nominal size.
///    (`copy_file_range` does not preserve holes by itself; it "may expand" them.)
fn copy_bytes(src: &File, dst: &File, len: u64, ctx: &mut Ctx) -> Step<()> {
    ctx.check()?;
    if len == 0 {
        return Ok(());
    }
    if sys::clone_file(src, dst) {
        // A reflink is instant and whole; count it all at once.
        return ctx.advance(len);
    }
    // The size first. Holes are skipped rather than written, so for a file that
    // ends in one nothing else would make the destination as long as the source.
    dst.set_len(len)
        .map_err(|e| Stop::Failed(format!("copy failed: {e}")))?;
    let mut mover = Mover::Kernel;
    let mut pos = 0u64;
    while let Some((data, end)) = next_extent(src, pos, len) {
        // The hole stepped over counts as done, or the bar would stall on it.
        ctx.advance(data - pos)?;
        let copied = copy_extent(src, dst, data, end, &mut mover, ctx)?;
        if copied < end - data {
            // The source shrank while it was being read. What was copied is
            // consistent; the rest was never there, so it is not left as zeros.
            return dst
                .set_len(data + copied)
                .map_err(|e| Stop::Failed(format!("copy failed: {e}")));
        }
        pos = end;
    }
    // A source truncated into what was a hole shows no short read: the walk just
    // finds no more data. Its length is asked once more, so that case ends the
    // same way as the one above rather than with a tail of zeros.
    if let Ok(now) = src.metadata() {
        if now.len() < len {
            dst.set_len(now.len())
                .map_err(|e| Stop::Failed(format!("copy failed: {e}")))?;
        }
    }
    ctx.advance(len.saturating_sub(pos))
}

/// The next run of stored bytes at or after `pos`, as `(start, end)` clamped to
/// `len`, or `None` when nothing but hole is left.
fn next_extent(src: &File, pos: u64, len: u64) -> Option<(u64, u64)> {
    if pos >= len {
        return None;
    }
    let data = match sys::next_data(src, pos) {
        Ok(Some(data)) => data.max(pos),
        Ok(None) => return None,
        // A filesystem that cannot say is treated as having no holes at all,
        // which is always a correct copy, only not a sparse one.
        Err(_) => return Some((pos, len)),
    };
    if data >= len {
        return None;
    }
    let end = match sys::next_hole(src, data) {
        Ok(hole) if hole > data => hole.min(len),
        _ => len,
    };
    Some((data, end))
}

/// Copy the bytes in `start..end`, returning how many were there to copy. Fewer
/// than asked for means the source ended early.
fn copy_extent(
    src: &File,
    dst: &File,
    start: u64,
    end: u64,
    mover: &mut Mover,
    ctx: &mut Ctx,
) -> Step<u64> {
    let mut at = start;
    while at < end {
        ctx.check()?;
        let want = (end - at).min(CHUNK as u64) as usize;
        let moved = match mover {
            Mover::Kernel => match sys::copy_range(src, dst, at, want) {
                Ok(n) => n,
                // Every offset is passed explicitly, so the buffer can pick up
                // exactly where the kernel left off.
                Err(e) if sys::unsupported(&e) => {
                    *mover = Mover::Buffer(vec![0u8; RW_BUF]);
                    continue;
                }
                Err(e) => return Err(Stop::Failed(format!("copy failed: {e}"))),
            },
            Mover::Buffer(buf) => {
                let want = want.min(buf.len());
                let Some(chunk) = buf.get_mut(..want) else {
                    break;
                };
                let n = match src.read_at(chunk, at) {
                    Ok(n) => n,
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(e) => return Err(Stop::Failed(format!("read failed: {e}"))),
                };
                let Some(read) = chunk.get(..n) else {
                    break;
                };
                dst.write_all_at(read, at)
                    .map_err(|e| Stop::Failed(format!("write failed: {e}")))?;
                n
            }
        };
        if moved == 0 {
            break;
        }
        at += moved as u64;
        ctx.advance(moved as u64)?;
    }
    Ok(at - start)
}

// ---- move ----------------------------------------------------------------

/// Move every source into `dest_dir`.
///
/// A move within one filesystem is a `rename`, which is instant however large the
/// tree is and needs no measuring at all. Only a source that turns out to be on
/// another filesystem is measured and copied, which is why the progress total can
/// grow part way through a move: it counts the bytes that actually have to travel,
/// and until a rename fails there is no way to know which those are.
fn move_into(sources: &[PathBuf], dest_dir: &Path, ctx: &mut Ctx) -> Step<()> {
    guard_containment(sources, dest_dir, "move")?;
    for_each_source(sources, ctx, |src, ctx| move_one(src, dest_dir, ctx))
}

fn move_one(src: &Path, dest_dir: &Path, ctx: &mut Ctx) -> Step<()> {
    let Some(name) = src.file_name() else {
        return Ok(());
    };
    if lives_in(src, dest_dir) {
        // The destination is the source. Asking whether to overwrite it and then
        // obeying would remove the only copy there is, so this is refused before
        // any question is put.
        return Err(Stop::Failed(format!(
            "{} is already there",
            files::file_name(src)
        )));
    }
    move_tree(src, &dest_dir.join(name), ctx)
}

/// Move one path to `dst`, asking about whatever is already there.
///
/// The same rules as [`copy_tree`], so that Overwrite means one thing whichever
/// way the files got here: two real directories merge, entry by entry, and
/// anything else in the way is a conflict.
fn move_tree(src: &Path, dst: &Path, ctx: &mut Ctx) -> Step<()> {
    ctx.check()?;
    let meta = stat(src)?;
    let landing = match fs::symlink_metadata(dst) {
        Err(_) => Landing::Fresh(dst.to_path_buf()),
        Ok(there) if meta.is_dir() && there.is_dir() => {
            // One directory under two names (a bind mount of it, say). Merging it
            // into itself would find every entry in conflict with itself, and
            // "overwriting" a file with itself ends in removing it.
            if (there.dev(), there.ino()) == (meta.dev(), meta.ino()) {
                return Err(Stop::Failed(format!(
                    "{} is already there",
                    files::file_name(src)
                )));
            }
            return merge_move(src, dst, ctx);
        }
        Ok(_) => match ctx.resolve_conflict(dst)? {
            Decision::Skip => return Ok(()),
            Decision::Overwrite => Landing::Replace(dst.to_path_buf()),
            Decision::KeepBoth => Landing::Numbered(dst.to_path_buf()),
        },
    };
    ctx.set_current(&files::file_name(src))?;
    relocate(src, &meta, landing, ctx)
}

/// Move the entries of the directory `src` into the existing directory `dst`,
/// then remove `src` if that emptied it.
fn merge_move(src: &Path, dst: &Path, ctx: &mut Ctx) -> Step<()> {
    ctx.set_current(&files::file_name(src))?;
    let mut failures = Vec::new();
    for name in child_names(src, src)? {
        match move_tree(&src.join(&name), &dst.join(&name), ctx) {
            Ok(()) => {}
            Err(Stop::Cancelled) => return Err(Stop::Cancelled),
            Err(Stop::Failed(e)) => failures.push(e),
        }
    }
    all_or(failures)?;
    match fs::remove_dir(src) {
        Ok(()) => Ok(()),
        // Whatever the user chose to skip is still in there, and the directory
        // stays to hold it.
        Err(e) if e.kind() == ErrorKind::DirectoryNotEmpty => Ok(()),
        Err(e) => Err(failed(src, &e)),
    }
}

/// Put `src` where `landing` says and take it away from where it was.
///
/// Nothing at the destination is removed before its replacement is in place. A
/// file replacing a file is one `rename`, which swaps them atomically; a
/// directory and a non-directory cannot be swapped that way, so the old one steps
/// aside (see [`replacing`]) until the new one has arrived.
fn relocate(src: &Path, meta: &Metadata, landing: Landing, ctx: &mut Ctx) -> Step<()> {
    if let Landing::Replace(dst) = &landing {
        match fs::symlink_metadata(dst) {
            // Gone since the user was asked, so there is nothing to replace.
            Err(_) => return relocate(src, meta, Landing::Fresh(dst.clone()), ctx),
            // Another name for the very same file (a hard link). Renaming one
            // onto the other is defined to do nothing at all, which would leave
            // the source in place and the move reported as done; dropping the
            // source's name is the move.
            Ok(there)
                if !meta.is_dir() && (there.dev(), there.ino()) == (meta.dev(), meta.ino()) =>
            {
                return fs::remove_file(src).map_err(|e| failed(src, &e));
            }
            Ok(there) if there.is_dir() != meta.is_dir() => {
                let fresh = Landing::Fresh(dst.clone());
                let copied = replacing(dst, ctx, |ctx| match rename_into(src, &fresh) {
                    Ok(()) => Ok(false),
                    Err(e) if sys::is_cross_device(&e) => {
                        copy_into_vacated(src, meta, dst, ctx).map(|()| true)
                    }
                    Err(e) => Err(landing_error(&fresh, &e)),
                })?;
                // Only once the old entry has been let go: a failure in here
                // must not send it back on top of a destination that is whole.
                return if copied {
                    remove_tree(src, ctx)
                } else {
                    Ok(())
                };
            }
            Ok(_) => {}
        }
    }
    match rename_into(src, &landing) {
        Ok(()) => Ok(()),
        // Another filesystem, so the bytes have to travel after all.
        Err(e) if sys::is_cross_device(&e) => move_across(src, meta, landing, ctx),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Err(landing_error(&landing, &e)),
        Err(e) => Err(failed(src, &e)),
    }
}

/// [`copy_across`] onto a name that was vacated for it, leaving the name empty
/// again if the copy does not complete.
///
/// A directory that fails or is cancelled part way stays behind as a partial
/// tree, which is right for an ordinary move (what was copied is whole, and the
/// source is untouched) and wrong here: the old entry is waiting to come back
/// to this name, and cannot while something is sitting on it.
fn copy_into_vacated(src: &Path, meta: &Metadata, dst: &Path, ctx: &mut Ctx) -> Step<()> {
    let copied = copy_across(src, meta, Landing::Fresh(dst.to_path_buf()), ctx);
    if copied.is_err() {
        let _ = remove_any(dst);
    }
    copied
}

/// The cross-device move `rename` cannot do: copy the whole tree, and only then
/// remove the source.
///
/// The ordering is the whole point. Removing the source first, or as each file is
/// copied, turns any failure part way through — a full disk, an unreadable file,
/// the machine losing power — into data that existed at the start and exists
/// nowhere at the end.
fn move_across(src: &Path, meta: &Metadata, landing: Landing, ctx: &mut Ctx) -> Step<()> {
    copy_across(src, meta, landing, ctx)?;
    remove_tree(src, ctx)
}

/// The copying half of a cross-device move: everything under `src`, whole, with
/// its `user.*` attributes, and on disk, or an error.
///
/// A bulk copy does not flush, because `cp` does not and the cost is real. This
/// one does, file by file and directory by directory, because what follows it is
/// the removal of the only other copy.
fn copy_across(src: &Path, meta: &Metadata, landing: Landing, ctx: &mut Ctx) -> Step<()> {
    ctx.total_bytes = ctx
        .total_bytes
        .saturating_add(measure(std::slice::from_ref(&src.to_path_buf()), ctx)?);
    let parent = parent_dir(landing.path()).to_path_buf();
    let mut walk = Walk::new(Purpose::Relocate, src);
    copy_onto(src, meta, landing, ctx, &mut walk)?;
    walk.finish()?;
    // The new name is an entry in its parent, and is not durable until the
    // parent is: without this the files could survive a power cut and the name
    // that reaches them not.
    File::open(&parent)
        .and_then(|dir| dir.sync_all())
        .map_err(|e| failed(&parent, &e))
}

// ---- trash and delete ----------------------------------------------------

/// Move every path to the XDG trash.
fn trash_all(paths: &[PathBuf], ctx: &mut Ctx) -> Step<()> {
    let trash = trash::Trash::for_user()
        .ok_or_else(|| Stop::Failed("there is no trash directory on this machine".into()))?;
    trash_into(&trash, paths, ctx)
}

fn trash_into(trash: &trash::Trash, paths: &[PathBuf], ctx: &mut Ctx) -> Step<()> {
    let now = now_secs();
    // Each tree is walked once. The sizes add up to the bar's denominator, and
    // each is also what its own rename moves: trashing a directory takes
    // everything under it in one step, and its own entry length says nothing
    // about how much that was.
    let mut sizes = Vec::with_capacity(paths.len());
    for path in paths {
        sizes.push(measure(std::slice::from_ref(path), ctx)?);
    }
    ctx.total_bytes = sizes.iter().fold(0u64, |sum, &n| sum.saturating_add(n));
    let mut sizes = sizes.into_iter();
    for_each_source(paths, ctx, |path, ctx| {
        // One size per source, taken in the order they were measured.
        let size = sizes.next().unwrap_or(0);
        ctx.set_current(&files::file_name(path))?;
        trash.put(path, now).map_err(|e| failed(path, &e))?;
        ctx.advance(size)
    })
}

/// Remove every path outright. Nothing here is recoverable, which is why the only
/// way to ask for it is an explicit modifier and a confirmation.
fn delete_all(paths: &[PathBuf], ctx: &mut Ctx) -> Step<()> {
    ctx.total_bytes = measure(paths, ctx)?;
    for_each_source(paths, ctx, remove_tree)
}

/// Remove a path and everything under it, checking for cancellation as it goes.
///
/// Written out rather than handed to `remove_dir_all`, which is opaque: a job the
/// user can call off has to be able to stop part way, and the status line has to
/// be able to say which file it is on.
///
/// A recursive delete that walks by path trusts every directory to still be the
/// one it listed. Swap one for a symlink mid-walk and the next call, resolved
/// afresh from the top, is deleting somewhere else entirely. So each directory is
/// opened once, refusing a symlink, and everything under it is then reached
/// through that descriptor:
///
/// ```text
///   open("tree/sub", O_NOFOLLOW | O_DIRECTORY)  ──▶ fd 7
///   list    /proc/self/fd/7            the directory fd 7 is, whatever
///   unlink  /proc/self/fd/7/a.txt      "tree/sub" names by now
/// ```
///
/// A child swapped for a symlink between being examined and being opened fails
/// the open (`O_NOFOLLOW`) or is unlinked as the link it now is; neither follows
/// it. What is left is the path the caller passed in, which is resolved normally,
/// and a machine with no `/proc`, where the listing falls back to the path and
/// the window between this open and that listing is open again.
fn remove_tree(path: &Path, ctx: &mut Ctx) -> Step<()> {
    ctx.check()?;
    let meta = stat(path)?;
    // `symlink_metadata` already answers about the link rather than its target, so
    // a symlink to a directory is removed as the one-entry thing it is.
    if !meta.is_dir() {
        let size = meta.len();
        fs::remove_file(path).map_err(|e| failed(path, &e))?;
        return ctx.advance(size);
    }
    ctx.set_current(&files::file_name(path))?;
    let dir = fs::OpenOptions::new()
        .read(true)
        .custom_flags(sys::O_DIRECTORY | sys::O_NOFOLLOW)
        .open(path)
        .map_err(|e| failed(path, &e))?;
    let anchored = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
    let inside = if fs::metadata(&anchored).is_ok() {
        anchored.as_path()
    } else {
        path
    };
    for name in child_names(inside, path)? {
        remove_tree(&inside.join(name), ctx)?;
    }
    drop(dir);
    fs::remove_dir(path).map_err(|e| failed(path, &e))
}

/// The wall clock as a unix timestamp, for the trash's deletion dates.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Total bytes in `paths`, so progress has a denominator.
///
/// A stat-only walk: on a tree big enough for the measurement to be noticeable,
/// the copy that follows is far longer still. Cancellation is checked throughout,
/// so an accidental copy of an enormous tree can be called off during the count
/// rather than only once bytes start moving.
///
/// Unlike the walks that act on what they find, this one steps over anything it
/// cannot read. It is only an estimate for the progress bar, and the operation
/// that follows reports the same problem properly.
fn measure(paths: &[PathBuf], ctx: &Ctx) -> Step<u64> {
    let mut total = 0u64;
    for path in paths {
        ctx.check()?;
        let Ok(meta) = fs::symlink_metadata(path) else {
            continue;
        };
        if meta.is_dir() {
            let Ok(entries) = fs::read_dir(path) else {
                continue;
            };
            let children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            total = total.saturating_add(measure(&children, ctx)?);
        } else if meta.is_file() {
            total = total.saturating_add(meta.len());
        }
    }
    Ok(total)
}

// ---- rename --------------------------------------------------------------

/// Rename one entry in place.
///
/// Renaming is not a job in the sense the others are — it is one syscall, and it
/// either happens or does not — but it goes through the same path so that its
/// failures surface the same way and the caller has one way to ask for a change.
fn rename_one(path: &Path, name: &str) -> Step<()> {
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        return Err(Stop::Failed(format!("{name:?} is not a usable name")));
    }
    let dst = path.with_file_name(name);
    if dst == path {
        return Ok(());
    }
    // One call that either renames or reports the name taken, so nothing that
    // appears under the new name at the last moment can be silently replaced.
    match sys::rename_noreplace(path, &dst) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            Err(Stop::Failed(format!("{name} already exists")))
        }
        Err(e) => Err(failed(path, &e)),
    }
}

#[cfg(test)]
mod tests {
    use crate::jobs::*;
    use crate::platform::ffi::PollSet;

    /// A unique temp directory for one test, removed by the test that made it.
    fn temp_dir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "bnkfiles_jobs_{tag}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).expect("make temp dir");
        base
    }

    /// Drive a job the way the event loop does — block in poll, drain, repeat —
    /// until it reports a terminal message. Returns that message, and answers any
    /// conflict prompt with `on_conflict`.
    fn drive(jobs: &mut Jobs, on_conflict: impl Fn(&Path) -> ConflictReply) -> JobEvent {
        let mut poll = PollSet::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            poll.clear();
            poll.add(jobs.wake_fd());
            poll.wait(Some(Duration::from_millis(20))).expect("poll");
            jobs.drain_wake();
            while let Some((_, event)) = jobs.try_recv() {
                match event {
                    JobEvent::Conflict { ref path } => {
                        let reply = on_conflict(path);
                        jobs.answer(reply);
                    }
                    terminal => return terminal,
                }
            }
        }
        panic!("the job never reported a terminal message");
    }

    /// Run one request to completion, refusing any conflict it raises.
    fn run_job(req: Request) -> JobEvent {
        let mut jobs = Jobs::new(Writes::Allowed).expect("jobs");
        jobs.submit(req).expect("submit");
        drive(&mut jobs, |_| ConflictReply::Skip)
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    /// A worker context for driving one operation directly, without a `Jobs` and
    /// its thread around it. The channel ends the event loop would own are held
    /// here, so a send inside the operation reaches somewhere rather than
    /// reporting a disconnected channel.
    struct Harness {
        ctx: Ctx,
        _msgs: Receiver<JobMsg>,
        _replies: Sender<ConflictReply>,
    }

    fn harness() -> Harness {
        let (tx, msgs) = std::sync::mpsc::sync_channel::<JobMsg>(QUEUE_DEPTH);
        let (replies, reply_rx) = std::sync::mpsc::channel::<ConflictReply>();
        Harness {
            ctx: Ctx {
                id: 1,
                kind: JobKind::Copy,
                tx,
                wake: ffi::eventfd().expect("eventfd"),
                cancel: Arc::new(AtomicBool::new(false)),
                reply_rx,
                standing: None,
                done_bytes: 0,
                total_bytes: 0,
                current: String::new(),
                last_report: Instant::now(),
                dropped: 0,
                items_done: 0,
                temp_seq: 0,
            },
            _msgs: msgs,
            _replies: replies,
        }
    }

    #[test]
    fn a_copied_tree_matches_the_original() {
        // The ordinary case, with the shapes a real folder has: nested
        // directories, a file big enough to take more than one chunk of the
        // copy loop, and an empty directory that must still be created.
        let base = temp_dir("copy_tree");
        let src = base.join("src");
        let dst = base.join("dst");
        fs::create_dir_all(src.join("nested/deeper")).expect("dirs");
        fs::create_dir_all(src.join("empty")).expect("empty dir");
        fs::create_dir_all(&dst).expect("dst");
        fs::write(src.join("top.txt"), "top").expect("file");
        fs::write(src.join("nested/middle.txt"), "middle").expect("file");
        fs::write(src.join("nested/deeper/leaf.bin"), vec![7u8; 300_000]).expect("file");

        let payload = run_job(Request::Copy {
            sources: vec![src.clone()],
            dest_dir: dst.clone(),
        });
        assert!(
            matches!(payload, JobEvent::Done { .. }),
            "the copy finished"
        );

        assert_eq!(read(&dst.join("src/top.txt")), "top");
        assert_eq!(read(&dst.join("src/nested/middle.txt")), "middle");
        assert_eq!(
            fs::read(dst.join("src/nested/deeper/leaf.bin")).expect("leaf"),
            vec![7u8; 300_000]
        );
        assert!(
            dst.join("src/empty").is_dir(),
            "an empty directory is still part of the tree"
        );

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_copy_preserves_modes_and_modification_times() {
        let base = temp_dir("copy_meta");
        let (src, dst) = (base.join("src"), base.join("dst"));
        fs::create_dir_all(&src).expect("src");
        fs::create_dir_all(&dst).expect("dst");
        let file = src.join("script.sh");
        fs::write(&file, "#!/bin/sh\n").expect("file");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o750)).expect("chmod");
        let want = fs::metadata(&file).expect("meta");

        let payload = run_job(Request::Copy {
            sources: vec![file.clone()],
            dest_dir: dst.clone(),
        });
        assert!(matches!(payload, JobEvent::Done { .. }));

        let got = fs::metadata(dst.join("script.sh")).expect("copied meta");
        assert_eq!(
            got.mode() & 0o777,
            0o750,
            "an executable copy must stay executable"
        );
        assert_eq!(got.mtime(), want.mtime(), "the modification time carries");
        assert_eq!(got.mtime_nsec(), want.mtime_nsec(), "to the nanosecond");

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_cancelled_copy_leaves_no_torn_destination() {
        // The torn-kill case: a copy stopped part way must leave nothing under the
        // destination's real name, and no part file either. A file large enough
        // that the copy is still running when the cancel lands.
        let base = temp_dir("copy_cancel");
        let (src, dst) = (base.join("src"), base.join("dst"));
        fs::create_dir_all(&src).expect("src");
        fs::create_dir_all(&dst).expect("dst");
        for i in 0..40 {
            fs::write(
                src.join(format!("big_{i:02}.bin")),
                vec![0u8; 4 * 1024 * 1024],
            )
            .expect("file");
        }

        let mut jobs = Jobs::new(Writes::Allowed).expect("jobs");
        jobs.submit(Request::Copy {
            sources: vec![src.clone()],
            dest_dir: dst.clone(),
        })
        .expect("submit");
        // Let it get going, then call it off.
        let mut poll = PollSet::new();
        poll.clear();
        poll.add(jobs.wake_fd());
        poll.wait(Some(Duration::from_millis(50))).expect("poll");
        jobs.cancel();

        let payload = drive(&mut jobs, |_| ConflictReply::Skip);
        assert!(
            matches!(payload, JobEvent::Cancelled),
            "a cancelled job reports Cancelled, not Failed"
        );

        // Whatever it managed to copy is whole; nothing is half-written.
        let mut parts = Vec::new();
        let mut copied = 0;
        for entry in fs::read_dir(dst.join("src"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(PART_PREFIX) {
                parts.push(name);
            } else {
                copied += 1;
                assert_eq!(
                    entry.metadata().expect("meta").len(),
                    4 * 1024 * 1024,
                    "{name} is under its real name but is not whole"
                );
            }
        }
        assert!(parts.is_empty(), "part files left behind: {parts:?}");
        assert!(copied < 40, "the cancel landed after the copy had finished");

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn dropping_the_jobs_waits_for_the_running_one_to_clean_up() {
        // Quitting mid-copy: once the jobs are gone the worker must be too, with
        // its part file removed, since the process exits straight afterwards.
        let base = temp_dir("copy_quit");
        let (src, dst) = (base.join("src"), base.join("dst"));
        fs::create_dir_all(&src).expect("src");
        fs::create_dir_all(&dst).expect("dst");
        for i in 0..40 {
            fs::write(
                src.join(format!("big_{i:02}.bin")),
                vec![0u8; 4 * 1024 * 1024],
            )
            .expect("file");
        }

        let mut jobs = Jobs::new(Writes::Allowed).expect("jobs");
        jobs.submit(Request::Copy {
            sources: vec![src.clone()],
            dest_dir: dst.clone(),
        })
        .expect("submit");
        let mut poll = PollSet::new();
        poll.clear();
        poll.add(jobs.wake_fd());
        poll.wait(Some(Duration::from_millis(50))).expect("poll");
        drop(jobs);

        let names = |dir: &Path| -> Vec<String> {
            fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        };
        let after_drop = names(&dst.join("src"));
        assert!(
            !after_drop.iter().any(|name| name.starts_with(PART_PREFIX)),
            "part files left behind: {after_drop:?}"
        );
        assert!(
            after_drop.len() < 40,
            "the copy finished before the jobs were dropped"
        );
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            names(&dst.join("src")),
            after_drop,
            "the worker was still running after the jobs were dropped"
        );

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn copying_a_directory_into_itself_is_refused() {
        // The reachable infinite recursion: the destination grows inside the
        // source as fast as the walk reads it.
        let base = temp_dir("copy_into_self");
        let src = base.join("src");
        fs::create_dir_all(src.join("inner")).expect("dirs");
        fs::write(src.join("a.txt"), "a").expect("file");

        let payload = run_job(Request::Copy {
            sources: vec![src.clone()],
            dest_dir: src.join("inner"),
        });
        match payload {
            JobEvent::Failed(e) => assert!(
                e.contains("itself"),
                "the message should say what is wrong: {e}"
            ),
            _ => panic!("copying a directory into itself must fail"),
        }

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn the_cycle_guard_visits_each_directory_once() {
        // The guard's rule, on its own. A real cycle needs a bind mount (root) or
        // a hardlinked directory (which Linux does not allow), so the loop it
        // protects against is not constructible in a test; what is testable is
        // that re-entering an already-entered (dev, ino) is refused, which is the
        // property the recursion's termination rests on.
        let base = temp_dir("cycle_guard");
        let meta = fs::symlink_metadata(&base).expect("meta");
        let key = (meta.dev(), meta.ino());
        let mut seen = HashSet::new();
        assert!(seen.insert(key), "first visit proceeds");
        assert!(
            !seen.insert(key),
            "a second visit to the same directory stops"
        );
        seen.remove(&key);
        assert!(
            seen.insert(key),
            "leaving lets a sibling path reach it again"
        );

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_symlink_is_copied_as_a_symlink() {
        // Following it would copy the target's bytes under the link's name, and a
        // link to an ancestor would start a loop the guard then has to catch.
        let base = temp_dir("copy_symlink");
        let (src, dst) = (base.join("src"), base.join("dst"));
        fs::create_dir_all(&src).expect("src");
        fs::create_dir_all(&dst).expect("dst");
        fs::write(src.join("real.txt"), "real").expect("file");
        std::os::unix::fs::symlink("real.txt", src.join("link.txt")).expect("symlink");
        // A link pointing up and out, which following would turn into a loop.
        std::os::unix::fs::symlink("..", src.join("up")).expect("symlink");

        let payload = run_job(Request::Copy {
            sources: vec![src.clone()],
            dest_dir: dst.clone(),
        });
        assert!(matches!(payload, JobEvent::Done { .. }));

        let link = dst.join("src/link.txt");
        assert!(
            fs::symlink_metadata(&link)
                .expect("link")
                .file_type()
                .is_symlink(),
            "the copy is still a symlink"
        );
        assert_eq!(fs::read_link(&link).expect("target"), Path::new("real.txt"));
        assert_eq!(
            fs::read_link(dst.join("src/up")).expect("target"),
            Path::new("..")
        );

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_conflict_is_asked_about_once_per_file_and_the_answer_is_obeyed() {
        let base = temp_dir("conflict");
        let (src, dst) = (base.join("src"), base.join("dst"));
        fs::create_dir_all(&src).expect("src");
        fs::create_dir_all(&dst).expect("dst");
        fs::write(src.join("a.txt"), "new").expect("file");
        fs::write(src.join("b.txt"), "new").expect("file");
        fs::write(dst.join("a.txt"), "old").expect("file");
        fs::write(dst.join("b.txt"), "old").expect("file");

        // Skip: both keep their old contents.
        let mut jobs = Jobs::new(Writes::Allowed).expect("jobs");
        jobs.submit(Request::Copy {
            sources: vec![src.join("a.txt"), src.join("b.txt")],
            dest_dir: dst.clone(),
        })
        .expect("submit");
        let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&asked);
        let payload = drive(&mut jobs, move |_| {
            counter.fetch_add(1, Ordering::Relaxed);
            ConflictReply::Skip
        });
        assert!(matches!(payload, JobEvent::Done { .. }));
        assert_eq!(asked.load(Ordering::Relaxed), 2, "asked once per file");
        assert_eq!(read(&dst.join("a.txt")), "old");
        assert_eq!(read(&dst.join("b.txt")), "old");

        // Overwrite: both take the new contents.
        let mut jobs = Jobs::new(Writes::Allowed).expect("jobs");
        jobs.submit(Request::Copy {
            sources: vec![src.join("a.txt"), src.join("b.txt")],
            dest_dir: dst.clone(),
        })
        .expect("submit");
        assert!(matches!(
            drive(&mut jobs, |_| ConflictReply::Overwrite),
            JobEvent::Done { .. }
        ));
        assert_eq!(read(&dst.join("a.txt")), "new");
        assert_eq!(read(&dst.join("b.txt")), "new");

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn apply_to_all_stops_the_asking() {
        let base = temp_dir("apply_all");
        let (src, dst) = (base.join("src"), base.join("dst"));
        fs::create_dir_all(&src).expect("src");
        fs::create_dir_all(&dst).expect("dst");
        for i in 0..5 {
            fs::write(src.join(format!("f{i}.txt")), "new").expect("file");
            fs::write(dst.join(format!("f{i}.txt")), "old").expect("file");
        }

        let sources: Vec<PathBuf> = (0..5).map(|i| src.join(format!("f{i}.txt"))).collect();
        let mut jobs = Jobs::new(Writes::Allowed).expect("jobs");
        jobs.submit(Request::Copy {
            sources,
            dest_dir: dst.clone(),
        })
        .expect("submit");
        let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&asked);
        let payload = drive(&mut jobs, move |_| {
            counter.fetch_add(1, Ordering::Relaxed);
            ConflictReply::ApplyToAll(Box::new(ConflictReply::Overwrite))
        });
        assert!(matches!(payload, JobEvent::Done { .. }));
        assert_eq!(
            asked.load(Ordering::Relaxed),
            1,
            "the standing answer covers the rest"
        );
        for i in 0..5 {
            assert_eq!(read(&dst.join(format!("f{i}.txt"))), "new");
        }

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn keeping_both_puts_the_copy_beside_the_original() {
        let base = temp_dir("keep_both");
        let (src, dst) = (base.join("src"), base.join("dst"));
        fs::create_dir_all(&src).expect("src");
        fs::create_dir_all(&dst).expect("dst");
        fs::write(src.join("photo.jpg"), "new").expect("file");
        fs::write(dst.join("photo.jpg"), "old").expect("file");

        let mut jobs = Jobs::new(Writes::Allowed).expect("jobs");
        jobs.submit(Request::Copy {
            sources: vec![src.join("photo.jpg")],
            dest_dir: dst.clone(),
        })
        .expect("submit");
        assert!(matches!(
            drive(&mut jobs, |_| ConflictReply::RenameNew),
            JobEvent::Done { .. }
        ));
        assert_eq!(
            read(&dst.join("photo.jpg")),
            "old",
            "the original is intact"
        );
        assert_eq!(
            read(&dst.join("photo (2).jpg")),
            "new",
            "the number goes before the extension"
        );

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn numbered_names_count_up_and_keep_the_extension() {
        let mut names = numbered_names(Path::new("/pics/photo.jpg"));
        assert_eq!(names.next(), Some(PathBuf::from("/pics/photo (2).jpg")));
        assert_eq!(names.next(), Some(PathBuf::from("/pics/photo (3).jpg")));
        // No extension: the number simply goes on the end.
        assert_eq!(
            numbered_names(Path::new("README")).next(),
            Some(PathBuf::from("README (2)"))
        );
        // A name that is not UTF-8 keeps its bytes rather than being rewritten
        // with replacement characters, which would be a different file's name.
        let odd = Path::new(std::ffi::OsStr::from_bytes(b"caf\xe9.txt"));
        assert_eq!(
            numbered_names(odd).next().expect("a name").as_os_str(),
            std::ffi::OsStr::from_bytes(b"caf\xe9 (2).txt")
        );
    }

    #[test]
    fn keeping_both_twice_takes_the_next_number_rather_than_the_same_one() {
        // The number is claimed by the rename that uses it, so a name taken by an
        // earlier copy is stepped over rather than written onto.
        let base = temp_dir("keep_both_twice");
        let (src, dst) = (base.join("src"), base.join("dst"));
        fs::create_dir_all(&src).expect("src");
        fs::create_dir_all(&dst).expect("dst");
        fs::write(dst.join("photo.jpg"), "old").expect("file");
        for body in ["second", "third"] {
            fs::write(src.join("photo.jpg"), body).expect("file");
            let mut jobs = Jobs::new(Writes::Allowed).expect("jobs");
            jobs.submit(Request::Copy {
                sources: vec![src.join("photo.jpg")],
                dest_dir: dst.clone(),
            })
            .expect("submit");
            assert!(matches!(
                drive(&mut jobs, |_| ConflictReply::RenameNew),
                JobEvent::Done { .. }
            ));
        }
        assert_eq!(read(&dst.join("photo.jpg")), "old");
        assert_eq!(read(&dst.join("photo (2).jpg")), "second");
        assert_eq!(read(&dst.join("photo (3).jpg")), "third");

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn renaming_moves_the_entry_and_refuses_a_bad_name() {
        let base = temp_dir("rename");
        let file = base.join("before.txt");
        fs::write(&file, "body").expect("file");

        assert!(matches!(
            run_job(Request::Rename {
                path: file.clone(),
                name: "after.txt".into(),
            }),
            JobEvent::Done { .. }
        ));
        assert!(!file.exists());
        assert_eq!(read(&base.join("after.txt")), "body");

        // A name with a separator in it would move the file somewhere else
        // entirely, which is not what a rename box is for.
        let renamed = base.join("after.txt");
        for bad in ["", "../escape", "a/b", ".", ".."] {
            assert!(
                matches!(
                    run_job(Request::Rename {
                        path: renamed.clone(),
                        name: bad.into(),
                    }),
                    JobEvent::Failed(_)
                ),
                "{bad:?} should be refused"
            );
        }

        // And it will not quietly clobber something already there.
        fs::write(base.join("taken.txt"), "other").expect("file");
        assert!(matches!(
            run_job(Request::Rename {
                path: renamed,
                name: "taken.txt".into(),
            }),
            JobEvent::Failed(_)
        ));
        assert_eq!(read(&base.join("taken.txt")), "other");

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_move_within_one_filesystem_renames_rather_than_copying() {
        // The inode is the proof: a rename keeps it, a copy-and-delete cannot.
        // This is what makes moving a folder of photographs instant.
        let base = temp_dir("move_rename");
        let (src, dst) = (base.join("src"), base.join("dst"));
        fs::create_dir_all(&src).expect("src");
        fs::create_dir_all(&dst).expect("dst");
        let file = src.join("photo.jpg");
        fs::write(&file, "pixels").expect("file");
        let before = fs::metadata(&file).expect("meta").ino();

        assert!(matches!(
            run_job(Request::Move {
                sources: vec![file.clone()],
                dest_dir: dst.clone(),
            }),
            JobEvent::Done { .. }
        ));

        assert!(!file.exists(), "the source is gone");
        let landed = dst.join("photo.jpg");
        assert_eq!(read(&landed), "pixels");
        assert_eq!(
            fs::metadata(&landed).expect("meta").ino(),
            before,
            "the same inode, so nothing was copied"
        );

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn the_cross_device_fallback_copies_the_whole_tree_before_removing_it() {
        // The fallback taken when `rename` reports EXDEV, driven directly: a real
        // second filesystem cannot be mounted from a test, but the two halves of
        // the operation and the order they must happen in can be.
        let base = temp_dir("move_across");
        let src = base.join("tree");
        fs::create_dir_all(src.join("sub")).expect("dirs");
        fs::write(src.join("a.txt"), "a").expect("file");
        fs::write(src.join("sub/b.txt"), "b").expect("file");

        let mut h = harness();
        let dst = base.join("moved");
        let meta = fs::symlink_metadata(&src).expect("meta");
        move_across(&src, &meta, Landing::Fresh(dst.clone()), &mut h.ctx)
            .expect("the fallback runs");
        assert!(!src.exists(), "the source is gone once the copy is whole");
        assert_eq!(read(&dst.join("a.txt")), "a");
        assert_eq!(read(&dst.join("sub/b.txt")), "b");

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_cross_device_move_that_cannot_copy_leaves_the_source_alone() {
        // The property that matters more than the happy path: if the copy half
        // fails, the delete half must never run, or the move has destroyed the
        // only copy of the data.
        let base = temp_dir("move_across_fail");
        let src = base.join("tree");
        fs::create_dir_all(&src).expect("dirs");
        fs::write(src.join("precious.txt"), "irreplaceable").expect("file");

        let mut h = harness();
        // A destination whose parent does not exist, so the copy cannot start.
        let dst = base.join("no-such-directory/moved");
        let meta = fs::symlink_metadata(&src).expect("meta");
        assert!(
            move_across(&src, &meta, Landing::Fresh(dst), &mut h.ctx).is_err(),
            "the copy must fail for this test to prove anything"
        );
        assert_eq!(
            read(&src.join("precious.txt")),
            "irreplaceable",
            "a failed cross-device move must not remove the source"
        );

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn deleting_removes_a_whole_tree_and_leaves_symlink_targets_alone() {
        let base = temp_dir("delete");
        let tree = base.join("tree");
        fs::create_dir_all(tree.join("sub")).expect("dirs");
        fs::write(tree.join("a.txt"), "a").expect("file");
        fs::write(tree.join("sub/b.txt"), "b").expect("file");
        // A link out of the tree: removing the link must not remove the target.
        let outside = base.join("outside.txt");
        fs::write(&outside, "untouched").expect("file");
        std::os::unix::fs::symlink(&outside, tree.join("link.txt")).expect("symlink");

        assert!(matches!(
            run_job(Request::Delete {
                paths: vec![tree.clone()],
            }),
            JobEvent::Done { .. }
        ));

        assert!(!tree.exists(), "the tree is gone");
        assert_eq!(read(&outside), "untouched", "and the link's target is not");

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn one_failure_does_not_abandon_the_rest_of_the_sources() {
        // A hundred files where one is missing should copy the ninety-nine and
        // then say what went wrong, not stop at the problem.
        let base = temp_dir("partial_failure");
        let (src, dst) = (base.join("src"), base.join("dst"));
        fs::create_dir_all(&src).expect("src");
        fs::create_dir_all(&dst).expect("dst");
        fs::write(src.join("first.txt"), "first").expect("file");
        fs::write(src.join("last.txt"), "last").expect("file");

        let payload = run_job(Request::Copy {
            sources: vec![
                src.join("first.txt"),
                src.join("vanished.txt"),
                src.join("last.txt"),
            ],
            dest_dir: dst.clone(),
        });
        match payload {
            JobEvent::Failed(e) => assert!(
                e.contains("vanished.txt"),
                "the failure should name what failed: {e}"
            ),
            other => panic!(
                "expected a reported failure, got {}",
                match other {
                    JobEvent::Done { .. } => "Done",
                    JobEvent::Cancelled => "Cancelled",
                    _ => "something else",
                }
            ),
        }
        assert_eq!(read(&dst.join("first.txt")), "first");
        assert_eq!(
            read(&dst.join("last.txt")),
            "last",
            "the source after the failing one was still copied"
        );

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn jobs_run_one_at_a_time_in_the_order_they_were_asked_for() {
        let base = temp_dir("queue");
        let (src, dst) = (base.join("src"), base.join("dst"));
        fs::create_dir_all(&src).expect("src");
        fs::create_dir_all(&dst).expect("dst");
        fs::write(src.join("one.txt"), "one").expect("file");
        fs::write(src.join("two.txt"), "two").expect("file");

        let mut jobs = Jobs::new(Writes::Allowed).expect("jobs");
        jobs.submit(Request::Copy {
            sources: vec![src.join("one.txt")],
            dest_dir: dst.clone(),
        })
        .expect("submit");
        jobs.submit(Request::Copy {
            sources: vec![src.join("two.txt")],
            dest_dir: dst.clone(),
        })
        .expect("submit");
        assert_eq!(
            jobs.progress().map(|p| p.queued),
            Some(1),
            "the second waits behind the first"
        );

        assert!(matches!(
            drive(&mut jobs, |_| ConflictReply::Skip),
            JobEvent::Done { .. }
        ));
        assert!(jobs.is_busy(), "the second started when the first ended");
        assert!(matches!(
            drive(&mut jobs, |_| ConflictReply::Skip),
            JobEvent::Done { .. }
        ));
        assert!(!jobs.is_busy(), "and then there was nothing left");

        assert_eq!(read(&dst.join("one.txt")), "one");
        assert_eq!(read(&dst.join("two.txt")), "two");

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_job_cancelled_while_a_prompt_is_up_still_reports_back() {
        // The one place a worker blocks indefinitely. Cancelling drops the reply
        // channel, and the worker must notice that rather than waiting forever for
        // an answer that is not coming.
        let base = temp_dir("cancel_prompt");
        let (src, dst) = (base.join("src"), base.join("dst"));
        fs::create_dir_all(&src).expect("src");
        fs::create_dir_all(&dst).expect("dst");
        fs::write(src.join("a.txt"), "new").expect("file");
        fs::write(dst.join("a.txt"), "old").expect("file");

        let mut jobs = Jobs::new(Writes::Allowed).expect("jobs");
        jobs.submit(Request::Copy {
            sources: vec![src.join("a.txt")],
            dest_dir: dst.clone(),
        })
        .expect("submit");

        // Wait for the prompt, then cancel instead of answering it.
        let mut poll = PollSet::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut prompted = false;
        while !prompted && Instant::now() < deadline {
            poll.clear();
            poll.add(jobs.wake_fd());
            poll.wait(Some(Duration::from_millis(20))).expect("poll");
            jobs.drain_wake();
            while let Some((_, event)) = jobs.try_recv() {
                if matches!(event, JobEvent::Conflict { .. }) {
                    prompted = true;
                }
            }
        }
        assert!(prompted, "the conflict prompt never arrived");
        jobs.cancel();

        assert!(
            matches!(
                drive(&mut jobs, |_| ConflictReply::Skip),
                JobEvent::Cancelled
            ),
            "a job cancelled at a prompt must still resolve"
        );
        assert_eq!(read(&dst.join("a.txt")), "old", "nothing was written");

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn blocked_jobs_refuse_every_request_and_touch_nothing() {
        // The read-only contract: with writes blocked, no kind of request is
        // queued, let alone reaches a worker. Checked against real files, so a
        // gate that let the work start and refused it later shows up as a changed
        // tree. Trash is aimed at a path that does not exist, so if this gate ever
        // breaks, the test cannot put anything in the real trash.
        let base = temp_dir("blocked");
        let file = base.join("keep.txt");
        let dest = base.join("dest");
        fs::write(&file, "untouched").expect("file");
        fs::create_dir_all(&dest).expect("dest");

        let mut jobs = Jobs::new(Writes::Blocked).expect("jobs");
        let requests = [
            Request::Copy {
                sources: vec![file.clone()],
                dest_dir: dest.clone(),
            },
            Request::Move {
                sources: vec![file.clone()],
                dest_dir: dest.clone(),
            },
            Request::Rename {
                path: file.clone(),
                name: "renamed.txt".into(),
            },
            Request::Trash {
                paths: vec![base.join("not-there.txt")],
            },
            Request::Delete {
                paths: vec![file.clone(), dest.clone()],
            },
        ];
        for req in requests {
            let kind = req.kind();
            assert!(jobs.permit(kind).is_err(), "{kind:?} is permitted");
            assert!(jobs.submit(req).is_err(), "{kind:?} was accepted");
            assert!(!jobs.is_busy(), "{kind:?} was queued");
        }

        assert_eq!(read(&file), "untouched");
        let mut names: Vec<String> = fs::read_dir(&base)
            .expect("list")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["dest", "keep.txt"]);
        assert_eq!(fs::read_dir(&dest).expect("list dest").count(), 0);

        fs::remove_dir_all(&base).expect("cleanup");
    }

    /// The names in `dir`, sorted, hidden ones included.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("list")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Part files and stepped-aside entries left in `dir`, which a finished job
    /// of any outcome must not leave.
    fn debris(dir: &Path) -> Vec<String> {
        names(dir)
            .into_iter()
            .filter(|n| n.starts_with(PART_PREFIX) || n.starts_with(ASIDE_PREFIX))
            .collect()
    }

    /// Run one request, counting the conflict prompts and answering each with
    /// `reply`.
    fn run_counting(req: Request, reply: ConflictReply) -> (JobEvent, usize) {
        let mut jobs = Jobs::new(Writes::Allowed).expect("jobs");
        jobs.submit(req).expect("submit");
        let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&asked);
        let event = drive(&mut jobs, move |_| {
            counter.fetch_add(1, Ordering::Relaxed);
            reply.clone()
        });
        (event, asked.load(Ordering::Relaxed))
    }

    fn failure(event: JobEvent) -> String {
        match event {
            JobEvent::Failed(e) => e,
            JobEvent::Done { .. } => panic!("expected a failure, the job reported Done"),
            JobEvent::Cancelled => panic!("expected a failure, the job reported Cancelled"),
            JobEvent::Conflict { .. } => panic!("expected a failure, got a prompt"),
        }
    }

    extern "C" {
        fn mkfifo(path: *const std::ffi::c_char, mode: u32) -> std::ffi::c_int;
    }

    /// Make a real FIFO, the entry whose `open` waits for a writer forever.
    fn make_fifo(path: &Path) {
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("path");
        // SAFETY: `c` is a NUL-terminated path that outlives the call.
        let rc = unsafe { mkfifo(c.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo {}", path.display());
    }

    /// Whether a file made unreadable is in fact unreadable. It is not for root,
    /// and a test that needs a read to fail has nothing to prove there.
    fn unreadable(path: &Path) -> bool {
        fs::set_permissions(path, fs::Permissions::from_mode(0o000)).expect("chmod");
        File::open(path).is_err()
    }

    #[test]
    fn moving_something_into_the_folder_it_is_in_never_touches_it() {
        // The destination is the source. Treated as a conflict, Overwrite would
        // remove "what is in the way" and then find nothing left to move.
        let base = temp_dir("move_onto_self");
        let dir = base.join("dir");
        fs::create_dir_all(dir.join("tree")).expect("dirs");
        fs::write(dir.join("precious.txt"), "only copy").expect("file");
        fs::write(dir.join("tree/inner.txt"), "inner").expect("file");
        // The same directory under another spelling.
        let alias = base.join("alias");
        std::os::unix::fs::symlink(&dir, &alias).expect("symlink");

        for dest in [dir.clone(), alias.clone(), dir.join("tree/..")] {
            let (event, asked) = run_counting(
                Request::Move {
                    sources: vec![dir.join("precious.txt"), dir.join("tree")],
                    dest_dir: dest.clone(),
                },
                ConflictReply::Overwrite,
            );
            let why = failure(event);
            assert!(why.contains("already there"), "{why}");
            assert_eq!(asked, 0, "there is no conflict to ask about");
            assert_eq!(read(&dir.join("precious.txt")), "only copy");
            assert_eq!(read(&dir.join("tree/inner.txt")), "inner");
        }

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn copying_something_into_the_folder_it_is_in_makes_a_numbered_duplicate() {
        // Copying a file onto itself must not truncate it; what a file manager
        // means by it is "duplicate".
        let base = temp_dir("copy_onto_self");
        fs::create_dir_all(base.join("tree")).expect("dirs");
        fs::write(base.join("photo.jpg"), "pixels").expect("file");
        fs::write(base.join("tree/inner.txt"), "inner").expect("file");

        let (event, asked) = run_counting(
            Request::Copy {
                sources: vec![base.join("photo.jpg"), base.join("tree")],
                dest_dir: base.join("tree/.."),
            },
            ConflictReply::Overwrite,
        );
        assert!(matches!(event, JobEvent::Done { items: 2 }));
        assert_eq!(asked, 0, "a duplicate is not a conflict");
        assert_eq!(read(&base.join("photo.jpg")), "pixels");
        assert_eq!(read(&base.join("photo (2).jpg")), "pixels");
        assert_eq!(read(&base.join("tree/inner.txt")), "inner");
        assert_eq!(read(&base.join("tree (2)/inner.txt")), "inner");
        assert!(debris(&base).is_empty());

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn moving_a_file_onto_its_own_hard_link_still_removes_the_source() {
        // `rename` between two names of one inode is defined to do nothing, which
        // would report the move done with the source still sitting there.
        let base = temp_dir("move_hardlink");
        let (src, dst) = (base.join("src"), base.join("dst"));
        fs::create_dir_all(&src).expect("src");
        fs::create_dir_all(&dst).expect("dst");
        fs::write(src.join("a.txt"), "body").expect("file");
        fs::hard_link(src.join("a.txt"), dst.join("a.txt")).expect("link");

        let (event, _) = run_counting(
            Request::Move {
                sources: vec![src.join("a.txt")],
                dest_dir: dst.clone(),
            },
            ConflictReply::Overwrite,
        );
        assert!(matches!(event, JobEvent::Done { .. }));
        assert!(!src.join("a.txt").exists(), "the source name is gone");
        assert_eq!(read(&dst.join("a.txt")), "body");

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_directory_cannot_be_copied_into_itself_through_a_symlink() {
        // The destination's text does not start with the source's, but the
        // directory it names is inside it all the same.
        let base = temp_dir("copy_into_self_link");
        let src = base.join("src");
        fs::create_dir_all(src.join("inner")).expect("dirs");
        std::os::unix::fs::symlink(src.join("inner"), base.join("elsewhere")).expect("symlink");

        let why = failure(run_job(Request::Copy {
            sources: vec![src.clone()],
            dest_dir: base.join("elsewhere"),
        }));
        assert!(why.contains("itself"), "{why}");
        assert_eq!(names(&src.join("inner")), Vec::<String>::new());

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_move_that_overwrites_a_file_replaces_it_in_one_step() {
        let base = temp_dir("move_overwrite");
        let (src, dst) = (base.join("src"), base.join("dst"));
        fs::create_dir_all(&src).expect("src");
        fs::create_dir_all(&dst).expect("dst");
        fs::write(src.join("a.txt"), "new").expect("file");
        fs::write(dst.join("a.txt"), "old").expect("file");
        let moved_inode = fs::metadata(src.join("a.txt")).expect("meta").ino();

        let (event, asked) = run_counting(
            Request::Move {
                sources: vec![src.join("a.txt")],
                dest_dir: dst.clone(),
            },
            ConflictReply::Overwrite,
        );
        assert!(matches!(event, JobEvent::Done { .. }));
        assert_eq!(asked, 1);
        assert_eq!(read(&dst.join("a.txt")), "new");
        assert_eq!(
            fs::metadata(dst.join("a.txt")).expect("meta").ino(),
            moved_inode,
            "renamed over, not copied"
        );
        assert_eq!(names(&dst), ["a.txt"]);
        assert!(!src.join("a.txt").exists());

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_failed_overwriting_move_puts_the_old_directory_back() {
        // The data-loss case: a rename that fails must leave the old destination
        // under its name. Here the source vanishes after it was examined, which
        // fails the rename.
        let base = temp_dir("move_overwrite_fail");
        let src = base.join("incoming");
        let dst = base.join("dst/incoming");
        fs::create_dir_all(&dst).expect("dst");
        fs::write(dst.join("keep.txt"), "irreplaceable").expect("file");
        fs::write(&src, "new").expect("file");
        let meta = fs::symlink_metadata(&src).expect("meta");
        fs::remove_file(&src).expect("vanish");

        let mut h = harness();
        assert!(relocate(&src, &meta, Landing::Replace(dst.clone()), &mut h.ctx).is_err());
        assert_eq!(
            read(&dst.join("keep.txt")),
            "irreplaceable",
            "the directory that was to be replaced is back, contents and all"
        );
        assert_eq!(names(&base.join("dst")), ["incoming"]);

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn overwriting_a_directory_with_a_file_removes_the_directory_only_afterwards() {
        let base = temp_dir("file_over_dir");
        for kind in [JobKind::Copy, JobKind::Move] {
            let (src, dst) = (base.join("src"), base.join("dst"));
            fs::create_dir_all(&src).expect("src");
            fs::create_dir_all(dst.join("thing/sub")).expect("dst");
            fs::write(dst.join("thing/sub/old.txt"), "old").expect("file");
            fs::write(src.join("thing"), "a file now").expect("file");

            let sources = vec![src.join("thing")];
            let dest_dir = dst.clone();
            let req = match kind {
                JobKind::Copy => Request::Copy { sources, dest_dir },
                _ => Request::Move { sources, dest_dir },
            };
            let (event, asked) = run_counting(req, ConflictReply::Overwrite);
            assert!(matches!(event, JobEvent::Done { .. }), "{kind:?}");
            assert_eq!(asked, 1, "{kind:?}");
            assert_eq!(read(&dst.join("thing")), "a file now", "{kind:?}");
            assert_eq!(names(&dst), ["thing"], "{kind:?}: nothing left aside");
            assert_eq!(src.join("thing").exists(), kind == JobKind::Copy);

            fs::remove_dir_all(&src).expect("cleanup");
            fs::remove_dir_all(&dst).expect("cleanup");
        }
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn overwriting_a_file_with_a_directory_swaps_them() {
        let base = temp_dir("dir_over_file");
        let (src, dst) = (base.join("src"), base.join("dst"));
        fs::create_dir_all(src.join("thing")).expect("src");
        fs::create_dir_all(&dst).expect("dst");
        fs::write(src.join("thing/new.txt"), "new").expect("file");
        fs::write(dst.join("thing"), "a file").expect("file");

        let (event, asked) = run_counting(
            Request::Copy {
                sources: vec![src.join("thing")],
                dest_dir: dst.clone(),
            },
            ConflictReply::Overwrite,
        );
        assert!(matches!(event, JobEvent::Done { .. }));
        assert_eq!(asked, 1);
        assert_eq!(read(&dst.join("thing/new.txt")), "new");
        assert_eq!(names(&dst), ["thing"]);

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn copy_and_move_both_merge_a_directory_into_one_already_there() {
        // Overwrite means one thing: a move merges into an existing directory as
        // a copy does, keeping the files the incoming one lacks, and only
        // colliding files are asked about.
        let base = temp_dir("merge");
        for kind in [JobKind::Copy, JobKind::Move] {
            let (src, dst) = (base.join("src"), base.join("dst"));
            fs::create_dir_all(src.join("album/sub")).expect("src");
            fs::create_dir_all(dst.join("album")).expect("dst");
            fs::write(src.join("album/both.txt"), "new").expect("file");
            fs::write(src.join("album/sub/incoming.txt"), "incoming").expect("file");
            fs::write(dst.join("album/both.txt"), "old").expect("file");
            fs::write(dst.join("album/resident.txt"), "resident").expect("file");

            let sources = vec![src.join("album")];
            let dest_dir = dst.clone();
            let req = match kind {
                JobKind::Copy => Request::Copy { sources, dest_dir },
                _ => Request::Move { sources, dest_dir },
            };
            let (event, asked) = run_counting(req, ConflictReply::Overwrite);
            assert!(matches!(event, JobEvent::Done { .. }), "{kind:?}");
            assert_eq!(asked, 1, "{kind:?}: only the colliding file is asked about");
            assert_eq!(read(&dst.join("album/both.txt")), "new", "{kind:?}");
            assert_eq!(
                read(&dst.join("album/resident.txt")),
                "resident",
                "{kind:?}: what the incoming directory never had is left alone"
            );
            assert_eq!(read(&dst.join("album/sub/incoming.txt")), "incoming");
            assert_eq!(
                src.join("album").exists(),
                kind == JobKind::Copy,
                "{kind:?}: a move that emptied the source directory removes it"
            );

            fs::remove_dir_all(&src).expect("cleanup");
            fs::remove_dir_all(&dst).expect("cleanup");
        }
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_merging_move_leaves_what_was_skipped_where_it_was() {
        let base = temp_dir("merge_skip");
        let (src, dst) = (base.join("src"), base.join("dst"));
        fs::create_dir_all(src.join("album")).expect("src");
        fs::create_dir_all(dst.join("album")).expect("dst");
        fs::write(src.join("album/both.txt"), "new").expect("file");
        fs::write(src.join("album/only.txt"), "only").expect("file");
        fs::write(dst.join("album/both.txt"), "old").expect("file");

        let (event, _) = run_counting(
            Request::Move {
                sources: vec![src.join("album")],
                dest_dir: dst.clone(),
            },
            ConflictReply::Skip,
        );
        assert!(matches!(event, JobEvent::Done { .. }));
        assert_eq!(read(&dst.join("album/both.txt")), "old");
        assert_eq!(read(&dst.join("album/only.txt")), "only");
        assert_eq!(
            read(&src.join("album/both.txt")),
            "new",
            "the skipped file was not moved and not lost"
        );
        assert_eq!(names(&src.join("album")), ["both.txt"]);

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_cross_device_overwrite_that_cannot_copy_keeps_the_old_destination() {
        // When the bytes have to travel, the old file is replaced only by the
        // rename of a finished part file, so a copy that fails leaves it exactly
        // as it was.
        let base = temp_dir("across_overwrite_fail");
        let (src, dst) = (base.join("src.txt"), base.join("dst.txt"));
        fs::write(&src, "new").expect("file");
        fs::write(&dst, "old").expect("file");
        let meta = fs::symlink_metadata(&src).expect("meta");
        if !unreadable(&src) {
            fs::remove_dir_all(&base).expect("cleanup");
            return;
        }

        let mut h = harness();
        assert!(move_across(&src, &meta, Landing::Replace(dst.clone()), &mut h.ctx).is_err());
        assert_eq!(read(&dst), "old");
        assert!(src.exists(), "and the source was not removed");
        assert!(debris(&base).is_empty());

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn an_overwrite_cancelled_mid_file_leaves_the_old_destination_whole() {
        // Cancelled once the part file exists, which is as far into the copy as
        // a cancel can land before the rename. The copy is of real bytes rather
        // than a hole, so it is still running when the cancel arrives.
        let base = temp_dir("overwrite_cancel");
        let (src, dst) = (base.join("src"), base.join("dst"));
        fs::create_dir_all(&src).expect("src");
        fs::create_dir_all(&dst).expect("dst");
        fs::write(src.join("big.bin"), vec![7u8; 48 * 1024 * 1024]).expect("file");
        fs::write(dst.join("big.bin"), "old").expect("file");

        let mut jobs = Jobs::new(Writes::Allowed).expect("jobs");
        jobs.submit(Request::Copy {
            sources: vec![src.join("big.bin")],
            dest_dir: dst.clone(),
        })
        .expect("submit");

        let mut poll = PollSet::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut cancelled = false;
        let event = 'job: loop {
            assert!(Instant::now() < deadline, "the job never ended");
            if !cancelled && !debris(&dst).is_empty() {
                jobs.cancel();
                cancelled = true;
            }
            poll.clear();
            poll.add(jobs.wake_fd());
            poll.wait(Some(Duration::from_micros(200))).expect("poll");
            jobs.drain_wake();
            while let Some((_, event)) = jobs.try_recv() {
                match event {
                    JobEvent::Conflict { .. } => jobs.answer(ConflictReply::Overwrite),
                    terminal => break 'job terminal,
                }
            }
        };
        assert!(cancelled, "the copy finished before its part file was seen");
        assert!(matches!(event, JobEvent::Cancelled));
        assert_eq!(read(&dst.join("big.bin")), "old", "the old file is intact");
        assert_eq!(names(&dst), ["big.bin"], "and the part file is gone");

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn pipes_are_reported_and_the_rest_of_the_tree_is_copied() {
        // A FIFO opened like a file blocks until something writes to it, hanging
        // the copy and the job queue behind it.
        let base = temp_dir("special");
        let (src, dst) = (base.join("src"), base.join("dst"));
        fs::create_dir_all(src.join("tree")).expect("src");
        fs::create_dir_all(&dst).expect("dst");
        fs::write(src.join("tree/a.txt"), "a").expect("file");
        fs::write(src.join("tree/z.txt"), "z").expect("file");
        make_fifo(&src.join("tree/pipe"));
        make_fifo(&src.join("tree/second-pipe"));

        let why = failure(run_job(Request::Copy {
            sources: vec![src.join("tree")],
            dest_dir: dst.clone(),
        }));
        assert!(
            why.contains("cannot be copied") && why.contains("1 more failed"),
            "both are reported: {why}"
        );
        assert_eq!(
            names(&dst.join("tree")),
            ["a.txt", "z.txt"],
            "the files either side of them were copied, and they were not"
        );

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_cross_device_move_never_deletes_a_pipe_it_could_not_copy() {
        // A device node or FIFO "copied" as an empty regular file and then
        // removed from the source is a node destroyed.
        let base = temp_dir("special_move");
        let tree = base.join("tree");
        fs::create_dir_all(&tree).expect("tree");
        fs::write(tree.join("a.txt"), "a").expect("file");
        make_fifo(&tree.join("pipe"));
        let meta = fs::symlink_metadata(&tree).expect("meta");

        let mut h = harness();
        let moved = move_across(&tree, &meta, Landing::Fresh(base.join("moved")), &mut h.ctx);
        assert!(moved.is_err(), "the move reports the pipe");
        assert!(
            fs::symlink_metadata(tree.join("pipe"))
                .expect("still there")
                .file_type()
                .is_fifo(),
            "the pipe is where it was"
        );
        assert_eq!(read(&tree.join("a.txt")), "a", "and so is everything else");
        assert!(!base.join("moved/pipe").exists());

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn opening_a_pipe_as_a_source_returns_at_once_with_a_refusal() {
        // The path a FIFO takes when it is swapped in after the entry was
        // examined: the open must not wait for a writer that is never coming.
        let base = temp_dir("open_fifo");
        let pipe = base.join("pipe");
        make_fifo(&pipe);
        let started = Instant::now();
        match open_regular(&pipe) {
            Err(Stop::Failed(why)) => assert!(why.contains("not a regular file"), "{why}"),
            _ => panic!("a pipe was accepted as a file to copy"),
        }
        assert!(started.elapsed() < Duration::from_secs(5));

        // And a symlink swapped in is not followed to whatever it points at.
        fs::write(base.join("secret"), "secret").expect("file");
        std::os::unix::fs::symlink(base.join("secret"), base.join("link")).expect("symlink");
        assert!(open_regular(&base.join("link")).is_err());

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_listing_that_fails_part_way_is_a_failure_not_a_short_list() {
        // What a flaky network filesystem produces: some entries, then an error.
        // Dropping the error returns a list that looks complete, and a
        // cross-device move then deletes a source it only partly copied.
        let base = temp_dir("listing");
        fs::write(base.join("a.txt"), "a").expect("file");
        fs::write(base.join("b.txt"), "b").expect("file");

        let whole = collect_names(fs::read_dir(&base).expect("list"), &base).expect("names");
        assert_eq!(whole.len(), 2);

        let broken = fs::read_dir(&base)
            .expect("list")
            .chain(std::iter::once(Err(std::io::Error::from_raw_os_error(5))));
        match collect_names(broken, &base) {
            Err(Stop::Failed(why)) => assert!(why.contains("could not be listed"), "{why}"),
            _ => panic!("a listing that ended in an error was accepted"),
        }

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_cross_device_move_with_an_unreadable_directory_keeps_the_source() {
        let base = temp_dir("across_unreadable");
        let tree = base.join("tree");
        fs::create_dir_all(tree.join("locked")).expect("tree");
        fs::write(tree.join("locked/inner.txt"), "inner").expect("file");
        fs::write(tree.join("a.txt"), "a").expect("file");
        let meta = fs::symlink_metadata(&tree).expect("meta");
        fs::set_permissions(tree.join("locked"), fs::Permissions::from_mode(0o000)).expect("chmod");
        let locked = fs::read_dir(tree.join("locked")).is_err();

        let mut h = harness();
        let moved = move_across(&tree, &meta, Landing::Fresh(base.join("moved")), &mut h.ctx);
        fs::set_permissions(tree.join("locked"), fs::Permissions::from_mode(0o700)).expect("chmod");
        if locked {
            assert!(moved.is_err(), "an incomplete copy is not a move");
            assert_eq!(read(&tree.join("a.txt")), "a");
            assert_eq!(read(&tree.join("locked/inner.txt")), "inner");
        }

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_symlink_at_the_destination_is_a_conflict_and_is_never_written_through() {
        // A directory copied onto a symlink to a directory is not merged into it:
        // its children would be written through the link, to wherever it points.
        let base = temp_dir("dst_symlink");
        let (src, dst, outside) = (base.join("src"), base.join("dst"), base.join("outside"));
        fs::create_dir_all(src.join("album")).expect("src");
        fs::create_dir_all(&dst).expect("dst");
        fs::create_dir_all(&outside).expect("outside");
        fs::write(src.join("album/a.txt"), "a").expect("file");
        std::os::unix::fs::symlink(&outside, dst.join("album")).expect("symlink");
        let copy = || Request::Copy {
            sources: vec![src.join("album")],
            dest_dir: dst.clone(),
        };

        let (event, asked) = run_counting(copy(), ConflictReply::Skip);
        assert!(matches!(event, JobEvent::Done { .. }));
        assert_eq!(asked, 1, "the link is asked about like any other conflict");
        assert_eq!(names(&outside), Vec::<String>::new());

        let (event, _) = run_counting(copy(), ConflictReply::Overwrite);
        assert!(matches!(event, JobEvent::Done { .. }));
        assert!(
            fs::symlink_metadata(dst.join("album"))
                .expect("album")
                .is_dir(),
            "the link was replaced by the directory"
        );
        assert_eq!(read(&dst.join("album/a.txt")), "a");
        assert_eq!(
            names(&outside),
            Vec::<String>::new(),
            "and nothing went where the link pointed"
        );
        assert_eq!(names(&dst), ["album"]);

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_symlink_overwriting_a_file_replaces_it() {
        let base = temp_dir("symlink_overwrite");
        let (src, dst) = (base.join("src"), base.join("dst"));
        fs::create_dir_all(&src).expect("src");
        fs::create_dir_all(&dst).expect("dst");
        std::os::unix::fs::symlink("target", src.join("link")).expect("symlink");
        fs::write(dst.join("link"), "a file").expect("file");

        let (event, _) = run_counting(
            Request::Copy {
                sources: vec![src.join("link")],
                dest_dir: dst.clone(),
            },
            ConflictReply::Overwrite,
        );
        assert!(matches!(event, JobEvent::Done { .. }));
        assert_eq!(
            fs::read_link(dst.join("link")).expect("a link"),
            Path::new("target")
        );
        assert_eq!(names(&dst), ["link"]);

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_name_at_the_length_limit_can_still_be_copied() {
        // A part name built from the destination's name plus a suffix would push
        // any name within fifteen bytes of the limit over it.
        let base = temp_dir("long_name");
        let (src, dst) = (base.join("src"), base.join("dst"));
        fs::create_dir_all(&src).expect("src");
        fs::create_dir_all(&dst).expect("dst");
        let name = format!("{}.bin", "n".repeat(251));
        fs::write(src.join(&name), "body").expect("file");

        assert!(matches!(
            run_job(Request::Copy {
                sources: vec![src.join(&name)],
                dest_dir: dst.clone(),
            }),
            JobEvent::Done { .. }
        ));
        assert_eq!(read(&dst.join(&name)), "body");

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_private_file_is_private_from_the_first_byte_and_ends_with_its_own_mode() {
        let base = temp_dir("part_mode");
        let src = base.join("secret.key");
        fs::write(&src, "key").expect("file");
        fs::set_permissions(&src, fs::Permissions::from_mode(0o640)).expect("chmod");

        // The part file as the copy makes it, before any byte is in it.
        let mut h = harness();
        let (part, _writer) = create_part(&base, &mut h.ctx).expect("part");
        assert_eq!(
            fs::metadata(&part.path).expect("meta").mode() & 0o777,
            PART_MODE,
            "nobody else can read a copy in progress"
        );
        drop(part);
        assert!(
            debris(&base).is_empty(),
            "an abandoned part file is removed"
        );

        // And the finished copy carries the source's mode, not the part's.
        let dst = base.join("dst");
        fs::create_dir_all(&dst).expect("dst");
        assert!(matches!(
            run_job(Request::Copy {
                sources: vec![src.clone()],
                dest_dir: dst.clone(),
            }),
            JobEvent::Done { .. }
        ));
        assert_eq!(
            fs::metadata(dst.join("secret.key")).expect("meta").mode() & 0o777,
            0o640
        );

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_part_name_already_taken_is_stepped_over_not_written_into() {
        // A symlink planted under the very name the next part file would get,
        // pointing at a file the copy has no business touching.
        let base = temp_dir("part_planted");
        let victim = base.join("victim.txt");
        fs::write(&victim, "untouched").expect("file");
        let mut h = harness();
        let planted = base.join(format!(
            "{PART_PREFIX}{}-{}-{}",
            std::process::id(),
            h.ctx.id,
            h.ctx.temp_seq + 1
        ));
        std::os::unix::fs::symlink(&victim, &planted).expect("symlink");

        let (part, writer) = create_part(&base, &mut h.ctx).expect("part");
        assert_ne!(part.path, planted, "the taken name was not reused");
        writer.write_all_at(b"copied bytes", 0).expect("write");
        assert_eq!(read(&victim), "untouched");

        drop(part);
        fs::remove_dir_all(&base).expect("cleanup");
    }

    /// Set a `user.` attribute, or report that this filesystem has none.
    fn set_user_xattr(path: &Path, name: &std::ffi::CStr, value: &[u8]) -> bool {
        let file = File::open(path).expect("open");
        sys::set_xattr(&file, name, value).is_ok()
    }

    fn user_xattr(path: &Path, name: &std::ffi::CStr) -> Option<Vec<u8>> {
        let file = File::open(path).expect("open");
        sys::get_xattr(&file, name).expect("read attribute")
    }

    #[test]
    fn extended_attributes_travel_with_a_copy_and_with_a_cross_device_move() {
        // Collections keep their metadata in `user.*` attributes. A copy that
        // drops them loses it for the copy; a cross-device move that drops them
        // and then deletes the source loses it for good.
        let base = temp_dir("xattr");
        let (src, dst) = (base.join("src"), base.join("dst"));
        fs::create_dir_all(src.join("album")).expect("src");
        fs::create_dir_all(&dst).expect("dst");
        let photo = src.join("album/photo.jpg");
        fs::write(&photo, "pixels").expect("file");
        if !set_user_xattr(&photo, c"user.xdg.tags", b"holiday,beach") {
            // No user attributes on this filesystem, so nothing to carry.
            fs::remove_dir_all(&base).expect("cleanup");
            return;
        }
        assert!(set_user_xattr(
            &src.join("album"),
            c"user.xdg.comment",
            b"summer"
        ));

        assert!(matches!(
            run_job(Request::Copy {
                sources: vec![src.join("album")],
                dest_dir: dst.clone(),
            }),
            JobEvent::Done { .. }
        ));
        assert_eq!(
            user_xattr(&dst.join("album/photo.jpg"), c"user.xdg.tags").as_deref(),
            Some(&b"holiday,beach"[..])
        );
        assert_eq!(
            user_xattr(&dst.join("album"), c"user.xdg.comment").as_deref(),
            Some(&b"summer"[..]),
            "a directory's attributes come too"
        );

        let meta = fs::symlink_metadata(src.join("album")).expect("meta");
        let mut h = harness();
        move_across(
            &src.join("album"),
            &meta,
            Landing::Fresh(base.join("moved")),
            &mut h.ctx,
        )
        .expect("the move");
        assert!(!src.join("album").exists());
        assert_eq!(
            user_xattr(&base.join("moved/photo.jpg"), c"user.xdg.tags").as_deref(),
            Some(&b"holiday,beach"[..])
        );
        assert_eq!(
            user_xattr(&base.join("moved"), c"user.xdg.comment").as_deref(),
            Some(&b"summer"[..])
        );

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_user_attribute_the_destination_cannot_keep_fails_a_move_but_not_a_copy() {
        // The destination here is a character device, which the kernel refuses
        // `user.*` attributes on: a real "cannot keep it" without needing a
        // second filesystem. A copy shrugs, as `cp` does; a move must stop while
        // the source still exists.
        let base = temp_dir("xattr_refused");
        let src = base.join("tagged.txt");
        fs::write(&src, "body").expect("file");
        if !set_user_xattr(&src, c"user.xdg.tags", b"keep-me") {
            fs::remove_dir_all(&base).expect("cleanup");
            return;
        }
        let from = File::open(&src).expect("open");
        let to = File::open("/dev/null").expect("open /dev/null");

        assert!(carry_xattrs(&from, &to, &src, Purpose::Duplicate).is_ok());
        match carry_xattrs(&from, &to, &src, Purpose::Relocate) {
            Err(Stop::Failed(why)) => assert!(why.contains("user.xdg.tags"), "{why}"),
            _ => panic!("a move went ahead without the attribute"),
        }

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_sparse_file_is_copied_as_a_sparse_file() {
        // A disk image that is mostly hole arrives sparse, not fully written out.
        let base = temp_dir("sparse");
        let (src, dst) = (base.join("src"), base.join("dst"));
        fs::create_dir_all(&src).expect("src");
        fs::create_dir_all(&dst).expect("dst");
        const LEN: u64 = 64 * 1024 * 1024;
        let image = File::create(src.join("disk.img")).expect("create");
        image.set_len(LEN).expect("size");
        image.write_all_at(b"middle", LEN / 2).expect("write");
        image.write_all_at(b"start", 0).expect("write");
        drop(image);
        let stored = fs::metadata(src.join("disk.img")).expect("meta").blocks();

        assert!(matches!(
            run_job(Request::Copy {
                sources: vec![src.join("disk.img")],
                dest_dir: dst.clone(),
            }),
            JobEvent::Done { .. }
        ));

        let copy = dst.join("disk.img");
        let meta = fs::metadata(&copy).expect("meta");
        assert_eq!(
            meta.len(),
            LEN,
            "a trailing hole still counts towards the size"
        );
        let file = File::open(&copy).expect("open");
        let mut buf = [0u8; 6];
        file.read_exact_at(&mut buf, LEN / 2).expect("read");
        assert_eq!(&buf, b"middle");
        file.read_exact_at(&mut buf[..5], 0).expect("read");
        assert_eq!(&buf[..5], b"start");
        file.read_exact_at(&mut buf, LEN - 6).expect("read");
        assert_eq!(buf, [0u8; 6], "the hole reads as zeros");
        // Only meaningful where the filesystem kept the source sparse to begin
        // with; a reflink shares the source's blocks and passes trivially.
        if stored < LEN / 512 / 4 {
            assert!(
                meta.blocks() < LEN / 512 / 4,
                "the copy occupies {} blocks for a {stored}-block source",
                meta.blocks()
            );
        }

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn trashing_counts_each_tree_once_and_ends_on_the_total() {
        let base = temp_dir("trash_job");
        let trash = trash::Trash::at(base.join("Trash"), 1000);
        fs::create_dir_all(base.join("tree/sub")).expect("tree");
        fs::write(base.join("tree/sub/a.bin"), vec![0u8; 1000]).expect("file");
        fs::write(base.join("b.bin"), vec![0u8; 234]).expect("file");

        let mut h = harness();
        trash_into(&trash, &[base.join("tree"), base.join("b.bin")], &mut h.ctx)
            .expect("trash them");
        assert_eq!(h.ctx.total_bytes, 1234);
        assert_eq!(
            h.ctx.done_bytes, 1234,
            "each source advanced by its own size"
        );
        assert_eq!(h.ctx.items_done, 2);
        assert_eq!(read(&base.join("Trash/files/b.bin")).len(), 234);
        assert!(base.join("Trash/files/tree/sub/a.bin").exists());

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_reply_for_all_means_the_same_however_it_is_wrapped() {
        assert_eq!(
            ConflictReply::Overwrite.settle(),
            (Decision::Overwrite, Scope::ThisOne)
        );
        let nested = ConflictReply::ApplyToAll(Box::new(ConflictReply::ApplyToAll(Box::new(
            ConflictReply::RenameNew,
        ))));
        assert_eq!(nested.settle(), (Decision::KeepBoth, Scope::AllRemaining));
    }

    #[test]
    fn overwrite_never_replaces_what_the_source_is_inside_of() {
        // Copying `b/d/d` into `b` merges into `b/d`, where its entry `d`
        // collides with `b/d/d`, the source directory itself. Replacing that
        // would delete the source mid-copy, files not yet copied included.
        let base = temp_dir("replace_ancestor");
        let source = base.join("d/d");
        fs::create_dir_all(&source).expect("dirs");
        fs::write(source.join("d"), "inner").expect("file");
        fs::write(source.join("keep1.txt"), "one").expect("file");
        fs::write(source.join("keep2.txt"), "two").expect("file");

        let (event, _) = run_counting(
            Request::Copy {
                sources: vec![source.clone()],
                dest_dir: base.clone(),
            },
            ConflictReply::ApplyToAll(Box::new(ConflictReply::Overwrite)),
        );
        let why = failure(event);
        assert!(why.contains("holds what is being copied"), "{why}");
        assert_eq!(read(&source.join("d")), "inner");
        assert_eq!(read(&source.join("keep1.txt")), "one");
        assert_eq!(read(&source.join("keep2.txt")), "two");
        assert!(debris(&base.join("d")).is_empty());

        // The same with a file: `b/d/d` copied into `b` must not replace `b/d`.
        fs::remove_dir_all(&source).expect("reset");
        fs::write(&source, "a file").expect("file");
        fs::write(base.join("d/sibling.txt"), "sibling").expect("file");
        let (event, _) = run_counting(
            Request::Copy {
                sources: vec![source.clone()],
                dest_dir: base.clone(),
            },
            ConflictReply::Overwrite,
        );
        assert!(failure(event).contains("holds what is being copied"));
        assert_eq!(read(&source), "a file");
        assert_eq!(read(&base.join("d/sibling.txt")), "sibling");

        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn a_failed_copy_onto_a_vacated_name_leaves_the_name_free_again() {
        // The cross-device half of replacing a file with a directory. The old
        // file has stepped aside and can only come back if the copy that failed
        // takes its partial directory away with it.
        let base = temp_dir("vacated");
        let tree = base.join("tree");
        fs::create_dir_all(&tree).expect("tree");
        fs::write(tree.join("a.txt"), "a").expect("file");
        make_fifo(&tree.join("pipe"));
        let meta = fs::symlink_metadata(&tree).expect("meta");
        let dst = base.join("dst");

        let mut h = harness();
        assert!(copy_into_vacated(&tree, &meta, &dst, &mut h.ctx).is_err());
        assert!(
            fs::symlink_metadata(&dst).is_err(),
            "nothing is left on the name"
        );
        assert_eq!(read(&tree.join("a.txt")), "a", "and the source is whole");

        fs::remove_dir_all(&base).expect("cleanup");
    }
}
