//! The folder listing: scanning a directory, watching it for changes,
//! adopting what a scan found, and saving its thumbnails.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::app::{Focus, PendingFocus, State, ViewMode, WRITES};
use crate::files::{self, FsEntry};
use crate::jobs::Writes;
use crate::notice::Level;
use crate::platform::inotify;
use crate::scan::Payload;
use crate::thumbcache::{self, Cached};
use crate::unreadable::Reason;

impl State {
    /// Save the thumbnails of the directory being shown.
    ///
    /// What gets written is what is resident plus whatever was loaded and is still
    /// valid, so the cache accumulates across visits instead of shrinking to
    /// whichever screenful was last on show. It is cut to
    /// [`thumbcache::DIR_BUDGET`], newly made thumbnails first, since those are
    /// the ones that cost a decode to get back.
    ///
    /// Called when the shown directory changes, a little after decodes settle, and
    /// once on the way out. Only the gathering happens here; the write is the
    /// [`thumbcache::Writer`]'s.
    pub(super) fn save_thumbs(&mut self) {
        if WRITES != Writes::Allowed {
            return;
        }
        if self.thumb_writer.is_none() || self.rows.is_empty() {
            return;
        }
        let edge = self.thumb_size().min(u16::MAX as u32) as u16;
        let (mut out, mut kept): (Vec<Cached>, Vec<Cached>) = (Vec::new(), Vec::new());
        for row in 0..self.rows.len() {
            let Some(entry) = self.row_entry(row) else {
                continue;
            };
            // Already on disk and still describing this file: keep it, so a folder
            // built up over several visits stays built up.
            if let Some(saved) = entry
                .path
                .file_name()
                .and_then(|n| self.saved_thumbs.get(n))
            {
                if saved.matches(entry, edge) {
                    kept.push(saved.clone());
                    continue;
                }
            }
            let Some(image) = self.row_thumb_key(row).and_then(|key| self.store.get(&key)) else {
                continue;
            };
            let (Ok(width), Ok(height)) = (u16::try_from(image.width), u16::try_from(image.height))
            else {
                continue;
            };
            let Some(pixels) = thumbcache::pack_pixels(&image.pixels, width, height) else {
                continue;
            };
            let Some(name) = entry.path.file_name() else {
                continue;
            };
            out.push(Cached {
                name: name.to_os_string(),
                mtime_secs: entry.mtime_secs,
                size: entry.size,
                edge,
                width,
                height,
                pixels,
            });
        }
        out.append(&mut kept);
        thumbcache::fit(&mut out, thumbcache::DIR_BUDGET);
        self.thumbs_unsaved = false;
        // A directory whose thumbnails are all gone should not leave a stale file
        // behind, and a save handed nothing removes it.
        if let Some(writer) = self.thumb_writer.as_ref() {
            writer.save(self.scan_dir.clone(), out);
        }
    }

    /// Point the scanner at `dir` and forget the listing we had.
    ///
    /// The generation bump abandons whatever is being read now, so walking quickly
    /// through directories does not queue up scans of every one passed through.
    pub(super) fn request_scan(&mut self, dir: PathBuf) {
        // Leaving a directory is the moment to write its thumbnails down. A
        // refresh of the same one is not leaving it, so it does not pay for a save.
        if self.scan_dir != dir {
            self.save_thumbs();
            self.saved_thumbs.clear();
            // Shapes and what they decided are about the folder they were seen in.
            self.aspects.clear();
            self.aspects_rev = self.aspects_rev.wrapping_add(1);
            self.auto_masonry = None;
            self.scan_dir = dir.clone();
            self.refresh_shape_here();
            // Named for where it is now, not for when the folder has been
            // read: one that never is, or is slow to be, is still here.
            self.refresh_title();
            // A filter is about the folder it was typed in.
            self.filter.clear();
            if self.focus == Focus::Filter {
                self.focus = Focus::Entries;
            }
        }
        self.scan_generation = self.scan_generation.wrapping_add(1);
        self.reveal_on_adopt = false;
        self.scan_dir = dir.clone();
        self.listing.clear();
        self.refresh = None;
        self.rows.clear();
        self.rows_rev = self.rows_rev.wrapping_add(1);
        self.sel.set_len(0);
        self.scan_done = false;
        self.unreadable = None;
        self.list_scroll = 0.0;
        self.grid_scroll = 0.0;
        self.explorer_bar.hide();
        self.watch_dir(&dir);
        self.scanner.scan(dir, self.scan_generation, self.order);
        self.dirty = true;
    }

    /// Point the directory watch at `dir`, dropping whatever it watched before.
    ///
    /// A directory that cannot be watched — a filesystem with no inotify, a watch
    /// limit reached — is one the explorer simply does not refresh by itself. The
    /// listing is unaffected, so this failure is not worth interrupting anyone
    /// over; it is reported only under `BNKFILES_STATS`.
    pub(super) fn watch_dir(&mut self, dir: &Path) {
        // Already watching this exact directory: leave it alone. Re-arming means
        // dropping the watch and adding it back, and the drop queues an
        // IN_IGNORED that this very watch is then woken by — so a refresh would
        // ask for another refresh, and one `touch` would rescan forever.
        if self.watch_wd.is_some() && self.watched.as_deref() == Some(dir) {
            return;
        }
        let Some(watch) = self.watch.as_ref() else {
            return;
        };
        if let Some(old) = self.watch_wd.take() {
            watch.rm(old);
        }
        self.watched = None;
        match watch.add_dir(dir) {
            Ok(wd) => {
                self.watch_wd = Some(wd);
                self.watched = Some(dir.to_path_buf());
            }
            Err(e) => {
                if self.stats {
                    eprintln!("bnkfiles: {e}");
                }
            }
        }
    }

    /// Drain the watch and arm the debounce. What changed is not read: any event
    /// means the directory is not what was scanned, and the answer to all of them
    /// is the same single rescan.
    pub(super) fn handle_watch(&mut self) {
        let mut buf = std::mem::take(&mut self.watch_buf);
        let mut touched = false;
        let mut ended = false;
        let current = self.watch_wd;
        if let Some(watch) = self.watch.as_ref() {
            // A drain that fails leaves the queue alone; the next poll retries.
            let _ = watch.for_each_event(&mut buf, |event| match event {
                // Only the directory being shown. A watch that has been dropped
                // still has its farewell sitting in the queue, and that is news
                // about a directory already left.
                inotify::Event::Changed(wd) => touched |= Some(wd) == current,
                inotify::Event::Ended(wd) => ended |= Some(wd) == current,
                // Lost events may have been about this directory.
                inotify::Event::Overflowed => touched = true,
            });
        }
        self.watch_buf = buf;
        if ended {
            // The kernel dropped the watch: the directory was deleted, moved onto
            // another mount, or unmounted. Forgetting it is what lets the rescan
            // arm a fresh one, on whatever is at that path now.
            self.watch_wd = None;
            touched = true;
        }
        if touched {
            self.rescan.touch(Instant::now());
        }
    }

    /// Write the thumbnail cache once the decodes have stopped arriving.
    pub(super) fn tick_thumb_save(&mut self) {
        if self.save_thumbs_at.fired(Instant::now()) && self.thumbs_unsaved {
            self.save_thumbs();
        }
    }

    /// Re-read the directory once its changes have settled.
    pub(super) fn tick_rescan(&mut self) {
        if self.rescan.fired(Instant::now()) {
            self.pipe_stats.rescans += 1;
            self.refresh_listing();
        }
    }

    /// Re-read the directory being shown, keeping the user's place in it.
    ///
    /// A file appearing in a folder someone is reading through must not throw them
    /// back to the top, drop what they had selected, or blank the list while it is
    /// read again. So the new listing is read aside and swapped in whole (see
    /// `refresh`), and the selection follows its entries across the swap.
    pub(super) fn refresh_listing(&mut self) {
        self.scan_generation = self.scan_generation.wrapping_add(1);
        self.refresh = Some(Vec::new());
        let dir = self.scan_dir.clone();
        // A watch the kernel dropped is armed again here, on what the path names now.
        self.watch_dir(&dir);
        self.scanner.scan(dir, self.scan_generation, self.order);
    }

    // ---- what the scan worker reports --------------------------------------

    /// Fold in whatever the scan worker has read: merge the batches, re-project the
    /// explorer's rows, and adopt the gallery when the directory finishes.
    pub(super) fn handle_scans(&mut self) {
        self.scanner.drain_wake();
        let mut changed = false;
        let mut finished = false;
        let mut failed = false;
        let mut vanished = false;
        let mut reveal = false;
        // What the selection is on, before the rows under it move. A folder still
        // streaming in with nothing selected has no place to keep: its cursor
        // stays on the first row rather than riding whichever entry arrived first.
        let held = (self.refresh.is_some() || self.sel.count() > 0).then(|| self.held_rows());
        while let Some(msg) = self.scanner.try_recv() {
            // A batch for a directory we have left. Its entries mean nothing here.
            if msg.generation != self.scan_generation {
                continue;
            }
            match msg.payload {
                // The directory's saved thumbnails, read before its entries.
                // Keyed by name, which is what the fingerprint check compares.
                Payload::Cached(saved) => {
                    self.pipe_stats.thumbs_from_cache += saved.len() as u64;
                    self.saved_thumbs = saved.into_iter().map(|c| (c.name.clone(), c)).collect();
                    // A saved thumbnail's shape is its picture's, whatever size
                    // it was saved at, so the rows can be laid out before any
                    // of them is decoded.
                    for c in self.saved_thumbs.values() {
                        if c.width > 0 && c.height > 0 {
                            let shape = c.width as f32 / c.height as f32;
                            self.aspects.insert(c.name.clone(), shape);
                        }
                    }
                    self.aspects_rev = self.aspects_rev.wrapping_add(1);
                }
                Payload::Batch(batch) => {
                    self.pipe_stats.scan_entries += batch.len() as u64;
                    match self.refresh.as_mut() {
                        Some(staged) => files::merge_sorted(staged, batch, self.order),
                        None => {
                            files::merge_sorted(&mut self.listing, batch, self.order);
                            changed = true;
                        }
                    }
                }
                Payload::Done { total } => {
                    self.pipe_stats.scans_done += 1;
                    self.pipe_stats.scan_largest = self.pipe_stats.scan_largest.max(total);
                    if let Some(staged) = self.refresh.take() {
                        self.adopt_refresh(staged);
                        self.unreadable = None;
                        reveal = std::mem::take(&mut self.reveal_on_adopt);
                    }
                    self.scan_done = true;
                    finished = true;
                    changed = true;
                }
                Payload::Failed(failure) => {
                    // Terminal, like Done: the directory is not coming, so nothing
                    // is still waiting on it.
                    // A refresh that broke off part way has found entries gone or
                    // unreadable; what it read is all there is to show. One that
                    // could not open the folder at all read nothing, and its
                    // pictures are not pruned: the viewer may be showing one of
                    // them, decoded, and it can stay up though its file has gone.
                    let refreshed = self.refresh.is_some();
                    if let Some(staged) = self.refresh.take() {
                        if failure.partial {
                            self.adopt_refresh(staged);
                        } else {
                            self.listing = staged;
                        }
                        reveal = std::mem::take(&mut self.reveal_on_adopt);
                    }
                    // A re-read of a folder that was read, which is now not
                    // there: it went away while it was on screen. A folder still
                    // on its first read has not been seen, and is not this.
                    let was_read =
                        self.scan_done && self.unreadable.as_ref().is_none_or(|u| u.partial);
                    vanished |= refreshed
                        && was_read
                        && !failure.partial
                        && matches!(failure.reason, Reason::Missing | Reason::NotFolder);
                    self.unreadable = Some(failure);
                    self.scan_done = true;
                    self.pending_focus = None;
                    failed = true;
                    changed = true;
                }
            }
        }
        if changed {
            match held {
                Some(held) => self.reproject_rows_carrying(&held),
                None => self.reproject_rows(),
            }
            self.dirty = true;
        }
        // After a new order swaps in, the cursor's entry is somewhere else.
        if reveal {
            self.reveal_lead();
        }
        if finished {
            // The gallery only adopts a scan of the folder it is showing: a scan of
            // somewhere else fills the listing without disturbing the pictures.
            if self.current_dir.as_deref() == Some(self.scan_dir.as_path()) {
                self.adopt_scanned_images();
            }
            self.note_collection();
            self.apply_pending_focus();
        }
        // A folder read, or found not there, is the moment the places are
        // most likely to have changed: arrived at after a while away, or
        // re-read because something in it went, which may be a pinned folder.
        // Asking costs a stat a place, on the usage worker.
        if finished || failed {
            self.check_places();
        }
        if failed {
            if vanished {
                self.climb_out();
            } else {
                self.tell_incomplete();
            }
        }
        if changed {
            self.settle_auto_shape();
        }
    }

    /// Leave a folder that went away while it was on screen, deleted, moved,
    /// or put out of reach by its drive going, for the nearest folder above it
    /// that is still there, with the cursor where it was, and say so. It is
    /// not a step in the history: the user did not choose it, and back from
    /// there is back from where they were.
    ///
    /// In the viewer the picture stays up, as it is already decoded; the
    /// explorer under it shows the folder as gone, and Up climbs from there.
    fn climb_out(&mut self) {
        let gone = self.scan_dir.clone();
        let what = match self.unreadable.as_ref().map(|u| u.reason) {
            Some(Reason::NotFolder) => "was replaced by a file",
            _ => "was moved or deleted",
        };
        let name = files::file_name(&gone);
        let nearest = self.unreadable.as_ref().and_then(|u| u.nearest.clone());
        let Some(nearest) = nearest.filter(|_| self.mode.is_explorer()) else {
            self.notify(Level::Warning, format!("“{name}” {what}"));
            return;
        };
        let landing = files::child_toward(&nearest, &gone);
        self.open_dir(
            nearest.clone(),
            landing.map_or(PendingFocus::First, PendingFocus::Path),
        );
        self.notify(
            Level::Warning,
            format!("“{name}” {what} — showing “{}”", files::file_name(&nearest)),
        );
    }

    /// Say that the listing on screen is not the whole folder, when it broke
    /// off part way. A listing that broke off before it began has the
    /// content pane to say so in instead.
    fn tell_incomplete(&mut self) {
        let incomplete = self.unreadable.as_ref().is_some_and(|u| u.partial);
        if incomplete && !self.listing.is_empty() {
            let name = files::file_name(&self.scan_dir);
            self.notify(
                Level::Warning,
                format!("“{name}” could not be read to the end — some entries are missing"),
            );
        }
    }

    /// Read the folder again. One that was read is re-read aside and swapped
    /// in, keeping the place in it, as a change on disk does; one that could
    /// not be read at all is tried afresh, as if just opened.
    pub(super) fn reload(&mut self) {
        if self.unreadable.is_some() && self.listing.is_empty() {
            let dir = self.scan_dir.clone();
            self.pending_focus = Some(PendingFocus::First);
            self.request_scan(dir);
        } else {
            self.refresh_listing();
        }
        self.dirty = true;
    }

    /// Replace the listing with a re-read of the same directory.
    ///
    /// The decoded images are kept across it, keyed by path, so a refresh does not
    /// blank the thumbnails. That is only right for a file that has not changed:
    /// one whose size or modification time moved, or that is gone, has its images
    /// dropped here so they are decoded again from what is on disk now.
    pub(super) fn adopt_refresh(&mut self, staged: Vec<FsEntry>) {
        let now: HashMap<&Path, (u64, i64)> = staged
            .iter()
            .map(|entry| (entry.path.as_path(), (entry.size, entry.mtime_secs)))
            .collect();
        let changed: HashSet<&Path> = self
            .listing
            .iter()
            .filter(|entry| now.get(entry.path.as_path()) != Some(&(entry.size, entry.mtime_secs)))
            .map(|entry| entry.path.as_path())
            .collect();
        if !changed.is_empty() {
            self.store
                .retain(|key| !changed.contains(key.path.as_path()));
            self.undrawn
                .retain(|key| !changed.contains(key.path.as_path()));
        }
        self.listing = staged;
    }

    /// Put the cursor where whoever asked for this directory wanted it.
    ///
    /// Runs once the scan has finished, because until then the row being asked for
    /// may not have been read yet. A request that names a folder no longer in the
    /// directory puts the cursor where that folder was; anything else that is
    /// gone leaves it at the top, which is where a freshly opened folder starts
    /// anyway.
    pub(super) fn apply_pending_focus(&mut self) {
        let row = match self.pending_focus.take() {
            Some(PendingFocus::Index(i)) => {
                self.index = i.min(self.images.len().saturating_sub(1));
                self.images
                    .get(self.index)
                    .cloned()
                    .and_then(|path| self.row_of(&path))
            }
            Some(PendingFocus::Path(path)) => self.row_of(&path).or_else(|| self.slot_of(&path)),
            Some(PendingFocus::First) => None,
            None => return,
        };
        if let Some(row) = row {
            self.sel.click(row);
        } else {
            self.sel.click(0);
        }
        self.reveal_lead();
    }

    /// The row a folder `path` in the directory shown would be on, now that
    /// it is not: the one that took its place.
    fn slot_of(&self, path: &Path) -> Option<usize> {
        if path.parent() != Some(self.scan_dir.as_path()) || self.rows.is_empty() {
            return None;
        }
        let name = files::file_name(path);
        let entries = self.rows.iter().filter_map(|&i| self.listing.get(i));
        let slot = self.order.folder_slot(&name, entries)?;
        Some(slot.min(self.rows.len() - 1))
    }

    /// Adopt the images a finished scan turned up as the gallery's list.
    ///
    /// `index` and `shown` are positions in `images`, and the scan renumbers it, so
    /// each is re-found by the path it pointed at rather than carried across. An
    /// image the projection does not list — one opened by name with an extension
    /// the gallery does not scan for — is kept, because the alternative is taking
    /// the picture out from under the viewer showing it.
    ///
    /// Nothing is evicted and no generation is bumped: the store is keyed by path,
    /// so every decode already made is still the right picture under the right key.
    pub(super) fn adopt_scanned_images(&mut self) {
        let at = |i: usize| self.images.get(i).cloned();
        let (was_index, was_shown) = (at(self.index), self.shown.and_then(at));

        let mut images = files::images_of(&self.listing, |e| self.shows(e));
        for anchor in [was_index.as_ref(), was_shown.as_ref()]
            .into_iter()
            .flatten()
        {
            files::keep_image(&mut images, anchor);
        }
        let find = |want: &Option<PathBuf>| {
            want.as_ref()
                .and_then(|w| images.iter().position(|p| p == w))
        };
        let (index, shown) = (find(&was_index), find(&was_shown));
        self.images = images;
        self.index = index.unwrap_or(0);
        self.shown = shown;
        self.refresh_title();
        self.dirty = true;

        // The image on screen is resident already, so this is a hit that only
        // re-points the viewer; the neighbours it can now see get preloaded.
        if self.mode == ViewMode::Viewer {
            self.show_target();
            self.preload();
        }
    }
}
