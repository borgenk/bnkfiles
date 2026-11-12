//! The application: one self-contained [`State`] owning the Wayland connection,
//! the fonts, the GPU presentation path, and the view the user is looking at.
//!
//! `bring_up` walks the
//! connection from a registry to a configured, GPU-backed toplevel, then
//! `run_until` is the event loop — drain the socket and the decode worker,
//! service the timers, present a frame if one is owed, and block in `poll` until
//! something wakes us.
//!
//! Nothing here is global. Two `State`s could be constructed side by side (each
//! with its own connection, fonts, and GPU device) without either noticing; we do
//! not build a multi-window UI, we simply never paint ourselves into the corner
//! where one is impossible.
//!
//! ```text
//!   scan (worker thread) ──batches──eventfd──▶ handle_scans ──▶ listing[]
//!                                                                  │
//!                                       explorer rows ◀────────────┤
//!                                            images[] ◀────────────┘
//!              │
//!              ├─request──▶ loader (worker pool) ──eventfd──▶ handle_decodes
//!              ▼                                                    │
//!           ImageStore  ◀── decoded ─────────────────────────────────
//!              │
//!              ▼
//!   viewer / grid / list ─────▶ DisplayList ──▶ gpu::build_frame ──▶ Vulkan
//! ```
//!
//! Neither worker's result is read from the socket: both wake the loop through an
//! eventfd in the poll set, and the next turn drains their channels.
//!
//! This file holds the state, the event loop and the geometry everything else
//! measures against. The rest of `State` is split by what it is about:
//! `wayland` (bring-up and dispatch), `listing` (scan, watch, saved
//! thumbnails), `decode` (loader requests and the image store), `nav`
//! (moving around), `fileops` (jobs and prompts), `input`, `clipboard`,
//! `paint` and `present`.

mod clipboard;
mod commands;
mod decode;
mod fileops;
mod input;
mod listing;
mod nav;
mod paint;
mod present;
mod probe;
mod shape;
mod wayland;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::explorer::ExplorerLayout;
use crate::files::{self, FsEntry, Opening};
use crate::filter::Filter;
use crate::grid::{self, Layout};
use crate::header::{Control, HeaderKind, HeaderLayout};
use crate::history::History;
use crate::jobs::{Jobs, Writes};
use crate::loader::Loader;
use crate::masonry;
use crate::menu::Menu;
use crate::notice::{Level, Notice};
use crate::order::Order;
use crate::places::Place;
use crate::platform::conn::{Connection, Fill};
use crate::platform::error::{Error, Result};
use crate::platform::ffi::{self, PollSet};
use crate::platform::freetype::{FontConfig, FontSelection, Fonts};
use crate::platform::geom::{logical_to_device, Rect, Scale};
use crate::platform::inotify::{Inotify, Wd};
use crate::platform::protocol::{self, wl_display};
use crate::platform::scroll::Scrollbar;
use crate::platform::wire::Arg;
use crate::platform::xkb::Xkb;
use crate::prompt::Prompt;
use crate::regions::{Panels, Regions};
use crate::render::image::{Image, ImageKey, ImageStore};
use crate::scan::{Debounce, Scanner};
use crate::select::Selection;
use crate::settings::{self, Settings};
use crate::sidebar::Section;
use crate::thumbcache::{self, Cached};
use crate::unreadable::Unreadable;
use crate::usage::{self, Usage};
use crate::viewer::Viewer;

use clipboard::Clipboard;
use input::KeyRepeat;
use present::GpuPresentation;

const APP_ID: &str = "io.github.borgenk.BnkFiles";
/// Ids at or above this belong to the server; only client ids are recycled.
const SERVER_ID_BASE: u32 = 0xff00_0000;
/// A compositor could otherwise configure us to a size no GPU will allocate.
/// Matches Vulkan's usual `maxImageDimension2D`; the device's own limit, once
/// there is a device, bounds it further (see `State::max_dimension`).
const MAX_DIMENSION: u32 = 16384;
/// The smallest window the layout still works in, in surface units.
const MIN_WIDTH: u32 = 200;
const MIN_HEIGHT: u32 = 150;
const SCALE_120_UNITY: u32 = 120;
/// How long after a picture failed to upload before the frame is tried again.
/// Long enough that a device which is simply out of memory is not asked at the
/// display's refresh rate.
const UPLOAD_RETRY_DELAY: Duration = Duration::from_secs(1);

/// How many images ahead, in the direction of travel, to decode before they are
/// asked for (one image the other way). Deep enough that holding an arrow key stays
/// ahead of the decodes, shallow enough not to thrash memory.
const PRELOAD_AHEAD: usize = 3;
/// How long directory changes are let settle before the listing is re-read.
///
/// An unpack, a large copy, or a `git checkout` produces hundreds of events over a
/// second or two; one rescan at the end of the burst is the right answer to all of
/// them, and 100 ms is short enough that a single `touch` still feels immediate.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(100);
/// The drain buffer for inotify events. Each record is 16 bytes plus a padded
/// name, so this holds a hefty burst without a second read.
const WATCH_BUF: usize = 8 * 1024;
/// How long after the last thumbnail lands the cache is written.
///
/// Long enough that scrolling through a folder writes once at the end rather than
/// after every decode, short enough that little is lost if the process is killed
/// rather than closed.
const THUMB_SAVE_DELAY: Duration = Duration::from_secs(2);
/// Whether bnkfiles may write to disk. Blocked: rename, trash, and delete are
/// refused before they start, and the thumbnail cache is not saved. The settings
/// file (window size, last folder, pinned places) is written either way: it is the
/// app's own config, not anyone's files. The code behind the rest stays compiled
/// and tested, so allowing writes is this one line.
const WRITES: Writes = Writes::Blocked;
/// How much of a file to read to identify it: enough for every rule in the magic
/// table, including tar's signature at offset 257.
const SNIFF_BYTES: usize = 512;

/// The resident full-image memory eviction keeps under. Full ARGB images are ~100 MB
/// at 24 MP, so a count of neighbours is a blunt bound; this keeps a small working
/// set whatever the image size (thumbnails are small and kept besides this). The
/// current and shown images are always kept, even if one alone exceeds it.
const RESIDENT_FULL_BUDGET: usize = 384 * 1024 * 1024;

/// Text sizes in logical pixels: a thumbnail's label, the status bar, and the
/// detail list's rows. The fonts are opened at the *device* pixels these come
/// to, and reopened when the scale changes.
pub const FONT_LABEL: u32 = 13;
pub const FONT_STATUS: u32 = 16;
pub const FONT_BROWSE: u32 = 18;
const FONT_SIZES: [u32; 3] = [FONT_LABEL, FONT_STATUS, FONT_BROWSE];

/// Open the window on whatever the command line named, and run until it closes.
///
/// The argument arrives already resolved ([`files::resolve_arg`]), so a path that
/// is not there has failed before this function, and before any Wayland
/// connection.
pub fn run(opening: Option<Opening>) -> Result<()> {
    let settings = Settings::load();
    let mut state = State::new(&settings)?;
    state.open_from_arg(opening, &settings);
    let result = state.bring_up();
    // On the way out, the thumbnails of wherever they ended up.
    state.save_thumbs();
    if state.stats {
        state.report_pipeline_stats();
    }
    // Persist the window size and where the user was looking (best-effort).
    let _ = Settings::update(|settings| state.record_session(settings));
    result
}

/// `--gpu-probe`: report what the compositor and GPU offer the dmabuf
/// presentation path, without opening a window. Its report *is* its output, so it
/// prints unconditionally.
pub fn gpu_probe() -> Result<()> {
    State::new(&Settings::default())?.probe_dmabuf()
}

/// What a masonry layout was made for: the width it fills, the box model, and
/// the revisions of the rows and of the shapes it was laid out from.
#[derive(Clone, Copy, PartialEq, Eq)]
struct MasonryFor {
    width: i32,
    column: i32,
    label: i32,
    gap: i32,
    pad: i32,
    rows_rev: u64,
    aspects_rev: u64,
}

/// The active view: the explorer in one of its two layouts, or one image.
///
/// [`List`](ViewMode::List) and [`Grid`](ViewMode::Grid) are the same view of the
/// same directory over the same selection, drawn two ways; only the viewer is a
/// different thing to be looking at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ViewMode {
    List,
    Grid,
    Viewer,
}

impl ViewMode {
    /// Whether this is the explorer rather than the viewer.
    fn is_explorer(self) -> bool {
        matches!(self, ViewMode::List | ViewMode::Grid)
    }
}

/// Which pane the keyboard is talking to. Tab cycles it; every pane is reachable
/// and operable without the pointer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Entries,
    Sidebar,
    Path,
    /// The filter field in the header: typing goes into it, not to the
    /// single-letter commands.
    Filter,
    /// The inspector, while it is shown. Read-only: it takes the focus so Tab
    /// reaches every region, and gives it back on Escape.
    Inspector,
}

/// Where to put the cursor once the directory being read has been read.
///
/// A scan runs on a worker, so "open this folder and select that thing" cannot be
/// two statements in a row: the thing does not exist yet when the folder is asked
/// for. Both forms are resolved in [`State::apply_pending_focus`].
enum PendingFocus {
    /// The first row: a folder just walked into.
    First,
    /// A gallery index, from a restored session that saved a number.
    Index(usize),
    /// A path: the directory just stepped out of, or the image just closed.
    /// A folder that is not there any more puts the cursor where it was.
    Path(PathBuf),
}

/// Counters for the decode pipeline: the felt-performance signals a benchmark
/// cannot capture, read live by the debug overlay (`d`) and dumped on exit under
/// `BNKFILES_STATS`. Kept always (the events fire at human speed, so the counting
/// costs nothing); they are what turns "scrolling feels like it starves" into a
/// number.
#[derive(Default)]
struct PipelineStats {
    /// Each time the viewer wanted to display an image: was it already resident (a
    /// hit, preload paid off) or did the user wait on a decode (a miss)? The
    /// readiness signal navigation and preload move.
    show_hits: u64,
    show_misses: u64,
    /// Decodes that came back and were kept.
    full_decoded: u64,
    thumb_decoded: u64,
    /// Requests the loader cancelled before decoding (their generation was already
    /// stale when a worker reached them): work the priority queue and cancellation
    /// saved us from doing at all.
    queued_skipped: u64,
    /// Decodes that ran to completion but landed for a folder the user had already
    /// left (generation mismatch): work that could not be cancelled in time because
    /// it was already in flight. Together with `queued_skipped`, the loader's wasted
    /// work.
    active_stale_completed: u64,
    /// Decodes that were kept, then evicted without ever being drawn: a thumbnail
    /// decoded for a row the user scrolled past before it landed. Focus re-scoring
    /// decodes the on-screen rows first, but a scrolled-past request is
    /// de-prioritized, not cancelled.
    decoded_then_unused: u64,
    /// High-water marks: in-flight requests at once, and resident decoded bytes.
    peak_inflight: usize,
    peak_resident_bytes: usize,
    /// Directory scans that ran to the end, the entries they described in total,
    /// and the biggest single directory read. The scan pipeline's counterpart to
    /// the decode counters above: what the loop would have blocked on if the
    /// scanner were not a thread.
    scans_done: u64,
    scan_entries: u64,
    scan_largest: usize,
    /// Rescans a directory watch asked for: how much work living in a busy
    /// directory costs, and whether the debounce is coalescing what it should.
    rescans: u64,
    /// Thumbnails read back from disk, and the ones actually used rather than
    /// decoded again: what the cache is buying.
    thumbs_from_cache: u64,
    thumbs_reused: u64,
}

/// The first bytes of a file, for identifying it. Short reads are looped over, so
/// a filesystem that hands back the head of a file in pieces still produces enough
/// of it for the magic table; anything unreadable comes back empty, and an empty
/// head simply falls through to the extension.
///
/// Only a regular file is read. This runs on the main thread, and a FIFO or a
/// device would hold it for as long as nobody writes: the open is non-blocking so
/// it returns for those too, and what the open descriptor turns out to be decides.
fn read_head(path: &Path) -> Vec<u8> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let opened = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(ffi::O_NONBLOCK)
        .open(path);
    let Ok(mut file) = opened else {
        return Vec::new();
    };
    if !file.metadata().is_ok_and(|meta| meta.is_file()) {
        return Vec::new();
    }
    let mut buf = vec![0u8; SNIFF_BYTES];
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    buf.truncate(filled);
    buf
}

/// The store key the viewer should draw, given the target image's full and thumbnail
/// keys and the last-shown fallback: the first of the three that is actually
/// resident. Pure, so the preference order is testable without a window — which
/// matters, because the order *is* the behaviour (a blurry right picture beats a
/// sharp wrong one).
fn pick_viewer_key(
    store: &ImageStore,
    full: Option<ImageKey>,
    thumb: Option<ImageKey>,
    fallback: Option<ImageKey>,
) -> Option<ImageKey> {
    [full, thumb, fallback]
        .into_iter()
        .flatten()
        .find(|key| store.contains(key))
}

/// Compositor scale tracking and the objects that deliver it.
///
/// Two mechanisms, in priority order. The *fractional* path is primary: a
/// `wp_fractional_scale_v1` reports `preferred_scale` in 120ths and a
/// `wp_viewport` maps the device-pixel buffer down to the logical window size, so
/// a 125% or 150% display renders pixel-exact. The *integer* path is the fallback
/// for compositors with no fractional-scale global: each `wl_output`'s `scale`,
/// the surface's being the max over the outputs it spans, and
/// `wl_surface.set_buffer_scale`. `viewport != 0` means the fractional path is
/// live.
pub struct Scaling {
    /// Effective scale as 120ths (120 = 1.0); drives font pixels and buffer size.
    factor_120: u32,
    /// Logical window size (surface-local px) from the last xdg configure. The
    /// device buffer is this scaled up; the viewport (or buffer scale) maps back.
    logical: (u32, u32),
    /// Whether the surface's geometry + viewport/buffer-scale state has been
    /// resent since `factor_120` or `logical` last changed.
    synced: bool,
    fractional_manager: Option<u32>,
    viewporter: Option<u32>,
    /// Per-surface objects the managers mint (0 when unavailable).
    fractional_scale: u32,
    viewport: u32,
    /// Integer fallback: each bound `wl_output`'s scale, and the ids the surface
    /// currently spans. Unused once the fractional path is live.
    outputs: Vec<(u32, i32)>,
    entered: Vec<u32>,
}

impl Scaling {
    fn new(logical: (u32, u32)) -> Self {
        Self {
            factor_120: SCALE_120_UNITY,
            logical,
            synced: false,
            fractional_manager: None,
            viewporter: None,
            fractional_scale: 0,
            viewport: 0,
            outputs: Vec::new(),
            entered: Vec::new(),
        }
    }

    /// Whether the fractional path is live (a viewport was created).
    fn is_fractional(&self) -> bool {
        self.viewport != 0
    }

    /// The integer scale the surface spans: the max over the outputs it is on.
    fn integer_scale(&self) -> i32 {
        self.entered
            .iter()
            .filter_map(|id| self.outputs.iter().find(|(oid, _)| oid == id))
            .map(|(_, scale)| *scale)
            .max()
            .unwrap_or(1)
            .max(1)
    }
}

pub struct State {
    conn: Connection,
    /// The font files chosen at startup, reopened at each new scale.
    font_selection: FontSelection,
    fonts: Fonts,
    xkb: Xkb,
    poll_set: PollSet,

    /// Object-id allocator. Ids start at 2 (1 is wl_display); ids the server
    /// retires via `delete_id` are recycled so a long session cannot run the
    /// space up.
    next_id: u32,
    free_ids: Vec<u32>,
    registry: u32,
    compositor: Option<u32>,
    wm_base: Option<u32>,
    seat: Option<u32>,
    /// The version the seat was bound at, which decides whether its input objects
    /// have a `release` request.
    seat_version: u32,
    decoration_manager: Option<u32>,
    cursor_shape_manager: Option<u32>,
    /// The clipboard's manager global, and the per-seat device the copy path drives
    /// (`0` until `bring_up` creates it, or forever if the compositor has neither).
    data_device_manager: Option<u32>,
    data_device: u32,
    surface: u32,
    xdg_surface: u32,
    toplevel: u32,
    decoration: u32,
    keyboard: u32,
    pointer: u32,
    cursor_shape_device: u32,
    /// The last `wl_pointer.enter` serial (a shape request must carry one) and the
    /// shape currently set, so an unchanged shape is not resent every motion.
    pointer_enter_serial: u32,
    pointer_shape: u32,

    /// Surface size in *device* pixels: what the buffers are allocated at.
    width: u32,
    height: u32,
    scaling: Scaling,
    presentation: GpuPresentation,
    pending_configure: Option<u32>,
    pending_size: Option<(u32, u32)>,
    pending_sync: Option<u32>,
    frame_callback: u32,
    configured: bool,
    closed: bool,
    dirty: bool,
    stats: bool,
    /// Whether to print the state after each frame (`BNKFILES_PROBE`), for the
    /// UI tests to wait on. See `probe.rs`.
    probe: bool,
    /// Fullscreen as the *compositor* reports it in the configure states, not as
    /// we last asked for it: it can be entered from a compositor keybinding, and a
    /// local toggle would desync the F key from what is on screen.
    fullscreen: bool,

    /// The folder's images and the index the user has navigated to. `shown` is the
    /// index actually on screen, which lags `index` only while a not-yet-decoded
    /// image is still being worked on.
    images: Vec<PathBuf>,
    index: usize,
    shown: Option<usize>,
    /// The decoded images the renderer can draw: full images under a file's path,
    /// thumbnails under a distinct key for the same file.
    store: ImageStore,
    /// Store keys with a decode in flight, so a picture is not requested twice. Keyed
    /// by the store key (a path for a full image, a size-tagged key for a thumbnail),
    /// so an old-size thumbnail request and a new-size one are distinct entries and a
    /// stale completion clears only its own key.
    inflight: HashSet<ImageKey>,
    /// Store keys decoded but not yet drawn, for the `decoded_then_unused` waste
    /// counter: a key removed here by eviction rather than by being drawn was work
    /// spent on something the user never saw.
    undrawn: HashSet<ImageKey>,
    /// Bumped by `open_images`: a decode carries the generation it was requested
    /// for, and a result from an older one is dropped (its index means something
    /// else now).
    generation: u64,
    loader: Loader,
    current_dir: Option<PathBuf>,

    /// The directory being read, its entries as far as the scan has got, and the
    /// explorer's projection of them. `scan_generation` is the scanner's alone: the
    /// listing and the gallery are different things with different lifetimes, and
    /// sharing one counter would have an explorer navigation cancel the decode the
    /// viewer is waiting on.
    scanner: Scanner,
    scan_dir: PathBuf,
    scan_generation: u64,
    listing: Vec<FsEntry>,
    /// The listing a refresh of the same directory is reading. It replaces
    /// `listing` only once it is whole, so the rows on screen, the selection over
    /// them and their thumbnails stay put while it fills.
    refresh: Option<Vec<FsEntry>>,
    /// The listing entries the explorer shows, by index, in display order. A
    /// projection: hidden entries and names the filter refuses are left out
    /// here, never dropped from the model, so the hidden toggle and the filter
    /// are a re-projection rather than a rescan.
    rows: Vec<usize>,
    /// Bumped whenever [`Self::rows`] is rebuilt, so what was laid out for the
    /// old rows is known to be stale.
    rows_rev: u64,
    /// How many rows there would be with no filter: the `N` of `n of N`.
    rows_total: usize,
    /// The text in the header's filter field. Belongs to the folder it was
    /// typed in, and is emptied on going somewhere else.
    filter: Filter,
    /// The order the listing is in. The scan worker sorts by it, so changing
    /// it re-reads the folder rather than re-sorting it here.
    order: Order,
    /// Bring the cursor into view when the re-read under way is swapped in.
    /// Set by an order change, which moves every row; a watch refresh leaves
    /// the view where the user put it.
    reveal_on_adopt: bool,
    /// Whether the current scan has reported a terminal message, and why the
    /// directory could not be read, or not to the end, when it could not.
    scan_done: bool,
    unreadable: Option<Unreadable>,
    /// Where to put the cursor once the scan lands.
    pending_focus: Option<PendingFocus>,
    /// The thumbnails saved for the directory being shown, by file name, as the
    /// scan worker read them back. Consumed by `request_thumb`, which prefers a
    /// matching one to decoding the picture again.
    saved_thumbs: std::collections::HashMap<std::ffi::OsString, Cached>,
    /// The shape, width over height, of each picture in the folder whose
    /// thumbnail has been seen, saved or decoded, by file name. What masonry is
    /// laid out from; a picture not seen yet is laid out square.
    aspects: std::collections::HashMap<std::ffi::OsString, f32>,
    /// Bumped whenever [`Self::aspects`] gains or changes a shape.
    aspects_rev: u64,
    /// The masonry last laid out, and for what. Laying out is linear in
    /// the folder and the layout is asked for many times a frame, so it is done
    /// again only when what it was laid out for changes.
    masonry: std::cell::RefCell<Option<(MasonryFor, std::rc::Rc<masonry::Masonry>)>>,
    /// The thumbnail shape chosen for folders, a folder's own or its nearest
    /// ancestor's applying. A folder with none is [`masonry::Shape::Auto`].
    shapes: Vec<(PathBuf, masonry::Shape)>,
    /// The shapes chosen this session, for the settings file on the way out.
    shapes_chosen: Vec<(PathBuf, masonry::Shape)>,
    /// The shape for the folder on show, from [`Self::shapes`].
    shape_here: masonry::Shape,
    /// What [`masonry::Shape::Auto`] settled on for this folder, once enough
    /// of its pictures' shapes were known to say. Asked once a visit, so the
    /// grid does not change under the user as more thumbnails come in.
    auto_masonry: Option<bool>,
    /// Where saved thumbnails are written. `None` on a machine with no cache
    /// directory, which simply decodes afresh every time.
    thumb_writer: Option<thumbcache::Writer>,

    /// The watch on the directory being shown, and the deadline its events set.
    ///
    /// `None` when inotify could not be opened at all, in which case the explorer
    /// simply does not refresh itself; everything else still works. The buffer is
    /// kept so a drain allocates nothing.
    watch: Option<Inotify>,
    watch_wd: Option<Wd>,
    /// The directory the watch is on, so a rescan of the same one does not re-arm
    /// it (see `watch_dir`).
    watched: Option<PathBuf>,
    watch_buf: Vec<u8>,
    rescan: Debounce,
    /// Set when a frame could not upload a picture, to present again once the
    /// renderer has had the chance to make room.
    retry_uploads: Debounce,
    /// Set when a thumbnail lands, cleared when the cache is written. Saving is
    /// debounced behind it, so a directory filling in writes its cache once when
    /// it settles rather than after every decode.
    thumbs_unsaved: bool,
    save_thumbs_at: Debounce,
    /// The notification line: what the status bar shows instead of its usual
    /// contents until it ages out, such as what came of opening a file or of a
    /// job, or that the folder on screen went away.
    notice: Option<Notice>,

    /// Copies, moves, renames and deletions, each on its own worker thread. The
    /// loop starts them, draws their progress, and answers their questions; it
    /// never moves a byte itself.
    jobs: Jobs,
    /// The question on screen, if the window is asking one. While it is up the
    /// entry list takes no keys: a rename, a conflict, and a permanent delete all
    /// need an answer before anything else can sensibly happen.
    prompt: Option<Prompt>,
    /// The command list, while it is up. Modal, like the prompt.
    menu: Option<Menu>,
    /// A conflict the running job raised while another prompt was up. The worker
    /// waits on it either way; it is asked once the prompt on screen is answered,
    /// rather than replacing a name the user is halfway through typing.
    deferred_conflict: Option<PathBuf>,

    /// The explorer: which rows are selected, where each layout is scrolled, and
    /// which pane has the keyboard. One selection across both layouts, because
    /// they are one view of one directory.
    sel: Selection,
    list_scroll: f32,
    grid_scroll: f32,
    explorer_bar: Scrollbar,
    focus: Focus,
    show_hidden: bool,
    /// The places sidebar, the row highlighted in it, and the folders pinned there.
    places: Vec<Place>,
    /// The sidebar row the keyboard is on: an index into
    /// [`crate::sidebar::rows`], headers included.
    place_sel: usize,
    /// `/proc/self/mounts` as last read, so a change to the collections can
    /// rebuild the places without reading it again.
    mounts_text: String,
    /// The sidebar sections folded shut.
    folded: Vec<Section>,
    /// Folders seen with a `.bnkfiles/` marker in them.
    collections: Vec<PathBuf>,
    /// What this window learned and forgot about collections, applied to the
    /// settings file's list on exit rather than replacing it, so a window
    /// closing last does not undo what another one learned.
    collections_learned: Vec<PathBuf>,
    collections_forgotten: Vec<PathBuf>,
    /// How full each mounted volume is, and which places' folders are gone,
    /// as the usage worker last answered.
    usage: Usage,
    fills: std::collections::HashMap<PathBuf, usage::Fill>,
    gone_places: HashSet<PathBuf>,
    pinned: Vec<PathBuf>,
    /// The path-bar segment the keyboard is on, while the path has the focus.
    path_sel: usize,
    /// Where the explorer has been, for the back and forward buttons and keys.
    history: History,
    /// The header button under the pointer, drawn lit. `None` when the pointer
    /// is elsewhere or outside the window.
    hover: Option<Control>,
    /// Which Alt keys are down (bit 0 left, bit 1 right). The xkb wrapper
    /// answers only for Shift and Ctrl, so Alt+arrow is told from the physical
    /// keys; see `input::ALT_KEYS`.
    alt_keys: u8,
    /// Which explorer layout to return to when the viewer closes.
    viewer_return: ViewMode,
    /// The picture the viewer was last left on. Leaving a picture is a step
    /// back, so Forward opens it again, until the explorer goes elsewhere.
    reopen: Option<PathBuf>,
    /// Whether the viewer shows the sidebar and its header around the picture
    /// (opened from the explorer) or only the picture and the status bar (the
    /// bare viewer a file named on the command line opens into). `B` swaps it.
    viewer_framed: bool,
    /// Whether the inspector is wanted. Shown in the explorer and the framed
    /// viewer when the window has room; never in the bare viewer.
    show_inspector: bool,

    /// The direction the user is navigating (+1 forward, -1 back), so preload leans
    /// ahead of them rather than symmetrically around the current image.
    nav_dir: i32,
    mode: ViewMode,
    viewer: Viewer,
    show_help: bool,
    show_debug: bool,

    /// Latest pointer position in device pixels (the zoom focus, the pan delta),
    /// and whether the pointer is over the window at all: after it leaves, the
    /// last position is history, and lights no button.
    pointer_pos: (f64, f64),
    pointer_in: bool,
    /// Left button held for a pan drag.
    dragging: bool,
    last_click: Option<Instant>,
    /// Scroll gathered within the current pointer frame, committed on `frame`.
    scroll_v: f64,
    scroll_notches: i32,
    scroll_source: u32,
    /// Continuous-scroll carry (a touchpad), consumed in notch-sized steps.
    scroll_accum: f64,
    repeat: KeyRepeat,
    /// The most recent input event's serial. A request the compositor validates
    /// against a real interaction (`set_selection` to own the clipboard) must quote
    /// one, so the latest key/button serial is kept here.
    last_serial: u32,
    /// The selection we own, when the user has copied a path or a picture.
    clipboard: Clipboard,

    /// Last frame's build time, for the debug overlay.
    last_frame_ms: f32,
    init_size: (u32, u32),
    /// Decode-pipeline counters (readiness, waste, memory high-water).
    pipe_stats: PipelineStats,
    /// The focus index last handed to the loader, so it is only re-scored when the
    /// user's centre of attention actually moves. `usize::MAX` until the first frame.
    last_focus: usize,
}

impl State {
    fn new(settings: &Settings) -> Result<Self> {
        let conn = Connection::connect()?;
        // Open at unity scale. The compositor's real scale arrives during bring-up
        // and reopens the fonts at the device size (see `set_scale_120`).
        let font_selection = FontSelection::resolve(&FontConfig::default())?;
        let fonts = Fonts::with_selection(&font_selection, &FONT_SIZES)?;
        let (w, h) = (
            settings.width.clamp(MIN_WIDTH, MAX_DIMENSION),
            settings.height.clamp(MIN_HEIGHT, MAX_DIMENSION),
        );
        let stats = std::env::var_os("BNKFILES_STATS").is_some();
        // One cache directory, read by the scanner and written by the writer.
        let thumb_cache = thumbcache::Cache::from_env();
        Ok(Self {
            conn,
            font_selection,
            fonts,
            xkb: Xkb::new()?,
            poll_set: PollSet::new(),
            next_id: 2,
            free_ids: Vec::new(),
            registry: 0,
            compositor: None,
            wm_base: None,
            seat: None,
            seat_version: 0,
            decoration_manager: None,
            cursor_shape_manager: None,
            data_device_manager: None,
            data_device: 0,
            surface: 0,
            xdg_surface: 0,
            toplevel: 0,
            decoration: 0,
            keyboard: 0,
            pointer: 0,
            cursor_shape_device: 0,
            pointer_enter_serial: 0,
            pointer_shape: 0,
            width: w,
            height: h,
            scaling: Scaling::new((w, h)),
            presentation: GpuPresentation::new(),
            pending_configure: None,
            pending_size: None,
            pending_sync: None,
            frame_callback: 0,
            configured: false,
            closed: false,
            dirty: true,
            stats,
            probe: std::env::var_os("BNKFILES_PROBE").is_some(),
            fullscreen: false,
            images: Vec::new(),
            index: 0,
            shown: None,
            store: ImageStore::new(),
            inflight: HashSet::new(),
            undrawn: HashSet::new(),
            generation: 0,
            loader: Loader::new()?,
            current_dir: None,
            scanner: Scanner::new(thumb_cache.clone())?,
            scan_dir: PathBuf::from("."),
            scan_generation: 0,
            listing: Vec::new(),
            refresh: None,
            rows: Vec::new(),
            rows_rev: 0,
            rows_total: 0,
            filter: Filter::new(),
            order: settings.order,
            reveal_on_adopt: false,
            scan_done: true,
            unreadable: None,
            pending_focus: None,
            saved_thumbs: std::collections::HashMap::new(),
            aspects: std::collections::HashMap::new(),
            aspects_rev: 0,
            masonry: std::cell::RefCell::new(None),
            shapes: settings.shapes.clone(),
            shapes_chosen: Vec::new(),
            shape_here: masonry::Shape::Auto,
            auto_masonry: None,
            thumb_writer: thumb_cache.and_then(|cache| thumbcache::Writer::start(cache, stats)),
            // A machine out of inotify instances is one that browses without live
            // refresh, not one that fails to start.
            watch: Inotify::new()
                .map_err(|e| eprintln!("bnkfiles: no live refresh: {e}"))
                .ok(),
            watch_wd: None,
            watched: None,
            watch_buf: vec![0u8; WATCH_BUF],
            rescan: Debounce::new(WATCH_DEBOUNCE),
            retry_uploads: Debounce::new(UPLOAD_RETRY_DELAY),
            thumbs_unsaved: false,
            save_thumbs_at: Debounce::new(THUMB_SAVE_DELAY),
            notice: None,
            jobs: Jobs::new(WRITES)?,
            prompt: None,
            menu: None,
            deferred_conflict: None,
            sel: Selection::new(),
            list_scroll: 0.0,
            grid_scroll: 0.0,
            explorer_bar: Scrollbar::hidden(),
            focus: Focus::Entries,
            show_hidden: settings.show_hidden,
            places: Vec::new(),
            place_sel: 0,
            mounts_text: String::new(),
            folded: settings.folded.clone(),
            collections: settings.collections.clone(),
            collections_learned: Vec::new(),
            collections_forgotten: Vec::new(),
            usage: Usage::new()?,
            fills: std::collections::HashMap::new(),
            gone_places: HashSet::new(),
            pinned: settings.pinned.clone(),
            path_sel: 0,
            history: History::new(),
            hover: None,
            alt_keys: 0,
            reopen: None,
            viewer_return: if settings.grid_view {
                ViewMode::Grid
            } else {
                ViewMode::List
            },
            viewer_framed: false,
            show_inspector: settings.inspector,
            nav_dir: 1,
            mode: if settings.grid_view {
                ViewMode::Grid
            } else {
                ViewMode::List
            },
            viewer: Viewer::new(),
            show_help: false,
            show_debug: false,
            pointer_pos: (0.0, 0.0),
            pointer_in: false,
            dragging: false,
            last_click: None,
            scroll_v: 0.0,
            scroll_notches: 0,
            scroll_source: u32::MAX,
            scroll_accum: 0.0,
            repeat: KeyRepeat::default(),
            last_serial: 0,
            clipboard: Clipboard::new(),
            last_frame_ms: 0.0,
            init_size: (w, h),
            pipe_stats: PipelineStats::default(),
            last_focus: usize::MAX,
        })
    }

    /// Open what the command line named: a folder in the grid, a file straight
    /// into the viewer at its place among its neighbours (so the arrows walk the
    /// folder from it). With no argument, the last folder is restored, or the
    /// window opens empty.
    ///
    /// Nothing here reads a directory. The scan is asked for and the loop starts;
    /// a named file is shown from its own path immediately and takes its place in
    /// the folder when the scan lands, so `bnkfiles photo.jpg` puts the picture on
    /// screen without waiting to hear what else is in the folder.
    fn open_from_arg(&mut self, opening: Option<Opening>, settings: &Settings) {
        match opening {
            Some(Opening::Dir(dir)) => self.open_dir(dir, PendingFocus::First),
            Some(Opening::File(path)) => {
                let folder = match path.parent() {
                    Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
                    _ => PathBuf::from("."),
                };
                self.current_dir = Some(folder.clone());
                self.open_images(vec![path], 0);
                self.request_scan(folder);
                self.viewer_return = self.mode;
                self.viewer_framed = false;
                self.enter_viewer();
            }
            // A restored `last_dir` may be long gone; the scan reports that in the
            // window rather than the terminal, which is where the user is looking.
            // With nothing saved at all, the home directory beats an empty window.
            None => {
                let dir = settings
                    .last_dir
                    .clone()
                    .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
                    .unwrap_or_else(|| PathBuf::from("."));
                self.open_dir(dir, PendingFocus::Index(settings.last_index));
            }
        }
        self.refresh_places();
    }

    fn roundtrip(&mut self) -> Result<()> {
        let callback = self.alloc_id();
        self.conn.request(
            protocol::WL_DISPLAY,
            wl_display::SYNC,
            &[Arg::NewId(callback)],
        );
        self.pending_sync = Some(callback);
        self.run_until(|s| s.pending_sync.is_none())
    }

    /// The event loop: drain what the compositor and the decode worker sent,
    /// service the timers, present a frame if one is owed and the compositor is
    /// ready for it, then block until any of those has something to say.
    fn run_until(&mut self, done: impl Fn(&State) -> bool) -> Result<()> {
        loop {
            while let Some(msg) = self.conn.next_message()? {
                self.handle(msg)?;
            }
            // Scans before decodes: a batch that completes the gallery asks for
            // decodes, and this way they are queued in the same turn of the loop
            // rather than waiting for the next wake.
            self.handle_scans();
            self.handle_decodes();
            self.handle_watch();
            self.handle_jobs();
            self.handle_usage();
            self.fire_repeat();
            self.tick_scrollbars();
            self.tick_rescan();
            self.tick_thumb_save();
            if self.retry_uploads.fired(Instant::now()) {
                self.dirty = true;
            }
            self.tick_notice();
            // Pace to the compositor: a frame is presented only when no callback
            // is outstanding, so a burst of events (a drag's worth of configures)
            // collapses into one repaint at the settled state. A configure that
            // has outwaited its callback draws anyway (see `may_draw`).
            let turn = Instant::now();
            if self.configured && self.dirty && self.may_draw(turn) && self.render_frame()? {
                self.dirty = false;
            }
            if done(self) {
                return Ok(());
            }
            self.conn.flush()?;

            self.poll_set.clear();
            let wayland = self.poll_set.add(self.conn.fd());
            let decodes = self.poll_set.add(self.loader.wake_fd());
            let scans = self.poll_set.add(self.scanner.wake_fd());
            let job_progress = self.poll_set.add(self.jobs.wake_fd());
            let volumes = self.poll_set.add(self.usage.wake_fd());
            let watch = self.watch.as_ref().map(|w| self.poll_set.add(w.fd()));
            // Under explicit sync a buffer's release is a fence, not an event on
            // the socket, so the frame waiting for one is woken through this.
            let release = self.release_wake_fd().map(|fd| self.poll_set.add(fd));
            let retired: Vec<usize> = self
                .retired_fences()
                .into_iter()
                .map(|fd| self.poll_set.add(fd))
                .collect();
            self.poll_set.wait(self.next_wake(turn))?;
            if release.is_some_and(|slot| self.poll_set.readable(slot)) {
                self.on_release_wake();
            }
            if retired.iter().any(|&slot| self.poll_set.readable(slot)) {
                self.reap_retired();
            }
            if self.poll_set.readable(wayland) {
                // poll says the socket has data (or hung up), so this read returns
                // at once; the timeout is only a safety net.
                if let Fill::Bytes(0) = self.conn.fill(Some(Duration::from_millis(50)))? {
                    return Err(Error::msg("compositor closed the connection"));
                }
            }
            // A ready decode, scan, or job needs no read here: their results come
            // over channels, and the next turn of the loop drains them, as does
            // the watch (whose fd is read by `handle_watch`).
            let _ = (decodes, scans, job_progress, volumes, watch);
        }
    }

    /// How long to block for input: the soonest of the key-repeat deadline and the
    /// scrollbars' fade deadlines, or `None` (indefinitely) when nothing is on a
    /// clock. While a frame callback is outstanding the compositor's own cadence
    /// drives the fades, so they are not also waited for here.
    ///
    /// `turn` is the instant this turn of the loop judged whether to draw.
    fn next_wake(&self, turn: Instant) -> Option<Duration> {
        let now = Instant::now();
        let due = |at: Instant| {
            at.saturating_duration_since(now)
                .max(Duration::from_millis(1))
        };
        let fade = if self.frame_callback == 0 {
            self.explorer_bar.retry_at()
        } else {
            None
        };
        // Every deadline the loop owes something to. A missing one here does not
        // fail loudly: the work simply waits for the next unrelated event.
        let notice = self.notice.as_ref().map(Notice::until);
        [
            self.repeat.next,
            fade,
            self.rescan.deadline(),
            self.save_thumbs_at.deadline(),
            self.retry_uploads.deadline(),
            notice,
            // The moment a pending configure stops waiting for its frame callback,
            // but only while that was still ahead when the turn judged the draw.
            // One that had passed by then was drawn, or is waiting on a buffer
            // release, and waiting on it again would spin.
            self.configure_overdue_at().filter(|&at| at > turn),
        ]
        .into_iter()
        .flatten()
        .map(due)
        .min()
    }

    /// Advance the scrollbar's fade/expand animation, and mark a repaint if it
    /// changed appearance.
    fn tick_scrollbars(&mut self) {
        if self.explorer_bar.tick(Instant::now()) {
            self.dirty = true;
        }
    }

    // ---- the notification line -------------------------------------------

    /// Put `text` on the notification line, for as long as `level` gives it.
    /// Everything the app says in passing comes through here.
    fn notify(&mut self, level: Level, text: impl Into<String>) {
        self.notice = Some(Notice::new(level, text.into(), Instant::now()));
        self.dirty = true;
    }

    /// Take the notice down before its time. Says whether one was up.
    fn dismiss_notice(&mut self) -> bool {
        let up = self.notice_up().is_some();
        self.notice = None;
        self.dirty |= up;
        up
    }

    /// Take the notice down once it has lingered. Its deadline is one of
    /// `next_wake`'s, and a deadline left in the past clamps to a 1 ms wait, so a
    /// notice that was never cleared would wake the loop a thousand times a second
    /// and stay drawn until something else repainted.
    fn tick_notice(&mut self) {
        if self
            .notice
            .as_ref()
            .is_some_and(|n| !n.is_up(Instant::now()))
        {
            self.notice = None;
            self.dirty = true;
        }
    }

    /// The notice, if one is up and still current.
    pub(super) fn notice_up(&self) -> Option<&Notice> {
        self.notice.as_ref().filter(|n| n.is_up(Instant::now()))
    }

    // ---- geometry --------------------------------------------------------

    /// The display scale, as the layout and the hit tests take it.
    fn scale(&self) -> Scale {
        Scale::from_120(self.scaling.factor_120)
    }

    /// A logical length in device pixels, at the current scale.
    fn to_device(&self, logical: u32) -> u32 {
        logical_to_device(logical, self.scaling.factor_120)
    }

    /// The largest window side, in device pixels, a buffer can be made for.
    fn max_dimension(&self) -> u32 {
        self.presentation
            .backend
            .as_ref()
            .map_or(MAX_DIMENSION, |gpu| {
                gpu.max_image_dimension().clamp(1, MAX_DIMENSION)
            })
    }

    fn device_size(&self, logical_w: u32, logical_h: u32) -> (u32, u32) {
        let max = self.max_dimension();
        (
            self.to_device(logical_w).clamp(1, max),
            self.to_device(logical_h).clamp(1, max),
        )
    }

    /// The font size, in device pixels, that a logical size was opened at.
    fn font_px(&self, logical: u32) -> u32 {
        self.to_device(logical).max(1)
    }

    /// The height of the status bar, in device pixels.
    fn status_h(&self) -> i32 {
        let metrics = self.fonts.metrics(self.font_px(FONT_STATUS));
        metrics.line_height + self.scale().px(12)
    }

    /// The regions the state on screen asks for. The explorer and the framed
    /// viewer have the sidebar and a header; the bare viewer has neither, and
    /// owns the window above the status bar.
    fn panels(&self) -> Panels {
        let framed = self.mode.is_explorer() || self.viewer_framed;
        Panels {
            sidebar: framed,
            header: framed,
            inspector: framed && self.show_inspector,
        }
    }

    /// Every region of the window at the current size and scale: the one place
    /// the painters and the hit tests take their rectangles from.
    fn regions(&self) -> Regions {
        Regions::new(
            self.scale(),
            self.width as i32,
            self.height as i32,
            self.status_h(),
            self.panels(),
        )
    }

    /// The area the *viewer* draws in: the content region, which is the window
    /// less the status bar in the bare viewer. `None` until the window is
    /// configured.
    ///
    /// The zoom geometry is worked out in the canvas's own coordinates, with its
    /// top-left at the origin, and moved to `canvas.x, canvas.y` only when drawn.
    /// Laying out against the whole window would put the bottom of a picture
    /// behind the status bar.
    fn canvas(&self) -> Option<Rect> {
        self.configured.then(|| self.regions().content)
    }

    /// Where the explorer's entries go. `None` until the window is configured,
    /// and in the viewer, which has no entries to place.
    fn content_rect(&self) -> Option<Rect> {
        (self.configured && self.mode.is_explorer()).then(|| self.regions().content)
    }

    /// The header's buttons and its middle, for the view on screen.
    fn header_layout(&self) -> HeaderLayout {
        let kind = match self.mode {
            ViewMode::Viewer => HeaderKind::Viewer,
            ViewMode::List | ViewMode::Grid => HeaderKind::Explorer,
        };
        HeaderLayout::new(self.scale(), self.regions().header, kind)
    }

    /// The explorer's rows and columns at the current size, scale, and scroll.
    fn explorer_layout(&self) -> ExplorerLayout {
        ExplorerLayout::new(
            self.scale(),
            &self.regions(),
            self.fonts.metrics(self.font_px(FONT_BROWSE)).line_height,
            self.rows.len(),
            self.list_scroll,
            self.mode == ViewMode::List,
        )
    }

    /// The grid's box model, sized to the font its cell captions are drawn in so a
    /// row leaves room for its own labels rather than for the row below's.
    fn grid_metrics(&self) -> grid::Metrics {
        let label = self.fonts.metrics(self.font_px(FONT_LABEL)).line_height;
        grid::Metrics::new(self.scale(), label)
    }

    /// The thumbnail grid, laid out inside the explorer's content region.
    fn grid_layout(&self) -> Layout {
        self.grid_layout_at(self.grid_scroll)
    }

    /// The thumbnail grid scrolled to `scroll`, clamped to its content.
    fn grid_layout_at(&self, scroll: f32) -> Layout {
        let content = self.content_rect().unwrap_or(Rect {
            x: 0,
            y: 0,
            w: self.width as i32,
            h: self.height as i32,
        });
        let metrics = self.grid_metrics();
        if self.masonry_now() {
            let columns = self.masonry_columns(content.w);
            return Layout::masonry(metrics, columns, content.h, scroll);
        }
        Layout::new(metrics, content.w, content.h, self.rows.len(), scroll)
    }

    /// The canvas's and the shown image's dimensions, when both exist.
    fn view_dims(&self) -> Option<(u32, u32, u32, u32)> {
        let canvas = self.canvas()?;
        let img = self.shown_image()?;
        Some((canvas.w as u32, canvas.h as u32, img.width, img.height))
    }

    fn shown_image(&self) -> Option<&Image> {
        let key = self.full_key(self.shown?)?;
        self.store.get(&key)
    }

    /// What the viewer draws, and under which store key.
    ///
    /// The image the user navigated to, at full resolution, whenever it is decoded.
    /// Until it is, its thumbnail stands in: the grid has usually already decoded
    /// one, and a blurry version of the right picture beats a sharp version of the
    /// one they just navigated away from — which is what showing the previous image
    /// for the length of a decode amounts to. Falling back to the last shown image
    /// covers the case where neither exists (opening straight into the viewer).
    ///
    /// The swap does not move anything on screen: a thumbnail keeps the source's
    /// aspect ratio, so the fit geometry it produces is the one the full image gets.
    fn viewer_source(&self) -> Option<(ImageKey, &Image)> {
        let key = pick_viewer_key(
            &self.store,
            self.full_key(self.index),
            self.images.get(self.index).map(|p| self.thumb_key_of(p)),
            self.shown.and_then(|s| self.full_key(s)),
        )?;
        let img = self.store.get(&key)?;
        Some((key, img))
    }

    // ---- window ----------------------------------------------------------

    /// Write the window size and viewing location into `settings`, to persist on
    /// exit. The pins are left as the file has them: they are saved when they
    /// change, and another window may have changed them since.
    fn record_session(&self, settings: &mut Settings) {
        // Logical pixels: the size to ask for next time, whatever scale that
        // display turns out to have.
        (settings.width, settings.height) = if self.configured {
            self.scaling.logical
        } else {
            self.init_size
        };
        // The directory the explorer is showing, not the gallery's: reopening
        // where you were browsing is what "last folder" means, and the two are
        // the same except while a scan of somewhere else is in flight.
        settings.last_dir = Some(self.scan_dir.clone());
        // The filter is not saved, so neither is a position among its matches:
        // the index is the picture's place in the whole folder's gallery.
        settings.last_index = if self.filter.is_active() {
            let show_hidden = self.show_hidden;
            let whole = files::images_of(&self.listing, |e| show_hidden || !e.hidden);
            self.images
                .get(self.index)
                .and_then(|p| whole.iter().position(|w| w == p))
                .unwrap_or(0)
        } else {
            self.index
        };
        settings.grid_view = self.viewer_return == ViewMode::Grid;
        settings.show_hidden = self.show_hidden;
        settings.inspector = self.show_inspector;
        settings.folded = self.folded.clone();
        settings.order = self.order;
        settings
            .collections
            .retain(|c| !self.collections_forgotten.contains(c));
        for learned in &self.collections_learned {
            if !settings.collections.contains(learned) {
                settings.collections.push(learned.clone());
            }
        }
        // This window's choices, latest last; another window's for other
        // folders stay. Auto is kept too: it can undo an ancestor's choice.
        for (dir, shape) in &self.shapes_chosen {
            settings.shapes.retain(|(d, _)| d != dir);
            settings.shapes.push((dir.clone(), *shape));
        }
        let over = settings.shapes.len().saturating_sub(settings::MAX_SHAPES);
        settings.shapes.drain(..over);
    }

    /// Print the decode-pipeline counters (`BNKFILES_STATS`): a session's readiness,
    /// wasted work, and memory high-water: the numbers loader parallelism, request
    /// priority and cancellation move.
    fn report_pipeline_stats(&self) {
        let s = &self.pipe_stats;
        let shows = s.show_hits + s.show_misses;
        let hit_pct = (s.show_hits * 100).checked_div(shows).unwrap_or(0);
        let peak_mb = s.peak_resident_bytes as f64 / (1024.0 * 1024.0);
        eprintln!("bnkfiles pipeline stats:");
        eprintln!(
            "  display ready:  {}/{shows} ({hit_pct}%) resident when shown",
            s.show_hits
        );
        eprintln!(
            "  decoded:        {} full, {} thumb",
            s.full_decoded, s.thumb_decoded
        );
        eprintln!(
            "  wasted:         {} skipped before decode, {} decoded then stale, {} decoded then unseen",
            s.queued_skipped, s.active_stale_completed, s.decoded_then_unused
        );
        eprintln!("  peak resident:  {peak_mb:.1} MB");
        eprintln!("  peak in-flight: {}", s.peak_inflight);
        eprintln!(
            "  scans:          {} completed, {} entries, largest {}, {} from the watch",
            s.scans_done, s.scan_entries, s.scan_largest, s.rescans
        );
        eprintln!(
            "  thumb cache:    {} read, {} reused instead of decoded",
            s.thumbs_from_cache, s.thumbs_reused
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::app::{pick_viewer_key, read_head};
    use crate::render::image::{Image, ImageKey, ImageStore};

    fn store_with(keys: &[&str]) -> ImageStore {
        let mut store = ImageStore::new();
        for k in keys {
            store.insert(
                ImageKey::full(*k),
                Image {
                    pixels: vec![0; 4],
                    width: 2,
                    height: 2,
                },
            );
        }
        store
    }

    fn key(s: &str) -> Option<ImageKey> {
        Some(ImageKey::full(s))
    }

    #[test]
    fn the_viewer_prefers_the_full_image_then_its_thumbnail_then_the_last_shown() {
        // Everything resident: the target's full decode wins.
        let store = store_with(&["full", "thumb", "previous"]);
        assert_eq!(
            pick_viewer_key(&store, key("full"), key("thumb"), key("previous")),
            key("full")
        );

        // The target has not decoded yet: its thumbnail stands in, rather than
        // leaving the picture the user navigated away from on screen.
        let store = store_with(&["thumb", "previous"]);
        assert_eq!(
            pick_viewer_key(&store, key("full"), key("thumb"), key("previous")),
            key("thumb")
        );

        // Neither: keep the last image that was successfully shown.
        let store = store_with(&["previous"]);
        assert_eq!(
            pick_viewer_key(&store, key("full"), key("thumb"), key("previous")),
            key("previous")
        );

        // Nothing resident at all draws nothing, rather than a stale key.
        let store = store_with(&[]);
        assert_eq!(
            pick_viewer_key(&store, key("full"), key("thumb"), key("previous")),
            None
        );
    }

    #[test]
    fn a_missing_candidate_is_skipped_not_treated_as_a_miss() {
        // Opening straight into the viewer: there is no thumbnail key at all, and no
        // previous image. The full decode is still chosen once it lands.
        let store = store_with(&["full"]);
        assert_eq!(
            pick_viewer_key(&store, key("full"), None, None),
            key("full")
        );
        // And before it lands, with no fallbacks, nothing is drawn.
        let store = store_with(&[]);
        assert_eq!(pick_viewer_key(&store, key("full"), None, None), None);
    }

    /// Enter on a FIFO reads its head on the main thread. Opening one for reading
    /// waits for a writer that may never come, which would freeze the window for
    /// good.
    #[test]
    fn the_head_of_a_fifo_is_empty_rather_than_waited_for() {
        extern "C" {
            fn mkfifo(path: *const core::ffi::c_char, mode: u32) -> core::ffi::c_int;
        }
        let dir = std::env::temp_dir().join(format!("bnkfiles-head-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let fifo = dir.join("pipe.txt");
        let c_path = std::ffi::CString::new(fifo.to_str().expect("utf-8 temp path")).expect("nul");
        // SAFETY: c_path is a NUL-terminated path that outlives the call.
        assert_eq!(unsafe { mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let regular = dir.join("note.txt");
        std::fs::write(&regular, b"hello").expect("write");

        let (from_fifo, from_file) = (read_head(&fifo), read_head(&regular));
        std::fs::remove_dir_all(&dir).expect("clean up");
        assert!(from_fifo.is_empty());
        assert_eq!(from_file, b"hello");
    }
}
