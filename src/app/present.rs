//! The GPU/dmabuf presentation half of the app: negotiating the buffers, sync,
//! and per-frame submit that turn a [`DisplayList`] into pixels on the
//! compositor's surface.
//!
//! ```text
//!   DisplayList ─gpu::build_frame─▶ FrameData ─Gpu::render_list─▶ dmabuf ─attach─▶ compositor
//!                                              (+ explicit sync when offered)
//! ```
//!
//! Two exported swap buffers double-buffer the surface so the compositor never
//! reads a frame we are still painting. Explicit sync (`linux-drm-syncobj-v1`)
//! orders the two sides when the compositor offers it; otherwise the implicit
//! dmabuf-fence bridge in the Vulkan backend does.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::app::State;
use crate::platform::conn::Connection;
use crate::platform::dmabuf;
use crate::platform::error::{Error, Result};
use crate::platform::ffi;
use crate::platform::geom::Rect;
use crate::platform::protocol::{
    self, wl_buffer, wl_display, wl_surface, wp_linux_drm_syncobj_manager_v1,
    wp_linux_drm_syncobj_surface_v1, wp_viewport, xdg_surface, zwp_linux_buffer_params_v1,
    zwp_linux_dmabuf_feedback_v1, zwp_linux_dmabuf_v1,
};
use crate::platform::wire::{Arg, Reader};
use crate::render::display::{self, DisplayList, DrawCmd};
use crate::render::gpu;
use crate::render::vulkan;
use crate::theme::{BACKGROUND, GLYPH_GAMMA};

/// How many swap buffers the surface cycles through.
const SWAP_BUFFERS: usize = 2;

/// How long past the latest frame a pending configure waits for the frame
/// callback before a frame is drawn without it. A hidden window gets no
/// callbacks at all, so without this a resize or state change that arrives
/// while it is minimized or covered would go unanswered until it is shown
/// again. A few refreshes at any common rate, and short of the 100 ms some
/// compositors wait for an ack before carrying on without one.
const FRAME_CALLBACK_OVERDUE: Duration = Duration::from_millis(50);

/// The retired-image list is fed at most two images per resize and drained as
/// their releases arrive, so it stays tiny. This cap is the backstop against a
/// compositor that never releases a buffer at all: past it, the oldest is freed
/// regardless, since bounded memory beats an unbounded leak. Eight resizes'
/// worth.
const MAX_RETIRED_IMAGES: usize = 16;

/// A swap image a resize replaced while the compositor may still be reading its
/// wl_buffer. Draining our own queue says nothing about the compositor, so the
/// image and its wl_buffer wait here until the buffer's release is observed.
struct RetiredImage {
    image: vulkan::RenderTarget,
    /// The wl_buffer this image backed, kept alive until the image goes.
    buffer: u32,
    release: RetiredRelease,
}

/// How a retired buffer's release will be known.
enum RetiredRelease {
    /// Implicit sync: its `wl_buffer.release` event.
    BufferEvent,
    /// Explicit sync: release `point` on buffer slot `slot`'s timeline, which
    /// has no fence yet.
    Point { slot: usize, point: u64 },
    /// Explicit sync: the fence at its release point, signalled once the
    /// compositor is done.
    Fence(OwnedFd),
}

/// Client-owned explicit-sync state (`linux-drm-syncobj-v1`), negotiated over a
/// live GPU backend when the compositor advertises the manager. DRM syncobj
/// timelines the compositor shares: we signal `acquire` with each frame's
/// render-done fence, and it signals a buffer slot's `release` timeline when
/// done reading that buffer. A scratch binary syncobj carries the sync-file
/// transfers, and a binary syncobj created signalled is the acquire fence of a
/// frame with no render-done fence. `point` increments per present and numbers
/// both of a frame's points. Dropping this closes the render-node fd, which
/// frees every syncobj created on it.
///
/// A buffer is free for the next frame once its release point has a fence, and
/// `wl_buffer.release` plays no part: the protocol leaves its delivery
/// undefined under explicit sync. While a buffer is held, `wake` is registered
/// on its release point and the event loop polls it, so the release itself ends
/// the wait.
pub(super) struct ExplicitSync {
    drm: OwnedFd,
    acquire_syncobj: u32,
    scratch: u32,
    signalled: u32,
    acquire_timeline: u32,
    release: [ReleaseTimeline; SWAP_BUFFERS],
    surface: u32,
    point: u64,
    wake: OwnedFd,
}

/// Where a buffer slot stands for the next frame rendered into it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotRelease {
    /// No frame has gone into the slot's buffer: render into it at once.
    Unused,
    /// The compositor still holds the buffer: its release point has no fence yet.
    Held,
    /// The compositor released the buffer: a fence sits at its release point,
    /// which the next render into it waits on. It may not have signalled yet.
    Released,
}

/// One buffer slot's release timeline. Each slot has its own because signalling
/// a timeline point signals every earlier point too, and a compositor may
/// release buffers in a different order than they were committed: one buffer's
/// release must never signal a point another buffer is waiting on.
#[derive(Default)]
struct ReleaseTimeline {
    syncobj: u32,
    /// The protocol object the compositor imported `syncobj` as.
    timeline: u32,
    /// The point last published for the slot's buffer, which its next reuse
    /// waits on. `None` before the slot's first frame and after a resize
    /// replaced the buffer.
    point: Option<u64>,
    /// The point the release wake was last registered on. A registration fires
    /// as soon as its point has a fence and the kernel keeps it until then, so
    /// while this is the held point no second one is made.
    waking: Option<u64>,
}

/// The two timeline points one commit carries, as the requests name them.
struct FramePoints {
    acquire_timeline: u32,
    release_timeline: u32,
    point: u64,
}

/// `list` without the images in `undrawn`: what a frame put on screen when
/// those had no texture to draw from.
fn without_images(list: &[DrawCmd], undrawn: &[gpu::TextureKey]) -> DisplayList {
    list.iter()
        .filter(|cmd| match cmd {
            DrawCmd::Image {
                key, generation, ..
            } => !undrawn
                .iter()
                .any(|texture| texture.image == *key && texture.generation == *generation),
            _ => true,
        })
        .cloned()
        .collect()
}

/// The first slot whose buffer the compositor is not holding.
fn first_free(slots: impl IntoIterator<Item = SlotRelease>) -> Option<usize> {
    slots.into_iter().position(|slot| slot != SlotRelease::Held)
}

impl ExplicitSync {
    /// Create the client's syncobjs on `drm`: the acquire timeline, a release
    /// timeline per buffer slot, and the scratch and signalled binaries. Their
    /// protocol objects are zero until [`State::negotiate_explicit_sync`] shares
    /// them with the compositor.
    ///
    /// `None` where the kernel or driver has no syncobj eventfds (Linux 6.6),
    /// which the release wake needs. The caller then keeps the implicit bridge.
    fn new(drm: OwnedFd) -> Result<Option<Self>> {
        let fd = drm.as_raw_fd();
        let mut release = [ReleaseTimeline::default(), ReleaseTimeline::default()];
        for slot in &mut release {
            slot.syncobj = ffi::drm_syncobj_create(fd, false)?;
        }
        let signalled = ffi::drm_syncobj_create(fd, true)?;
        let wake = ffi::eventfd()?;
        if !ffi::drm_syncobj_eventfd_supported(fd, signalled, wake.as_raw_fd())? {
            return Ok(None);
        }
        // The probe fired the eventfd at once; start with it clear.
        ffi::event_drain(wake.as_raw_fd());
        Ok(Some(Self {
            acquire_syncobj: ffi::drm_syncobj_create(fd, false)?,
            scratch: ffi::drm_syncobj_create(fd, false)?,
            signalled,
            drm,
            acquire_timeline: 0,
            release,
            surface: 0,
            point: 0,
            wake,
        }))
    }

    fn slot(&self, idx: usize) -> Result<&ReleaseTimeline> {
        self.release
            .get(idx)
            .ok_or_else(|| Error::msg(format!("no buffer slot {idx}")))
    }

    /// Where buffer slot `idx` stands: unused, held by the compositor, or released.
    fn slot_release(&self, idx: usize) -> Result<SlotRelease> {
        let slot = self.slot(idx)?;
        let Some(point) = slot.point else {
            return Ok(SlotRelease::Unused);
        };
        if ffi::drm_syncobj_point_available(self.drm.as_raw_fd(), slot.syncobj, point)? {
            Ok(SlotRelease::Released)
        } else {
            Ok(SlotRelease::Held)
        }
    }

    /// The fence at release `point` of slot `idx`'s timeline. Only for a point
    /// that has one: a held point has no fence to export, and asking is an error.
    fn point_fence(&self, idx: usize, point: u64) -> Result<OwnedFd> {
        ffi::drm_syncobj_point_to_sync_file(
            self.drm.as_raw_fd(),
            self.slot(idx)?.syncobj,
            point,
            self.scratch,
        )
    }

    /// The fence a render into buffer `idx` waits on: the compositor's release
    /// of the slot's last frame, or `None` before its first. Only for a slot
    /// that is not [`SlotRelease::Held`].
    fn release_fence(&self, idx: usize) -> Result<Option<OwnedFd>> {
        match self.slot(idx)?.point {
            Some(point) => Ok(Some(self.point_fence(idx, point)?)),
            None => Ok(None),
        }
    }

    /// The slot the next frame goes into, or `None` while the compositor holds
    /// every buffer. Then [`Self::wake_fd`] becomes readable once one is
    /// released: each held point carries one registration, made by the first
    /// wait that finds it held.
    fn free_slot(&mut self) -> Result<Option<usize>> {
        let mut states = [SlotRelease::Unused; SWAP_BUFFERS];
        for (idx, state) in states.iter_mut().enumerate() {
            *state = self.slot_release(idx)?;
        }
        if let Some(idx) = first_free(states) {
            return Ok(Some(idx));
        }
        for idx in 0..SWAP_BUFFERS {
            self.wake_on_release(idx)?;
        }
        Ok(None)
    }

    /// Have the release of slot `idx`'s current buffer make [`Self::wake_fd`]
    /// readable, unless a registration on that point is already pending.
    fn wake_on_release(&mut self, idx: usize) -> Result<()> {
        let drm = self.drm.as_raw_fd();
        let wake = self.wake.as_raw_fd();
        let slot = self
            .release
            .get_mut(idx)
            .ok_or_else(|| Error::msg(format!("no buffer slot {idx}")))?;
        if let Some(point) = slot.point.filter(|&point| slot.waking != Some(point)) {
            ffi::drm_syncobj_eventfd_when_available(drm, slot.syncobj, point, wake)?;
            slot.waking = Some(point);
        }
        Ok(())
    }

    /// The eventfd the event loop polls for a held buffer's release.
    fn wake_fd(&self) -> RawFd {
        self.wake.as_raw_fd()
    }

    /// The wake fired: clear it, so the next registration to fire wakes the
    /// loop again.
    fn woke(&mut self) {
        ffi::event_drain(self.wake.as_raw_fd());
    }

    /// Claim this frame's points: put the render-done `fence` at a fresh acquire
    /// point, and make the same point number buffer `idx`'s release point on
    /// that buffer's own timeline (its next reuse waits on that via
    /// [`Self::release_fence`]).
    ///
    /// `None` is a frame the CPU already waited out: its acquire point is
    /// published already signalled. Both points exist either way, because a
    /// commit that attaches a buffer without them is the `no_acquire_point`
    /// protocol error, which closes the connection.
    fn claim_points(&mut self, fence: Option<&OwnedFd>, idx: usize) -> Result<FramePoints> {
        let slot = self
            .release
            .get_mut(idx)
            .ok_or_else(|| Error::msg(format!("no buffer slot {idx}")))?;
        let point = self.point + 1;
        let drm = self.drm.as_raw_fd();
        match fence {
            Some(fence) => ffi::drm_syncobj_sync_file_to_point(
                drm,
                fence.as_raw_fd(),
                self.acquire_syncobj,
                point,
                self.scratch,
            )?,
            None => {
                ffi::drm_syncobj_signal_point(drm, self.acquire_syncobj, point, self.signalled)?
            }
        }
        self.point = point;
        slot.point = Some(point);
        Ok(FramePoints {
            acquire_timeline: self.acquire_timeline,
            release_timeline: slot.timeline,
            point,
        })
    }

    /// Claim this frame's points (see [`Self::claim_points`]) and set them on
    /// the surface, so the compositor waits for the GPU before reading buffer
    /// `idx` and signals the release point when it is done.
    fn publish(
        &mut self,
        conn: &mut Connection,
        fence: Option<&OwnedFd>,
        idx: usize,
    ) -> Result<()> {
        let points = self.claim_points(fence, idx)?;
        let (hi, lo) = ((points.point >> 32) as u32, points.point as u32);
        conn.request(
            self.surface,
            wp_linux_drm_syncobj_surface_v1::SET_ACQUIRE_POINT,
            &[
                Arg::Object(points.acquire_timeline),
                Arg::Uint(hi),
                Arg::Uint(lo),
            ],
        );
        conn.request(
            self.surface,
            wp_linux_drm_syncobj_surface_v1::SET_RELEASE_POINT,
            &[
                Arg::Object(points.release_timeline),
                Arg::Uint(hi),
                Arg::Uint(lo),
            ],
        );
        Ok(())
    }

    /// How the release of the buffer a resize is taking out of slot `idx` will
    /// be known, or `None` when the compositor never had it or is already done
    /// with it, so it can be freed at once. A buffer still held gets the release
    /// wake registered on its point, so the loop wakes to free it.
    fn retire(&mut self, idx: usize) -> Option<RetiredRelease> {
        let point = self.release.get(idx)?.point?;
        // A point that cannot be queried is treated as held: freeing late is
        // safe, freeing early is not.
        if self.slot_release(idx).ok() == Some(SlotRelease::Released) {
            if let Ok(fence) = self.point_fence(idx, point) {
                if ffi::wait_sync_file(fence.as_raw_fd(), 0) {
                    return None;
                }
                return Some(RetiredRelease::Fence(fence));
            }
        }
        // Without the wake the image is still reaped by the next frame drawn.
        let _ = self.wake_on_release(idx);
        Some(RetiredRelease::Point { slot: idx, point })
    }

    /// Whether the compositor is done with a retired buffer, advancing a bare
    /// point to its fence once it has one.
    ///
    /// A retired point shares its slot's timeline with the buffer that replaced
    /// it, and a fence at a later point answers for the earlier one too. So if
    /// a compositor releases the replacement first, the retired image is freed
    /// while the compositor still reads it. That is harmless: nothing of ours
    /// renders into it again, the compositor holds its own reference to the
    /// memory, and destroying a wl_buffer in use is legal. It takes a
    /// replacement superseded before it was ever shown, which only a configure
    /// drawn without waiting for its frame callback can produce.
    fn retired_released(&self, release: &mut RetiredRelease) -> bool {
        if let RetiredRelease::Point { slot, point } = *release {
            let available = self.slot(slot).is_ok_and(|timeline| {
                ffi::drm_syncobj_point_available(self.drm.as_raw_fd(), timeline.syncobj, point)
                    .unwrap_or(false)
            });
            if !available {
                return false;
            }
            match self.point_fence(slot, point) {
                Ok(fence) => *release = RetiredRelease::Fence(fence),
                Err(_) => return false,
            }
        }
        match release {
            RetiredRelease::Fence(fence) => ffi::wait_sync_file(fence.as_raw_fd(), 0),
            RetiredRelease::Point { .. } | RetiredRelease::BufferEvent => false,
        }
    }

    /// Forget every buffer's published release point (the buffers they named are
    /// gone, e.g. after a resize), so the next use of each slot waits on nothing.
    /// A registration still pending on a forgotten point stays: the slot's next
    /// buffer publishes later points on the same timeline, and their release
    /// fires it too.
    fn forget_buffers(&mut self) {
        for slot in &mut self.release {
            slot.point = None;
        }
    }
}

/// GPU/dmabuf presentation resources owned by the app: protocol globals and
/// feedback, the Vulkan backend, the two exported swap buffers, explicit-sync
/// state, and the small counters used by the stats line.
pub(super) struct GpuPresentation {
    pub(super) dmabuf: Option<u32>,
    pub(super) dmabuf_version: u32,
    pub(super) feedback_id: u32,
    pub(super) feedback: dmabuf::FeedbackState,
    pub(super) backend: Option<vulkan::Gpu>,
    pub(super) images: Vec<vulkan::GpuImage>,
    pub(super) modifiers: Vec<u64>,
    pub(super) glyphs: gpu::GlyphCache,
    pub(super) syncobj_manager: Option<u32>,
    pub(super) explicit_sync: Option<ExplicitSync>,
    pub(super) buffers: [u32; SWAP_BUFFERS],
    /// Whether the compositor holds each buffer, as `wl_buffer.release` reports
    /// it. It decides on the implicit path only; under explicit sync the release
    /// points do, and a release event can be a stale one for an earlier commit
    /// of a buffer that has since been committed again.
    pub(super) busy: [bool; SWAP_BUFFERS],
    /// Images a resize replaced that the compositor may still be reading,
    /// awaiting their release before they are freed (see [`RetiredImage`]).
    retired: Vec<RetiredImage>,
    pub(super) buffer_size: (u32, u32),
    pub(super) present_list: Rc<DisplayList>,
    /// Whether `present_list` is on screen. False for fresh buffers, which hold
    /// no frame at all, so the first frame into them is damaged whole.
    presented: bool,
    /// When the latest frame was committed, while its frame callback is still
    /// awaited (see [`State::may_draw`]).
    frame_committed: Option<Instant>,
    pub(super) frame_count: u64,
    pub(super) last_present: Option<Instant>,
    pub(super) explicit_fence_frames: u64,
    pub(super) explicit_cpu_wait_frames: u64,
}

impl GpuPresentation {
    /// How many replaced buffers are still waiting for the compositor to let
    /// go of them.
    pub(super) fn retired_count(&self) -> usize {
        self.retired.len()
    }

    pub(super) fn new() -> Self {
        Self {
            dmabuf: None,
            dmabuf_version: 0,
            feedback_id: 0,
            feedback: dmabuf::FeedbackState::default(),
            backend: None,
            images: Vec::new(),
            modifiers: Vec::new(),
            glyphs: gpu::GlyphCache::new(),
            syncobj_manager: None,
            explicit_sync: None,
            buffers: [0; SWAP_BUFFERS],
            busy: [false; SWAP_BUFFERS],
            retired: Vec::new(),
            buffer_size: (0, 0),
            present_list: Rc::default(),
            presented: false,
            frame_committed: None,
            frame_count: 0,
            last_present: None,
            explicit_fence_frames: 0,
            explicit_cpu_wait_frames: 0,
        }
    }
}

impl Drop for GpuPresentation {
    /// The swap images, live and retired, are the device's children and must be
    /// destroyed before it. `backend` is an earlier field, so without this the
    /// device would go first, with their views, framebuffers, fences and
    /// semaphores still alive.
    fn drop(&mut self) {
        let Some(gpu) = &self.backend else {
            return;
        };
        // Nothing may be in flight while its command buffer and targets go.
        gpu.wait_idle();
        for image in self.images.drain(..) {
            gpu.destroy_image(image);
        }
        for retired in self.retired.drain(..) {
            gpu.destroy_target(retired.image);
        }
    }
}

/// Whether the loop may draw a frame at `now`: whenever no frame callback is in
/// flight, and otherwise only once `overdue_at`, the moment a pending configure
/// stops waiting for that callback, has come.
fn may_draw_at(callback_in_flight: bool, overdue_at: Option<Instant>, now: Instant) -> bool {
    !callback_in_flight || overdue_at.is_some_and(|at| at <= now)
}

impl State {
    /// Whether the loop may draw now. Not while a frame callback is in flight,
    /// so a burst coalesces into one frame per refresh, unless a configure is
    /// waiting and the callback is overdue: a hidden window gets no callbacks,
    /// and the configure would otherwise stay unacked until the window is shown
    /// again. Its ack still rides a frame drawn at its size.
    pub(super) fn may_draw(&self, now: Instant) -> bool {
        may_draw_at(self.frame_callback != 0, self.configure_overdue_at(), now)
    }

    /// When a pending configure stops waiting for the frame callback in flight.
    pub(super) fn configure_overdue_at(&self) -> Option<Instant> {
        self.pending_configure?;
        if self.frame_callback == 0 {
            return None;
        }
        self.presentation
            .frame_committed
            .map(|at| at + FRAME_CALLBACK_OVERDUE)
    }

    /// Ask for a frame callback with the frame about to be committed. One still
    /// in flight means a configure is drawing past it. It stays, since it
    /// already asks for the next frame, and the next configure waits for it
    /// another [`FRAME_CALLBACK_OVERDUE`] from this frame, so a compositor that
    /// has stopped sending callbacks gets a frame per period rather than one per
    /// configure.
    fn arm_frame_callback(&mut self, now: Instant) {
        if self.frame_callback == 0 {
            let callback = self.alloc_id();
            self.conn
                .request(self.surface, wl_surface::FRAME, &[Arg::NewId(callback)]);
            self.frame_callback = callback;
        }
        self.presentation.frame_committed = Some(now);
    }

    /// Bring the Vulkan backend up from the committed dmabuf feedback: a
    /// device matching the compositor's, and a modifier list their common
    /// ground. Called before `create_buffers`, which allocates the exportable
    /// GPU images. A failure here is fatal (the app requires a GPU).
    pub(super) fn init_gpu(&mut self) -> Result<()> {
        if self.presentation.dmabuf.is_none() {
            return Err(Error::msg("compositor lacks zwp_linux_dmabuf_v1 v4+"));
        }
        let fb = self
            .presentation
            .feedback
            .ready()
            .ok_or_else(|| Error::msg("dmabuf feedback never committed"))?;
        let main_device = fb.main_device;
        let compositor_mods = fb.modifiers_for(dmabuf::DRM_FORMAT_XRGB8888);
        if compositor_mods.is_empty() {
            return Err(Error::msg("compositor does not accept XRGB8888 dmabufs"));
        }
        let gpu = vulkan::Gpu::new(main_device)?;
        let mods = gpu.image_modifiers(&compositor_mods);
        if mods.is_empty() {
            return Err(Error::msg(
                "no common DRM modifier between compositor and device",
            ));
        }
        self.presentation.modifiers = mods;
        self.presentation.backend = Some(gpu);
        // The size so far was bounded without knowing the device.
        let max = self.max_dimension();
        self.width = self.width.min(max);
        self.height = self.height.min(max);
        Ok(())
    }

    /// Negotiate explicit sync (`linux-drm-syncobj-v1`) when a GPU backend is
    /// live and the compositor advertises the manager. Best-effort: on any
    /// failure the GPU path keeps the implicit-sync bridge, which is why this
    /// returns nothing and only logs under stats.
    pub(super) fn init_explicit_sync(&mut self) {
        let Some(manager) = self.presentation.syncobj_manager else {
            return;
        };
        let Some(minor) = self
            .presentation
            .backend
            .as_ref()
            .and_then(vulkan::Gpu::render_node)
        else {
            return; // No GPU backend, or the device exposes no render node.
        };
        match self.negotiate_explicit_sync(manager, minor) {
            Ok(es) => self.presentation.explicit_sync = Some(es),
            Err(e) => {
                if self.stats {
                    eprintln!("bnkfiles: explicit sync unavailable, using the sync bridge: {e}");
                }
            }
        }
    }

    /// Create the client-owned syncobjs, share the timelines with the
    /// compositor, and attach explicit sync to the surface.
    ///
    /// Everything that can fail on our side (opening the node, creating and
    /// exporting the syncobjs) happens before the first request is queued. Once
    /// the surface has a syncobj object every commit must carry both points, so
    /// a failure after `get_surface` could not fall back to the bridge.
    pub(super) fn negotiate_explicit_sync(
        &mut self,
        manager: u32,
        minor: u32,
    ) -> Result<ExplicitSync> {
        let Some(mut es) = ExplicitSync::new(ffi::open_drm_render_node(minor)?)? else {
            return Err(Error::msg(
                "the kernel has no syncobj eventfds (needs Linux 6.6)",
            ));
        };
        let fd = es.drm.as_raw_fd();
        let acquire = ffi::drm_syncobj_export_fd(fd, es.acquire_syncobj)?;
        let mut releases = Vec::with_capacity(SWAP_BUFFERS);
        for slot in &es.release {
            releases.push(ffi::drm_syncobj_export_fd(fd, slot.syncobj)?);
        }
        es.acquire_timeline = self.import_timeline(manager, &acquire)?;
        for (slot, syncobj_fd) in es.release.iter_mut().zip(&releases) {
            slot.timeline = self.import_timeline(manager, syncobj_fd)?;
        }
        let surface = self.alloc_id();
        self.conn.request(
            manager,
            wp_linux_drm_syncobj_manager_v1::GET_SURFACE,
            &[Arg::NewId(surface), Arg::Object(self.surface)],
        );
        es.surface = surface;
        Ok(es)
    }

    /// The fd the event loop polls for a held buffer's release, while explicit
    /// sync is on.
    pub(super) fn release_wake_fd(&self) -> Option<RawFd> {
        self.presentation
            .explicit_sync
            .as_ref()
            .map(ExplicitSync::wake_fd)
    }

    /// The release fences of retired buffers that have one. Each polls readable
    /// once it signals, which is the only wake a buffer retired after its
    /// release point got a fence has: the point's own wake has already fired.
    pub(super) fn retired_fences(&self) -> Vec<RawFd> {
        self.presentation
            .retired
            .iter()
            .filter_map(|retired| match &retired.release {
                RetiredRelease::Fence(fence) => Some(fence.as_raw_fd()),
                RetiredRelease::Point { .. } | RetiredRelease::BufferEvent => None,
            })
            .collect()
    }

    /// A held buffer's release point got its fence: clear the wake, so the next
    /// turn's frame can take that buffer, and free any retired image the
    /// release was for.
    pub(super) fn on_release_wake(&mut self) {
        if let Some(es) = self.presentation.explicit_sync.as_mut() {
            es.woke();
        }
        self.reap_retired();
    }

    /// An event on `object` if it is one of our wl_buffers, live or retired;
    /// returns whether it was. `wl_buffer.release` frees a slot on the implicit
    /// path, where it is the only release signal there is.
    pub(super) fn on_buffer_event(&mut self, object: u32, opcode: u16) -> bool {
        let released = opcode == wl_buffer::EV_RELEASE;
        if let Some(idx) = self
            .presentation
            .buffers
            .iter()
            .position(|&b| b != 0 && b == object)
        {
            if released {
                if let Some(busy) = self.presentation.busy.get_mut(idx) {
                    *busy = false;
                }
            }
            return true;
        }
        let Some(pos) = self
            .presentation
            .retired
            .iter()
            .position(|r| r.buffer != 0 && r.buffer == object)
        else {
            return false;
        };
        let by_event = self
            .presentation
            .retired
            .get(pos)
            .is_some_and(|r| matches!(r.release, RetiredRelease::BufferEvent));
        if released && by_event {
            let retired = self.presentation.retired.swap_remove(pos);
            self.free_retired(retired);
        }
        true
    }

    /// Import an exported DRM syncobj fd into the compositor as a timeline
    /// object; returns the protocol object id. The fd is sent inline, so the
    /// caller's `OwnedFd` can drop once this returns.
    fn import_timeline(&mut self, manager: u32, syncobj_fd: &OwnedFd) -> Result<u32> {
        let id = self.alloc_id();
        self.conn.request_with_fd(
            manager,
            wp_linux_drm_syncobj_manager_v1::IMPORT_TIMELINE,
            &[Arg::NewId(id)],
            syncobj_fd.as_raw_fd(),
        )?;
        Ok(id)
    }

    /// Allocate the two presentable buffers at the current size: one swap slot
    /// per buffer, each drawing into an exported GPU image that
    /// [`Self::attach_buffers`] wraps in a wl_buffer. A failure propagates and
    /// ends the process; there is no other renderer to fall back to.
    pub(super) fn create_buffers(&mut self) -> Result<()> {
        let (w, h) = (self.width, self.height);
        let mut images = Vec::with_capacity(SWAP_BUFFERS);
        {
            let Some(gpu) = self.presentation.backend.as_ref() else {
                return Err(Error::msg("gpu buffers without a gpu backend"));
            };
            for _ in 0..SWAP_BUFFERS {
                match gpu.create_image(w, h, &self.presentation.modifiers) {
                    Ok(image) => images.push(image),
                    Err(e) => {
                        // Nothing was submitted with the ones made so far.
                        for image in images {
                            gpu.destroy_image(image);
                        }
                        return Err(e);
                    }
                }
            }
        }
        self.presentation.images = images;
        self.attach_buffers()
    }

    /// Give both slots a target at the current size, and free the targets they
    /// drew into or, for any the compositor may still be reading, retire them to
    /// be freed once their release arrives. A slot's command buffer, fence,
    /// semaphores and host buffers carry over, so a resize allocates only the
    /// two images.
    ///
    /// `wait_idle` drains only *our* device; the compositor reading a buffer is
    /// a separate lifetime. So each old target is freed now only when its buffer
    /// is known released: under explicit sync a release point whose fence has
    /// signalled (or a buffer never committed), on the implicit path a slot the
    /// compositor is not marked as holding. One that is still held moves to the
    /// retired list with its wl_buffer and is freed by [`Self::reap_retired`] or
    /// its `wl_buffer.release`.
    fn resize_buffers(&mut self) -> Result<()> {
        let (w, h) = (self.width, self.height);
        let mut replaced: Vec<vulkan::RenderTarget> = Vec::with_capacity(SWAP_BUFFERS);
        let mut failure = None;
        {
            let presentation = &mut self.presentation;
            let Some(gpu) = presentation.backend.as_ref() else {
                return Err(Error::msg("gpu buffers without a gpu backend"));
            };
            // Nothing of ours may still render into the old targets, whether
            // they are freed now or held for later; a resize is rare enough
            // that a full drain is the simple correct answer.
            gpu.wait_idle();
            for img in &mut presentation.images {
                match gpu.retarget(img, w, h, &presentation.modifiers) {
                    Ok(old) => replaced.push(old),
                    Err(e) => {
                        failure = Some(e);
                        break;
                    }
                }
            }
        }
        let busy = self.presentation.busy;
        for (idx, image) in replaced.into_iter().enumerate() {
            // The slot stops naming the old wl_buffer here, so it is destroyed
            // exactly once: now, or when it is reaped.
            let buffer = self
                .presentation
                .buffers
                .get_mut(idx)
                .map(std::mem::take)
                .unwrap_or(0);
            // How the old buffer's release will be known, or `None` when the
            // compositor does not hold it.
            let pending = match self.presentation.explicit_sync.as_mut() {
                Some(es) => es.retire(idx),
                None => busy
                    .get(idx)
                    .is_some_and(|&held| held)
                    .then_some(RetiredRelease::BufferEvent),
            };
            match pending {
                Some(release) => self.presentation.retired.push(RetiredImage {
                    image,
                    buffer,
                    release,
                }),
                None => self.free_target(image, buffer),
            }
        }
        self.enforce_retired_cap();
        if let Some(e) = failure {
            return Err(e);
        }
        self.attach_buffers()
    }

    /// Wrap each slot's target in a wl_buffer via `zwp_linux_buffer_params_v1`
    /// (`create_immed` is sound because every parameter comes from the
    /// compositor's own feedback), and reset what the old buffers left behind.
    fn attach_buffers(&mut self) -> Result<()> {
        let dmabuf_global = self
            .presentation
            .dmabuf
            .ok_or_else(|| Error::msg("gpu buffers without zwp_linux_dmabuf_v1"))?;
        let (w, h) = (self.width, self.height);
        // Taken out for the loop, which needs the connection and the id
        // allocator; put back before anything can fail, so no image is ever
        // dropped without being destroyed.
        let images = std::mem::take(&mut self.presentation.images);
        let mut buffers = [0u32; SWAP_BUFFERS];
        let mut sent = Ok(());
        for (img, slot) in images.iter().zip(&mut buffers) {
            let params = self.alloc_id();
            self.conn.request(
                dmabuf_global,
                zwp_linux_dmabuf_v1::CREATE_PARAMS,
                &[Arg::NewId(params)],
            );
            let modifier = img.modifier();
            sent = self.conn.request_with_fd(
                params,
                zwp_linux_buffer_params_v1::ADD,
                &[
                    Arg::Uint(0), // plane index
                    Arg::Uint(img.offset()),
                    Arg::Uint(img.stride()),
                    Arg::Uint((modifier >> 32) as u32),
                    Arg::Uint(modifier as u32),
                ],
                img.fd(),
            );
            if sent.is_err() {
                break;
            }
            let buffer = self.alloc_id();
            self.conn.request(
                params,
                zwp_linux_buffer_params_v1::CREATE_IMMED,
                &[
                    Arg::NewId(buffer),
                    Arg::Int(w as i32),
                    Arg::Int(h as i32),
                    Arg::Uint(dmabuf::DRM_FORMAT_XRGB8888),
                    Arg::Uint(0),
                ],
            );
            self.conn
                .request(params, zwp_linux_buffer_params_v1::DESTROY, &[]);
            *slot = buffer;
        }
        self.presentation.images = images;
        sent?;
        self.presentation.buffers = buffers;
        self.presentation.buffer_size = (w, h);
        self.presentation.busy = [false; SWAP_BUFFERS];
        // Fresh images never carried a release point, so a resize must forget
        // the old buffers' points; their next use starts with no wait.
        if let Some(es) = self.presentation.explicit_sync.as_mut() {
            es.forget_buffers();
        }
        // Fresh images hold no frame, so nothing is on screen to diff against:
        // the next frame into them is damaged whole.
        self.presentation.present_list = Rc::default();
        self.presentation.presented = false;
        Ok(())
    }

    /// Free every retired image whose buffer the compositor has released under
    /// explicit sync: a poll of each release point, never a wait. Retired
    /// buffers of the implicit path are freed by their `wl_buffer.release`
    /// instead (see [`Self::on_buffer_event`]). A no-op when the list is empty,
    /// so the frame path calls it every frame.
    pub(super) fn reap_retired(&mut self) {
        let mut idx = 0;
        while idx < self.presentation.retired.len() {
            let released = match (
                self.presentation.explicit_sync.as_ref(),
                self.presentation.retired.get_mut(idx),
            ) {
                (Some(es), Some(retired)) => es.retired_released(&mut retired.release),
                _ => false,
            };
            if released {
                // `remove`, not `swap_remove`: the list stays oldest first for
                // the cap.
                let retired = self.presentation.retired.remove(idx);
                self.free_retired(retired);
            } else {
                idx += 1;
            }
        }
    }

    /// Destroy a retired image and its wl_buffer.
    fn free_retired(&mut self, retired: RetiredImage) {
        self.free_target(retired.image, retired.buffer);
    }

    /// Destroy a replaced render target and the wl_buffer that wrapped it. The
    /// device was drained before the target was replaced and nothing has
    /// rendered into it since, so no work of ours references it.
    fn free_target(&mut self, image: vulkan::RenderTarget, buffer: u32) {
        if buffer != 0 {
            self.conn.request(buffer, wl_buffer::DESTROY, &[]);
        }
        // A target exists only through a backend, so there always is one.
        if let Some(gpu) = &self.presentation.backend {
            gpu.destroy_target(image);
        }
    }

    /// Backstop for a compositor that never releases a buffer: keep the retired
    /// list bounded by freeing the oldest past the cap.
    fn enforce_retired_cap(&mut self) {
        while self.presentation.retired.len() > MAX_RETIRED_IMAGES {
            if self.stats {
                eprintln!(
                    "bnkfiles: {} resized-away buffers still unreleased; freeing the oldest",
                    self.presentation.retired.len()
                );
            }
            let retired = self.presentation.retired.remove(0);
            self.free_retired(retired);
        }
    }

    /// Record a new surface size `(w, h)`. The buffers are NOT reallocated here:
    /// an interactive resize fires many configures, and reallocating exportable
    /// dmabuf images on each one is heavy churn (a full GPU drain plus an
    /// export/import round-trip), enough to upset a compositor's buffer path.
    /// `render_frame` resizes them lazily when `buffer_size` no longer matches,
    /// so the churn collapses to one reallocation per painted frame. A no-op if
    /// the size is unchanged.
    pub(super) fn resize_to(&mut self, w: u32, h: u32) {
        if (w, h) == (self.width, self.height) {
            return;
        }
        self.width = w;
        self.height = h;
        // Nothing here holds a position that a resize invalidates: the viewer's
        // zoom and pan are re-derived (and re-clamped) against the window size
        // every frame, and the grid's layout with them. Just owe a frame.
        self.dirty = true;
    }

    /// Declare the surface's scale state, then ack configure `serial`. Sending the
    /// geometry with the ack and the matching buffer gives the compositor one
    /// atomic, consistent size for the frame, so an anchored (left/top) resize edge
    /// does not drift. Caller commits after this.
    pub(super) fn ack_configure(&mut self, serial: u32) {
        self.declare_surface_scale();
        self.conn.request(
            self.xdg_surface,
            xdg_surface::ACK_CONFIGURE,
            &[Arg::Uint(serial)],
        );
    }

    /// (Re)declare the surface's logical geometry and how the device-pixel buffer
    /// maps onto it: a viewport destination on the fractional path, or an integer
    /// buffer scale on the fallback. The geometry is the whole logical surface (no
    /// client-side decorations), the double-buffered state the compositor anchors a
    /// resize's fixed edge to. Idempotent surface state; marks the scale synced.
    pub(super) fn declare_surface_scale(&mut self) {
        let (lw, lh) = (self.scaling.logical.0 as i32, self.scaling.logical.1 as i32);
        self.conn.request(
            self.xdg_surface,
            xdg_surface::SET_WINDOW_GEOMETRY,
            &[Arg::Int(0), Arg::Int(0), Arg::Int(lw), Arg::Int(lh)],
        );
        if self.scaling.is_fractional() {
            self.conn.request(
                self.scaling.viewport,
                wp_viewport::SET_DESTINATION,
                &[Arg::Int(lw), Arg::Int(lh)],
            );
        } else {
            let n = (self.scaling.factor_120 / 120).max(1) as i32;
            self.conn
                .request(self.surface, wl_surface::SET_BUFFER_SCALE, &[Arg::Int(n)]);
        }
        self.scaling.synced = true;
    }

    /// Build the current frame and present it on the GPU. Returns false if both
    /// buffers are still held by the compositor, leaving `dirty` set so the next
    /// buffer release retries: a `wl_buffer.release` on the implicit path, the
    /// release wake under explicit sync.
    ///
    /// The display list is built and diffed against `present_list` (what is on
    /// screen): an identical frame early-outs (the common idle case), otherwise
    /// the changed screen regions are reported to the compositor as damage. The
    /// GPU repaints the full frame every present; the *screen* damage is still
    /// just the diff.
    ///
    /// A rendering failure (a device lost mid-session) propagates and ends the
    /// process; there is no other renderer to fall back to.
    pub(super) fn render_frame(&mut self) -> Result<bool> {
        // Free any image an earlier resize retired that the compositor has now
        // released. A no-op once the list drains, a frame or two after a resize.
        self.reap_retired();
        // Resize the buffers if the surface was resized since they were made.
        // Deferring this from every configure to the first frame after them
        // collapses an interactive resize's dmabuf churn to one reallocation per
        // painted frame (frame-callback paced), instead of one per configure.
        if self.presentation.buffer_size != (self.width, self.height) {
            self.resize_buffers()?;
        }
        let slot = match self.presentation.explicit_sync.as_mut() {
            Some(es) => es.free_slot()?,
            None => self.presentation.busy.iter().position(|&held| !held),
        };
        let Some(idx) = slot else {
            return Ok(false);
        };
        let started = self.stats.then(Instant::now);
        let (sw, sh) = (self.width as i32, self.height as i32);
        let new_list = Rc::new(self.build_frame_list());

        let screen_dmg = if self.presentation.presented {
            display::damage(&self.presentation.present_list, &new_list, sw, sh)
        } else {
            // Fresh buffers (startup, or a resize that replaced them) hold no
            // frame, so there is nothing to diff against: a diff with the empty
            // list would leave out every region only the clear colour covers,
            // and an empty one would read as "already on screen". The first
            // frame into them is damaged whole.
            vec![Rect {
                x: 0,
                y: 0,
                w: sw.max(0),
                h: sh.max(0),
            }]
        };
        // Acknowledge the latest configure paired with this commit, so the buffer
        // the compositor sees is always the one sized to the configure it just
        // acked; that is what keeps an anchored (left/top) resize edge from
        // jumping. Taken here so it rides the same flush as the commit below.
        let ack = self.pending_configure.take();
        // Nothing changed since the on-screen frame: nothing to present (the
        // common idle case).
        if screen_dmg.is_empty() {
            // Nothing new to draw, but a pending configure must still be acked so
            // the compositor can finalize a state-only change (e.g. a maximize
            // that altered no pixels); a bare commit applies it against the
            // current, already correctly sized buffer.
            if let Some(serial) = ack {
                self.ack_configure(serial);
                self.conn.request(self.surface, wl_surface::COMMIT, &[]);
            }
            return Ok(true);
        }
        if let Some(serial) = ack {
            self.ack_configure(serial);
        }
        // A scale change can arrive without a configure (a monitor move), so make
        // sure the geometry + viewport/buffer-scale ride this commit too.
        if !self.scaling.synced {
            self.declare_surface_scale();
        }

        // Render, returning the render-done fence under explicit sync (`None`
        // on the bridge path, or when the driver could not export one). The
        // store is where every `DrawCmd::Image` in the list resolves its pixels:
        // the picture in the viewer, and each thumbnail in the grid.
        let (render_done, undrawn, upload_failed) = {
            let Some(gpu) = self.presentation.backend.as_mut() else {
                return Err(Error::msg("gpu frame without a gpu backend"));
            };
            let dropped_before = gpu.dropped_image_uploads();
            let Some(img) = self.presentation.images.get_mut(idx) else {
                return Err(Error::msg("gpu frame without gpu buffers"));
            };
            // The glyph cache is replaced whole on a scale change, so the
            // device's limit is handed to it here rather than once at startup.
            self.presentation
                .glyphs
                .limit_atlases(gpu.max_image_dimension());
            let known = |key: &gpu::TextureKey| gpu.has_image(key);
            let frame = gpu::build_frame(
                &self.fonts,
                &self.store,
                &new_list,
                &mut self.presentation.glyphs,
                &known,
            );
            let render_done = match self.presentation.explicit_sync.as_ref() {
                Some(es) => {
                    let release_wait = es.release_fence(idx)?;
                    gpu.render_list_explicit(
                        img,
                        &frame,
                        &self.store,
                        color_f32(BACKGROUND),
                        GLYPH_GAMMA,
                        release_wait,
                    )?
                }
                None => {
                    gpu.render_list(img, &frame, &self.store, color_f32(BACKGROUND), GLYPH_GAMMA)?;
                    None
                }
            };
            let undrawn: Vec<gpu::TextureKey> = frame
                .batches
                .iter()
                .filter_map(|batch| batch.image.as_ref())
                .filter(|key| !gpu.has_image(key))
                .cloned()
                .collect();
            (
                render_done,
                undrawn,
                gpu.dropped_image_uploads() > dropped_before,
            )
        };
        // A picture whose texture could not be made is not on screen, and the
        // record of what is must say so: otherwise the frame that does draw it
        // diffs equal to this one and is never presented.
        self.presentation.present_list = if undrawn.is_empty() {
            new_list
        } else {
            Rc::new(without_images(&new_list, &undrawn))
        };
        self.presentation.presented = true;
        // Memory or descriptor sets ran out. Eviction has made room by the time
        // the next frame is built, but nothing else would ask for that frame.
        if upload_failed {
            self.retry_uploads.touch(Instant::now());
        }

        let buffer = self.presentation.buffers.get(idx).copied().unwrap_or(0);
        self.conn.request(
            self.surface,
            wl_surface::ATTACH,
            &[Arg::Object(buffer), Arg::Int(0), Arg::Int(0)],
        );
        // Explicit sync: publish this frame's acquire point and claim a release
        // point, so the compositor waits before reading and signals when done.
        // A frame with no exportable fence was CPU-waited, and its acquire point
        // goes out already signalled: a commit without the points is a protocol
        // error.
        if let Some(es) = self.presentation.explicit_sync.as_mut() {
            es.publish(&mut self.conn, render_done.as_ref(), idx)?;
            if render_done.is_some() {
                self.presentation.explicit_fence_frames += 1;
            } else {
                self.presentation.explicit_cpu_wait_frames += 1;
            }
        }
        // The GPU repainted everything, but the *screen* change is still just
        // the diff, so report those regions and let the compositor
        // recomposite only what moved. `damage_buffer`, not `damage`: the
        // rectangles are in the device pixels the buffer is drawn in.
        for r in &screen_dmg {
            self.conn.request(
                self.surface,
                wl_surface::DAMAGE_BUFFER,
                &[Arg::Int(r.x), Arg::Int(r.y), Arg::Int(r.w), Arg::Int(r.h)],
            );
        }
        self.arm_frame_callback(Instant::now());
        self.conn.request(self.surface, wl_surface::COMMIT, &[]);
        if let Some(busy) = self.presentation.busy.get_mut(idx) {
            *busy = true;
        }

        if let Some(started) = started {
            self.log_frame_stats(started);
            self.presentation.last_present = Some(started);
        }
        self.presentation.frame_count += 1;
        if self.probe {
            self.report_probe();
        }
        Ok(true)
    }

    /// Print the per-frame stats line under `BNKFILES_STATS`: the build+submit
    /// time, the gap since the last present, the glyph-cache hit rate, and which
    /// sync strategy (explicit points or the implicit bridge) carried the frame.
    fn log_frame_stats(&self, started: Instant) {
        let cache = self.presentation.glyphs.stats;
        let gap = self
            .presentation
            .last_present
            .map(|t| started.duration_since(t).as_secs_f64() * 1000.0)
            .unwrap_or(0.0);
        let sync = if self.presentation.explicit_sync.is_some() {
            format!(
                "explicit-sync {} fence/{} cpu-wait",
                self.presentation.explicit_fence_frames, self.presentation.explicit_cpu_wait_frames
            )
        } else {
            "implicit-sync bridge".to_string()
        };
        let dropped = self
            .presentation
            .backend
            .as_ref()
            .map_or(0, vulkan::Gpu::dropped_image_uploads);
        eprintln!(
            "[stats] gpu frame {} {:.2}ms build+submit, {:.1}ms since last | glyphs hit {} miss {} ({:.1}% of {}) | {} | {dropped} image uploads dropped",
            self.presentation.frame_count,
            started.elapsed().as_secs_f64() * 1000.0,
            gap,
            cache.hits,
            cache.misses,
            cache.hit_rate() * 100.0,
            cache.lookups(),
            sync,
        );
    }

    /// Ask the dmabuf global for its default feedback. The events accumulate in
    /// `dmabuf_fb` as they arrive; nothing waits on them.
    pub(super) fn request_dmabuf_feedback(&mut self) {
        let Some(dmabuf) = self.presentation.dmabuf else {
            return;
        };
        let id = self.alloc_id();
        self.conn.request(
            dmabuf,
            zwp_linux_dmabuf_v1::GET_DEFAULT_FEEDBACK,
            &[Arg::NewId(id)],
        );
        self.presentation.feedback_id = id;
    }

    /// One event of a feedback burst; `dmabuf_fb` accumulates and commits.
    pub(super) fn on_dmabuf_feedback(&mut self, opcode: u16, r: &mut Reader) -> Result<()> {
        match opcode {
            zwp_linux_dmabuf_feedback_v1::EV_FORMAT_TABLE => {
                let size = r.u32()? as usize;
                let fd = self.conn.take_fd().ok_or_else(|| {
                    Error::msg("dmabuf feedback format_table arrived without its fd")
                })?;
                if size == 0 {
                    self.presentation.feedback.set_table(&[]);
                    return Ok(());
                }
                let bytes = ffi::read_mapped(fd.as_raw_fd(), size)?;
                self.presentation.feedback.set_table(&bytes);
            }
            zwp_linux_dmabuf_feedback_v1::EV_MAIN_DEVICE => {
                self.presentation.feedback.main_device(r.array()?)?;
            }
            zwp_linux_dmabuf_feedback_v1::EV_TRANCHE_TARGET_DEVICE => {
                self.presentation.feedback.tranche_device(r.array()?)?;
            }
            zwp_linux_dmabuf_feedback_v1::EV_TRANCHE_FORMATS => {
                self.presentation.feedback.tranche_formats(r.array()?);
            }
            zwp_linux_dmabuf_feedback_v1::EV_TRANCHE_FLAGS => {
                self.presentation.feedback.tranche_flags(r.u32()?);
            }
            zwp_linux_dmabuf_feedback_v1::EV_TRANCHE_DONE => {
                self.presentation.feedback.tranche_done()
            }
            zwp_linux_dmabuf_feedback_v1::EV_DONE => self.presentation.feedback.done(),
            _ => {}
        }
        Ok(())
    }

    /// The `--gpu-probe` body: bind the registry, fetch dmabuf feedback, and
    /// print what the GPU presentation path has to work with.
    pub(super) fn probe_dmabuf(&mut self) -> Result<()> {
        self.registry = self.alloc_id();
        self.conn.request(
            protocol::WL_DISPLAY,
            wl_display::GET_REGISTRY,
            &[Arg::NewId(self.registry)],
        );
        self.roundtrip()?;
        if self.presentation.dmabuf.is_none() {
            println!(
                "gpu-probe: compositor lacks zwp_linux_dmabuf_v1 v{}+; \
                 bnkfiles cannot run here (it requires the GPU path)",
                protocol::VERSION_DMABUF
            );
            return Ok(());
        }
        println!(
            "gpu-probe: zwp_linux_dmabuf_v1 bound at v{}",
            self.presentation.dmabuf_version
        );
        match self.presentation.syncobj_manager {
            Some(_) => println!(
                "gpu-probe: {} present: explicit sync available",
                protocol::IFACE_DRM_SYNCOBJ
            ),
            None => println!(
                "gpu-probe: {} absent: GPU path uses the implicit-sync bridge",
                protocol::IFACE_DRM_SYNCOBJ
            ),
        }
        // The server answers get_default_feedback before the sync that follows
        // it, so one roundtrip delivers the whole feedback burst.
        self.request_dmabuf_feedback();
        self.roundtrip()?;
        let Some(fb) = self.presentation.feedback.ready() else {
            return Err(Error::msg(
                "compositor never committed dmabuf feedback (no done event)",
            ));
        };
        println!(
            "gpu-probe: format table: {} format+modifier pairs",
            self.presentation.feedback.table_len()
        );
        println!(
            "gpu-probe: main device: {}:{}",
            dmabuf::dev_major(fb.main_device),
            dmabuf::dev_minor(fb.main_device)
        );
        for (i, t) in fb.tranches.iter().enumerate() {
            println!(
                "gpu-probe: tranche {i}: device {}:{}, flags {:#x}, {} pairs",
                dmabuf::dev_major(t.device),
                dmabuf::dev_minor(t.device),
                t.flags,
                t.formats.len()
            );
        }
        let mods = fb.modifiers_for(dmabuf::DRM_FORMAT_XRGB8888);
        if mods.is_empty() {
            println!("gpu-probe: XRGB8888 is not offered; the GPU path needs it");
            return Ok(());
        }
        let rendered: Vec<String> = mods
            .iter()
            .map(|&m| {
                if m == dmabuf::MOD_LINEAR {
                    "linear".to_string()
                } else {
                    format!("{m:#x}")
                }
            })
            .collect();
        println!("gpu-probe: XRGB8888 modifiers: {}", rendered.join(", "));

        // The Vulkan half: can a device matching the compositor's be brought
        // up with the extensions the presentation path needs, and can it
        // actually allocate and export a presentable image? A failure here is
        // the probe's answer, not an error.
        let main_device = fb.main_device;
        let gpu = match vulkan::Gpu::new(main_device) {
            Ok(gpu) => gpu,
            Err(e) => {
                println!("gpu-probe: vulkan unavailable: {e}");
                return Ok(());
            }
        };
        println!(
            "gpu-probe: vulkan device: {} (graphics queue family {}, images up to {} pixels a side)",
            gpu.name(),
            gpu.queue_family(),
            gpu.max_image_dimension()
        );
        let common = gpu.image_modifiers(&mods);
        if common.is_empty() {
            println!("gpu-probe: no common DRM modifier; the GPU path is unavailable");
            return Ok(());
        }
        println!(
            "gpu-probe: common modifiers: {} of {}",
            common.len(),
            mods.len()
        );
        match gpu.create_image(64, 64, &common) {
            Ok(img) => {
                println!(
                    "gpu-probe: test image exported: modifier {:#x}, stride {}, offset {}",
                    img.modifier(),
                    img.stride(),
                    img.offset()
                );
                gpu.destroy_image(img);
                println!("gpu-probe: ready (bnkfiles would use the GPU path)");
            }
            Err(e) => println!("gpu-probe: image export failed: {e}"),
        }
        Ok(())
    }
}

/// A 0x00RRGGBB color as the linear-float RGBA a Vulkan clear expects. The
/// authored bytes are sRGB-encoded; the color attachment is an sRGB view, so
/// the clear value is taken as linear and re-encoded on store. Converting here
/// makes the cleared background land on the same bytes as the authored color.
fn color_f32(color: u32) -> [f32; 4] {
    let srgb_to_linear = |c: u32| {
        let c = c as f32 / 255.0;
        if c > 0.04045 {
            ((c + 0.055) / 1.055).powf(2.4)
        } else {
            c / 12.92
        }
    };
    [
        srgb_to_linear((color >> 16) & 0xff),
        srgb_to_linear((color >> 8) & 0xff),
        srgb_to_linear(color & 0xff),
        1.0,
    ]
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant};

    use crate::app::present::{
        color_f32, first_free, may_draw_at, without_images, ExplicitSync, RetiredRelease,
        SlotRelease,
    };
    use crate::platform::ffi;
    use crate::platform::geom::Rect;
    use crate::render::display::{damage, DrawCmd};
    use crate::render::gpu::TextureKey;
    use crate::render::image::ImageKey;
    use crate::theme::BACKGROUND;

    /// Explicit-sync state on this machine's first render node, or `None`,
    /// after saying why, where `test` cannot run: no render node, or a kernel
    /// without syncobj eventfds. Any other failure fails the test.
    fn explicit_sync(test: &str) -> Option<ExplicitSync> {
        let Some(drm) = ffi::open_any_render_node() else {
            eprintln!("no DRM render node; skipping {test}");
            return None;
        };
        let es = ExplicitSync::new(drm).expect("create the syncobjs");
        if es.is_none() {
            eprintln!("no syncobj eventfds (needs Linux 6.6); skipping {test}");
        }
        es
    }

    /// What the compositor does when it is done with slot `idx`'s buffer: signal
    /// the release point the commit named, on the timeline it named.
    fn release(es: &ExplicitSync, idx: usize) {
        let slot = &es.release[idx];
        let point = slot.point.expect("a published release point");
        ffi::drm_syncobj_signal_point(es.drm.as_raw_fd(), slot.syncobj, point, es.signalled)
            .expect("signal the release point");
    }

    /// The wake is an eventfd, readable while its counter is nonzero.
    fn woken(es: &ExplicitSync) -> bool {
        ffi::wait_sync_file(es.wake_fd(), 0)
    }

    #[test]
    fn the_frame_goes_into_the_first_buffer_the_compositor_does_not_hold() {
        use SlotRelease::{Held, Released, Unused};
        assert_eq!(first_free([Unused, Unused]), Some(0));
        assert_eq!(first_free([Held, Unused]), Some(1));
        assert_eq!(first_free([Held, Released]), Some(1));
        assert_eq!(first_free([Released, Held]), Some(0));
        assert_eq!(first_free([Held, Held]), None, "both held: the frame waits");
    }

    /// A hidden window gets no frame callbacks, so a configure waiting on the one
    /// in flight would stay unacked until the window is shown. Once the
    /// callback is overdue the configure draws without it; output alone never
    /// does, or an occluded window would repaint at full rate for nobody.
    #[test]
    fn a_frame_waits_for_its_callback_unless_a_configure_has_outwaited_it() {
        let now = Instant::now();
        let later = now + Duration::from_millis(5);
        assert!(may_draw_at(false, None, now), "no callback in flight");
        assert!(may_draw_at(false, Some(later), now));
        assert!(!may_draw_at(true, None, now), "nothing pending: wait");
        assert!(!may_draw_at(true, Some(later), now), "not overdue yet");
        assert!(
            may_draw_at(true, Some(later), later),
            "overdue: draw past it"
        );
    }

    /// Every commit on a syncobj surface must carry both points, also for a
    /// frame the CPU waited out: its acquire point is published signalled, and
    /// the buffer is then held until the compositor releases it.
    #[test]
    fn a_cpu_waited_frame_still_claims_a_signalled_acquire_point() {
        let Some(mut es) = explicit_sync("the acquire-point test") else {
            return;
        };
        es.acquire_timeline = 9;
        es.release[1].timeline = 11;
        let points = es.claim_points(None, 1).expect("claim the frame's points");
        assert_eq!(
            (points.acquire_timeline, points.release_timeline),
            (9, 11),
            "the acquire timeline and the slot's own release timeline"
        );
        let acquire = ffi::drm_syncobj_point_to_sync_file(
            es.drm.as_raw_fd(),
            es.acquire_syncobj,
            points.point,
            es.scratch,
        )
        .expect("the acquire point has a fence");
        assert!(ffi::wait_sync_file(acquire.as_raw_fd(), 0), "signalled");
        assert_eq!(es.slot_release(1).expect("slot 1"), SlotRelease::Held);
        assert_eq!(es.slot_release(0).expect("slot 0"), SlotRelease::Unused);
    }

    /// Frames go into buffer 0 and then buffer 1, and the compositor finishes
    /// with buffer 1 first, which the protocol allows. Buffer 0 is still being
    /// read, so it must not read as released until its own point signals.
    #[test]
    fn a_buffer_released_out_of_order_does_not_release_the_other() {
        let Some(mut es) = explicit_sync("the release-order test") else {
            return;
        };
        es.claim_points(None, 0).expect("frame into buffer 0");
        es.claim_points(None, 1).expect("frame into buffer 1");

        release(&es, 1);
        assert_eq!(es.slot_release(1).expect("slot 1"), SlotRelease::Released);
        assert_eq!(
            es.slot_release(0).expect("slot 0"),
            SlotRelease::Held,
            "buffer 0 is still being read"
        );
        release(&es, 0);
        assert_eq!(es.slot_release(0).expect("slot 0"), SlotRelease::Released);
        let fence = es.release_fence(0).expect("export").expect("a fence");
        assert!(ffi::wait_sync_file(fence.as_raw_fd(), 0));
    }

    /// The compositor holds both buffers and never sends `wl_buffer.release`,
    /// which the protocol allows under explicit sync. The frame waits, and the
    /// release of one buffer both wakes the event loop and frees that buffer.
    #[test]
    fn a_release_point_alone_frees_a_held_buffer_and_wakes_the_loop() {
        let Some(mut es) = explicit_sync("the release-wake test") else {
            return;
        };
        assert_eq!(es.free_slot().expect("poll"), Some(0), "unused buffer 0");
        es.claim_points(None, 0).expect("frame into buffer 0");
        assert_eq!(es.free_slot().expect("poll"), Some(1), "unused buffer 1");
        es.claim_points(None, 1).expect("frame into buffer 1");

        assert_eq!(es.free_slot().expect("poll"), None, "both buffers are held");
        assert!(!woken(&es), "nothing has been released");
        assert_eq!(es.free_slot().expect("poll"), None, "still held");

        release(&es, 1);
        assert!(woken(&es), "the release wakes the loop");
        es.woke();
        assert!(!woken(&es), "the wake is cleared");
        assert_eq!(es.free_slot().expect("poll"), Some(1), "buffer 1 is free");
    }

    /// The compositor keeps buffer 0 while buffer 1 goes round many times, each
    /// round a wait with both held. Every registration is a kernel allocation
    /// that lives until it fires, so buffer 0's one point must carry one
    /// registration, not one per wait.
    #[test]
    fn a_held_buffer_keeps_one_wake_registration_while_the_other_cycles() {
        use std::io::Read;

        let Some(mut es) = explicit_sync("the registration test") else {
            return;
        };
        // Reading the eventfd returns how many registrations fired since the
        // last read.
        let mut wake = std::fs::File::from(es.wake.try_clone().expect("dup the wake"));
        let mut fired = || {
            let mut count = [0u8; 8];
            wake.read_exact(&mut count).expect("a registration fired");
            u64::from_ne_bytes(count)
        };

        es.claim_points(None, 0).expect("frame into buffer 0");
        es.claim_points(None, 1).expect("frame into buffer 1");
        for _ in 0..100 {
            assert_eq!(es.free_slot().expect("poll"), None, "both held");
            release(&es, 1);
            assert_eq!(fired(), 1, "buffer 1's release");
            assert_eq!(es.free_slot().expect("poll"), Some(1));
            es.claim_points(None, 1).expect("frame into buffer 1");
        }
        release(&es, 0);
        assert_eq!(fired(), 1, "buffer 0's release fires one registration");
    }

    /// A resize while both old buffers are held and the wake is armed: when both
    /// new buffers are held too, a new buffer's release must still wake the loop.
    #[test]
    fn a_wake_armed_before_a_resize_still_wakes_on_a_new_buffers_release() {
        let Some(mut es) = explicit_sync("the resize wake test") else {
            return;
        };
        es.claim_points(None, 0).expect("frame into old buffer 0");
        es.claim_points(None, 1).expect("frame into old buffer 1");
        assert_eq!(es.free_slot().expect("poll"), None, "both old buffers held");

        es.forget_buffers();
        es.claim_points(None, 0).expect("frame into new buffer 0");
        es.claim_points(None, 1).expect("frame into new buffer 1");
        assert_eq!(es.free_slot().expect("poll"), None, "both new buffers held");

        release(&es, 0);
        assert!(woken(&es), "the new buffer's release wakes the loop");
    }

    /// A resize takes a buffer out of its slot. One the compositor never had, or
    /// has released, is freed at once; one it still holds is kept until its
    /// release point signals, and that release wakes the loop to free it.
    #[test]
    fn a_resized_away_buffer_is_kept_until_its_release_point_signals() {
        let Some(mut es) = explicit_sync("the retire test") else {
            return;
        };
        assert!(es.retire(0).is_none(), "never committed: free at once");

        es.claim_points(None, 0).expect("frame into buffer 0");
        es.claim_points(None, 1).expect("frame into buffer 1");
        release(&es, 1);
        assert!(es.retire(1).is_none(), "released: free at once");

        let mut held = es.retire(0).expect("still held: retired");
        assert!(matches!(held, RetiredRelease::Point { slot: 0, .. }));
        assert!(!es.retired_released(&mut held), "the compositor has it");
        // The resize goes on to put a new buffer in the slot.
        es.forget_buffers();
        assert!(!es.retired_released(&mut held), "still the compositor's");

        let RetiredRelease::Point { point, .. } = held else {
            panic!("no fence yet");
        };
        ffi::drm_syncobj_signal_point(
            es.drm.as_raw_fd(),
            es.release[0].syncobj,
            point,
            es.signalled,
        )
        .expect("the compositor releases the old buffer");
        assert!(woken(&es), "the release wakes the loop to free the image");
        assert!(es.retired_released(&mut held), "released: free it");
        assert!(matches!(held, RetiredRelease::Fence(_)));
    }

    /// The store-side sRGB encode the attachment applies, inverse of the decode
    /// in color_f32.
    fn linear_to_srgb(c: f32) -> f32 {
        if c > 0.003_130_8 {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        } else {
            12.92 * c
        }
    }

    #[test]
    fn clear_color_decode_then_store_encode_reproduces_the_authored_bytes() {
        for color in [
            0x0000_0000,
            0x00ff_ffff,
            0x0080_8080,
            0x0012_3456,
            BACKGROUND,
        ] {
            let lin = color_f32(color);
            let enc = |c: f32| (linear_to_srgb(c) * 255.0).round() as u32;
            let got = (enc(lin[0]) << 16) | (enc(lin[1]) << 8) | enc(lin[2]);
            assert_eq!(
                got, color,
                "0x{color:08x} must survive decode then re-encode"
            );
            assert_eq!(lin[3], 1.0, "the clear is opaque");
        }
    }

    /// A picture whose texture could not be made was left out of the frame. The
    /// frame that does draw it has the same display list, so unless the record
    /// of what is on screen leaves the picture out too, that frame diffs as
    /// "nothing changed" and the cell stays blank.
    #[test]
    fn a_picture_that_could_not_upload_is_damaged_when_it_does() {
        let rect = |x| Rect {
            x,
            y: 0,
            w: 64,
            h: 64,
        };
        let image = |name: &str, x| DrawCmd::Image {
            rect: rect(x),
            key: ImageKey::full(name),
            generation: 7,
        };
        let list = vec![image("drawn.png", 0), image("dropped.png", 100)];
        let undrawn = [TextureKey {
            image: ImageKey::full("dropped.png"),
            generation: 7,
        }];

        let on_screen = without_images(&list, &undrawn);
        assert_eq!(on_screen, vec![image("drawn.png", 0)]);
        assert_eq!(damage(&on_screen, &list, 800, 600), vec![rect(100)]);

        // An older decode of the same file is a different texture, and one that
        // did draw stays in the record.
        let stale = [TextureKey {
            image: ImageKey::full("dropped.png"),
            generation: 6,
        }];
        assert_eq!(without_images(&list, &stale), list);
    }
}
