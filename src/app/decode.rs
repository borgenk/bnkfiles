//! The decode pipeline: which pictures to ask the loader for, what to do
//! with each answer, and which decoded pictures to keep in memory.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::app::{State, ViewMode, PRELOAD_AHEAD, RESIDENT_FULL_BUDGET};
use crate::loader::{Class, Job, Outcome, Req};
use crate::render::image::{Image, ImageKey};
use crate::{files, thumbcache};

impl State {
    /// The store key for image `i` of the gallery.
    pub(super) fn full_key(&self, i: usize) -> Option<ImageKey> {
        Some(ImageKey::full(self.images.get(i)?))
    }

    /// The current thumbnail edge, in device pixels, the grid decodes to.
    pub(super) fn thumb_size(&self) -> u32 {
        let edge = if self.masonry_now() {
            self.masonry_edge()
        } else {
            self.grid_metrics().thumb
        };
        edge.max(1) as u32
    }

    /// The store key for a file's thumbnail *at the current scale*, so a scale
    /// change does not let a stale-size result land where the right one belongs.
    ///
    /// Keyed by path rather than by index, because the two views index different
    /// things — the grid draws directory rows, the viewer walks a gallery — and one
    /// file's thumbnail is one file's thumbnail either way.
    pub(super) fn thumb_key_of(&self, path: &Path) -> ImageKey {
        ImageKey::thumb(path, self.thumb_size())
    }

    /// The store key for the thumbnail of explorer row `row`, when that row is a
    /// picture with a thumbnail to have.
    pub(super) fn row_thumb_key(&self, row: usize) -> Option<ImageKey> {
        let entry = self.row_entry(row)?;
        (!entry.is_dir() && files::is_image(&entry.path)).then(|| self.thumb_key_of(&entry.path))
    }

    /// Adopt a folder's image list and select the one at `start`. Nothing is
    /// decoded here: the grid shows thumbnails, and the viewer asks for the full
    /// image when it is entered, so a grid open pays for no decode it never uses.
    ///
    /// An empty list is a real state, not a no-op: it is a folder whose scan has
    /// not landed yet, and the gallery has to be emptied of the last folder's
    /// pictures before it does. Callers set `current_dir`; deriving it from an
    /// image's parent could not answer for a folder with no images in it.
    pub fn open_images(&mut self, images: Vec<PathBuf>, start: usize) {
        let start = start.min(images.len().saturating_sub(1));
        self.images = images;
        self.index = start;
        // Opening a (possibly different) folder: drop everything decoded for the
        // old one. The generation bump disowns the decodes still in flight for it;
        // telling the loader lets it cancel the ones it has not started rather than
        // decode them only for the result to be thrown away.
        self.generation = self.generation.wrapping_add(1);
        self.loader.set_generation(self.generation);
        self.store.clear();
        self.inflight.clear();
        self.undrawn.clear();
        self.shown = None;
        self.grid_scroll = 0.0;
        self.dirty = true;
        self.refresh_title();
    }

    /// Queue a background decode of the full image `i` at priority `class`, unless it
    /// is resident or already in flight.
    pub(super) fn request_full(&mut self, i: usize, class: Class) {
        let Some(key) = self.full_key(i) else { return };
        if self.store.contains(&key) || self.inflight.contains(&key) {
            return;
        }
        let Some(path) = self.images.get(i).cloned() else {
            return;
        };
        self.enqueue(key, i, path, Job::Full, class);
    }

    /// Queue a background decode of explorer row `row`'s thumbnail at priority
    /// `class`, unless it is resident, already in flight, or not a picture.
    pub(super) fn request_thumb(&mut self, row: usize, class: Class) {
        let Some(key) = self.row_thumb_key(row) else {
            return;
        };
        if self.store.contains(&key) || self.inflight.contains(&key) {
            return;
        }
        // A saved thumbnail that still describes this file is the picture already:
        // the payload is raw pixels, so there is nothing to decode and no worker
        // to wait for. This is what makes a warm start paint immediately.
        if self.adopt_saved_thumb(row, &key) {
            return;
        }
        let Some(path) = self.row_entry(row).map(|e| e.path.clone()) else {
            return;
        };
        let size = self.thumb_size();
        self.enqueue(key, row, path, Job::Thumb { size }, class);
    }

    /// Take the saved thumbnail for `row` into the store, if there is one and it
    /// still describes the file. Reports whether it did.
    pub(super) fn adopt_saved_thumb(&mut self, row: usize, key: &ImageKey) -> bool {
        let edge = self.thumb_size().min(u16::MAX as u32) as u16;
        let Some(entry) = self.row_entry(row) else {
            return false;
        };
        let Some(saved) = entry
            .path
            .file_name()
            .and_then(|n| self.saved_thumbs.get(n))
        else {
            return false;
        };
        if !saved.matches(entry, edge) {
            return false;
        }
        let (w, h) = (saved.width, saved.height);
        let Some(pixels) = thumbcache::unpack_pixels(&saved.pixels, w, h) else {
            return false;
        };
        self.pipe_stats.thumbs_reused += 1;
        self.store.insert(
            key.clone(),
            Image {
                pixels,
                width: w as u32,
                height: h as u32,
            },
        );
        self.dirty = true;
        true
    }

    /// Mark a store key in flight and hand the request to the loader.
    pub(super) fn enqueue(
        &mut self,
        key: ImageKey,
        index: usize,
        path: PathBuf,
        job: Job,
        class: Class,
    ) {
        self.inflight.insert(key.clone());
        self.pipe_stats.peak_inflight = self.pipe_stats.peak_inflight.max(self.inflight.len());
        self.loader.request(Req {
            key,
            index,
            path,
            job,
            generation: self.generation,
            class,
        });
    }

    /// Drain the finished decodes into the store, showing the image the viewer is
    /// waiting on if it has just arrived.
    pub(super) fn handle_decodes(&mut self) {
        self.loader.drain_wake();
        // A thumbnail can change its picture's shape, and with it the masonry: the cell in the middle stays where it was on screen.
        let anchor = self.grid_anchor();
        let shapes = self.aspects_rev;
        while let Some(resp) = self.loader.try_recv() {
            // Every response clears its in-flight key, whatever the outcome, so the
            // key can be requested again. The key is absolute (it carries the size for
            // a thumbnail), so this never clears the wrong entry.
            self.inflight.remove(&resp.key);
            let thumb = matches!(resp.job, Job::Thumb { .. });
            match resp.outcome {
                Outcome::Skipped => self.pipe_stats.queued_skipped += 1,
                Outcome::Failed(e) => eprintln!("bnkfiles: {e}"),
                Outcome::Decoded(image) => {
                    // A decode that ran to completion for a folder the user has since
                    // left: its key is not in this folder, so it is dropped rather than
                    // stored, and counted as work that could not be cancelled in time.
                    if resp.generation != self.generation {
                        self.pipe_stats.active_stale_completed += 1;
                        continue;
                    }
                    if thumb {
                        self.pipe_stats.thumb_decoded += 1;
                    } else {
                        self.pipe_stats.full_decoded += 1;
                    }
                    // Whether this is the picture the viewer is waiting on is asked
                    // of the key, not of the index it was requested at: a scan
                    // landing mid-decode renumbers the gallery, and the key is the
                    // one thing that still means the same file afterwards.
                    let is_target = !thumb && self.full_key(self.index).as_ref() == Some(&resp.key);
                    // Track it as decoded-but-not-drawn; the paint pass clears it once
                    // it reaches the screen, and eviction counts whatever it never did.
                    self.undrawn.insert(resp.key.clone());
                    if thumb {
                        self.note_shape(&resp.key.path, image.width, image.height);
                    }
                    self.store.insert(resp.key, image);
                    if thumb {
                        self.thumbs_unsaved = true;
                        self.save_thumbs_at.touch(Instant::now());
                        // The grid draws thumbnails, and so does the inspector's
                        // preview, in either layout.
                        if self.mode == ViewMode::Grid || self.inspector_thumb_row().is_some() {
                            self.dirty = true;
                        }
                    } else if is_target && self.mode == ViewMode::Viewer {
                        self.set_shown(self.index);
                    }
                }
            }
        }
        // The resident high-water is sampled before eviction trims it back.
        let bytes = self.store.bytes();
        self.pipe_stats.peak_resident_bytes = self.pipe_stats.peak_resident_bytes.max(bytes);
        self.evict();
        if self.aspects_rev != shapes {
            if let Some(anchor) = anchor {
                self.hold_anchor(anchor);
            }
            self.settle_auto_shape();
            self.dirty = true;
        }
    }

    /// Decode image `i` on this thread. Only for the first image of a session,
    /// before the window is mapped (see `enter_viewer`); every later decode goes
    /// through the loader so the event loop never stalls.
    pub(super) fn load_now(&mut self, i: usize) {
        let Some(key) = self.full_key(i) else { return };
        if self.store.contains(&key) {
            return;
        }
        let Some(path) = self.images.get(i) else {
            return;
        };
        match Image::load(path) {
            Ok(image) => self.store.insert(key, image),
            Err(e) => eprintln!("bnkfiles: {e}"),
        }
    }

    /// Keep only what is on screen or about to be: the thumbnails around the grid's
    /// viewport (small, all kept), and the full images of the working set nearest the
    /// current index, up to [`RESIDENT_FULL_BUDGET`]. A folder of 4K photographs is
    /// gigabytes decoded, so this is what bounds the memory a long browse costs; a
    /// byte budget rather than a bare count of neighbours so the bound holds whatever
    /// the image size.
    pub(super) fn evict(&mut self) {
        if self.images.is_empty() && self.rows.is_empty() {
            self.store.clear();
            self.undrawn.clear();
            return;
        }
        let mut keep: HashSet<ImageKey> = HashSet::new();
        for row in self
            .visible_thumbs()
            .into_iter()
            .chain(self.inspector_thumb_row())
        {
            if let Some(k) = self.row_thumb_key(row) {
                keep.insert(k);
            }
        }
        // Full images nearest-first; once the budget is full a farther one is dropped,
        // but the current and shown images are always kept even if one alone exceeds
        // it (there is no not showing them).
        let mut bytes = 0usize;
        for j in self.working_full_indices() {
            let Some(k) = self.full_key(j) else { continue };
            if let Some(size) = self.store.get(&k).map(Image::byte_len) {
                let protected = j == self.index || Some(j) == self.shown;
                if !protected && bytes + size > RESIDENT_FULL_BUDGET && !keep.is_empty() {
                    continue;
                }
                bytes += size;
            }
            keep.insert(k);
        }
        // Anything decoded but never drawn that is about to be evicted was wasted
        // work: a row that scrolled past before its thumbnail landed.
        self.pipe_stats.decoded_then_unused +=
            self.undrawn.iter().filter(|k| !keep.contains(*k)).count() as u64;
        self.undrawn.retain(|k| keep.contains(k));
        self.store.retain(|k| keep.contains(k));
    }

    /// The explorer rows whose thumbnails are worth having resident: those on
    /// screen, plus a screen of margin either way so a scroll does not begin with a
    /// blank. Empty in the viewer, which is not showing any.
    pub(super) fn visible_thumbs(&self) -> Vec<usize> {
        if self.mode != ViewMode::Grid {
            return Vec::new();
        }
        let Some(content) = self.content_rect() else {
            return Vec::new();
        };
        let h = content.h as f32;
        self.grid_layout()
            .cells_overlapping(self.rows.len(), -h, 2.0 * h)
            .collect()
    }

    /// The index the user's attention centres on, handed to the loader so queued
    /// decodes run nearest-first. In the viewer that is the picture on screen; in
    /// the grid, the cell nearest the middle of the visible area, which tracks a
    /// wheel scroll where the selection does not.
    ///
    /// The two count different things — gallery images and directory rows — but the
    /// loader only ever measures distances between requests made in the same view,
    /// so a switch merely re-orders a queue rather than mis-ordering one.
    pub(super) fn focus_index(&self) -> usize {
        match self.mode {
            ViewMode::Viewer => self.index.min(self.images.len().saturating_sub(1)),
            ViewMode::List => self.sel.lead().unwrap_or(0),
            ViewMode::Grid => {
                let n = self.rows.len();
                if n == 0 {
                    return 0;
                }
                let Some(content) = self.content_rect() else {
                    return self.sel.lead().unwrap_or(0).min(n - 1);
                };
                self.grid_layout().centre(n, content.h)
            }
        }
    }

    /// Re-point the loader's focus if the user's centre of attention has moved, so
    /// queued decodes are re-prioritized around where they are now looking.
    pub(super) fn sync_focus(&mut self) {
        let focus = self.focus_index();
        if focus != self.last_focus {
            self.last_focus = focus;
            self.loader.set_focus(focus);
        }
    }

    /// Ask for the thumbnails the grid is about to show. Cheap to call every frame:
    /// a resident or in-flight one is not requested again. Cells actually on screen
    /// are requested at [`Class::Visible`]; the margin kept ready for a scroll is
    /// [`Class::Margin`], so a fast scroll's newly-visible rows out-rank the margin
    /// work queued for where the scroll used to be.
    pub(super) fn ensure_thumbs(&mut self) {
        let Some(content) = self.content_rect() else {
            return;
        };
        let layout = self.grid_layout();
        for row in self.visible_thumbs() {
            let Some(tile) = layout.tile(row) else {
                continue;
            };
            let class = if tile.y > -tile.h && tile.y < content.h as f32 {
                Class::Visible
            } else {
                Class::Margin
            };
            self.request_thumb(row, class);
        }
    }

    /// The row whose thumbnail the inspector's preview shows, while it is shown
    /// in the explorer. The viewer's preview is the viewer's own picture.
    pub(super) fn inspector_thumb_row(&self) -> Option<usize> {
        let shown = self.configured && self.regions().inspector.w > 0;
        (shown && self.mode.is_explorer())
            .then(|| self.inspected_row())
            .flatten()
    }

    /// The full-image indices worth keeping resident, nearest-first: the current
    /// image, one ahead and one behind, then deeper ahead in the direction of travel.
    /// Preload asks for these and eviction keeps them (within the byte budget), so the
    /// two never disagree about the working set.
    pub(super) fn working_full_indices(&self) -> Vec<usize> {
        let n = self.images.len();
        if n == 0 {
            return Vec::new();
        }
        let ni = n as i32;
        let dir = if self.nav_dir >= 0 { 1 } else { -1 };
        let mut offsets = vec![0, dir, -dir];
        for d in 2..=PRELOAD_AHEAD as i32 {
            offsets.push(dir * d);
        }
        let mut v = Vec::new();
        for off in offsets {
            let idx = (self.index as i32 + off).rem_euclid(ni) as usize;
            if !v.contains(&idx) {
                v.push(idx);
            }
        }
        if let Some(s) = self.shown {
            if !v.contains(&s) {
                v.push(s);
            }
        }
        v
    }

    /// Preload the working set ahead of the user, so the next arrow press is a hit
    /// rather than a wait. The current image is [`show_target`]'s (`Class::Target`);
    /// its neighbours come here at `Class::Preload`.
    pub(super) fn preload(&mut self) {
        if self.images.is_empty() {
            return;
        }
        for idx in self.working_full_indices() {
            if idx != self.index {
                self.request_full(idx, Class::Preload);
            }
        }
        self.evict();
    }

    /// Put image `i` on screen, resetting the view to fit.
    pub(super) fn set_shown(&mut self, i: usize) {
        if self.shown != Some(i) {
            self.shown = Some(i);
            self.viewer.reset();
            self.refresh_title();
            self.dirty = true;
        }
    }

    /// Make sure the picture the viewer is waiting on is at least asked for.
    ///
    /// Called every repaint. A request can go missing without anything noticing —
    /// reopening a folder disowns the decodes in flight for it — and re-asking is
    /// free whenever the picture is resident or already queued, which it nearly
    /// always is. Without it the viewer can sit on the previous image indefinitely,
    /// since nothing else re-requests the target until the user navigates again.
    pub(super) fn ensure_target(&mut self) {
        self.request_full(self.index, Class::Target);
    }

    /// Show the current index now if it is resident, else ask for it and keep the
    /// current image up until it arrives.
    pub(super) fn show_target(&mut self) {
        let i = self.index;
        match self.full_key(i) {
            Some(key) if self.store.contains(&key) => {
                self.pipe_stats.show_hits += 1;
                self.set_shown(i);
            }
            Some(_) => {
                self.pipe_stats.show_misses += 1;
                self.request_full(i, Class::Target);
            }
            None => {}
        }
    }
}
