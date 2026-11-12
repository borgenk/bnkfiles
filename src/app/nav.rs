//! Navigation: moving between pictures, folders, places and view modes,
//! and keeping the cursor row in view.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::app::{
    read_head, Focus, PendingFocus, State, ViewMode, FONT_BROWSE, FONT_LABEL, FONT_STATUS,
};
use crate::explorer::{self, Segment};
use crate::files::{self, FsEntry};
use crate::header::Control;
use crate::keymap::Command;
use crate::notice::Level;
use crate::order::Order;
use crate::places::PlaceKind;
use crate::platform::geom::Rect;
use crate::prompt::TextField;
use crate::render::display::DrawCmd;
use crate::render::image::ImageKey;
use crate::select::Selection;
use crate::settings::Settings;
use crate::unreadable::{self, Action, Style};
use crate::{mime, open, places, sidebar, usage};

/// The directory whose presence makes a folder a collection.
const COLLECTION_MARKER: &str = ".bnkfiles";

impl State {
    /// Step to another image, wrapping around.
    pub(super) fn navigate(&mut self, delta: i32) {
        if self.images.is_empty() {
            return;
        }
        if delta != 0 {
            self.nav_dir = delta.signum();
        }
        let n = self.images.len() as i32;
        self.index = ((((self.index as i32 + delta) % n) + n) % n) as usize;
        self.show_target();
        self.preload();
        // The title tracks the position even while the image it names is still
        // decoding, so it never pairs an old name with a new count.
        self.refresh_title();
    }

    // ---- the views -------------------------------------------------------

    /// Switch the active view. Any pan drag belongs to the view it began in, so it
    /// ends here: otherwise a still-held button pans the hidden viewer, and forces
    /// a redraw, on every motion event in the new view.
    pub(super) fn set_mode(&mut self, mode: ViewMode) {
        self.mode = mode;
        self.dragging = false;
        // The header holds other buttons now, and the pointer may be over one.
        self.update_hover();
        self.dirty = true;
    }

    pub fn enter_viewer(&mut self) {
        self.set_mode(ViewMode::Viewer);
        // Nothing has been presented yet (an image named on the command line):
        // decode inline, so the first frame carries the image rather than flashing
        // an empty window and repainting a moment later.
        if !self.configured {
            self.load_now(self.index);
        }
        self.show_target();
        self.preload();
        self.refresh_title();
    }

    /// Leave the viewer for the explorer layout it was entered from, with the
    /// cursor on the picture that was open. Coming back to the top of the directory
    /// after looking at one photograph in the middle of it is disorienting.
    pub(super) fn leave_viewer(&mut self) {
        let showing = self.images.get(self.index).cloned();
        self.set_mode(self.viewer_return);
        if let Some(row) = showing.as_deref().and_then(|p| self.row_of(p)) {
            self.sel.click(row);
        }
        self.reopen = showing;
        self.reveal_lead();
        self.refresh_title();
    }

    /// Swap the viewer between bare (the picture and the status bar) and framed
    /// (the sidebar and a header around it). The zoom carries over: a fit stays
    /// a fit to the new canvas, and a zoomed picture stays where it was on
    /// screen. The pan is canvas-local, so it moves by however far the canvas's
    /// origin did, then clamps to the new canvas's edges.
    pub(super) fn toggle_viewer_frame(&mut self) {
        let before = self.canvas();
        self.viewer_framed = !self.viewer_framed;
        if let (Some(before), Some((w, h, iw, ih))) = (before, self.view_dims()) {
            if let Some(after) = self.canvas() {
                let (dx, dy) = ((before.x - after.x) as f32, (before.y - after.y) as f32);
                self.viewer.pan_by(dx, dy, w, h, iw, ih);
            }
        }
        self.update_hover();
        self.dirty = true;
    }

    /// Show or hide the inspector. The keyboard cannot stay in a panel that has
    /// gone, so it goes back to the entries.
    pub(super) fn toggle_inspector(&mut self) {
        self.show_inspector = !self.show_inspector;
        if !self.show_inspector && self.focus == Focus::Inspector {
            self.focus = Focus::Entries;
        }
        self.update_hover();
        self.dirty = true;
    }

    /// The explorer row the inspector describes on its own, if it describes one:
    /// the one selected entry, or in the viewer the picture navigated to.
    pub(super) fn inspected_row(&self) -> Option<usize> {
        match self.mode {
            ViewMode::Viewer => self.row_of(self.images.get(self.index)?),
            ViewMode::List | ViewMode::Grid => match self.sel.count() {
                1 => self.sel.iter().next(),
                _ => None,
            },
        }
    }

    /// Swap the explorer between the detail list and the thumbnail grid. The
    /// selection is one model over one set of rows, so nothing is lost either way;
    /// only the scroll is per-layout, since a row offset means nothing to a grid.
    pub(super) fn toggle_layout(&mut self) {
        let next = match self.mode {
            ViewMode::Grid => ViewMode::List,
            _ => ViewMode::Grid,
        };
        self.viewer_return = next;
        self.set_mode(next);
        self.reveal_lead();
    }

    /// Show or hide dot-files. A re-projection of the listing already read, not a
    /// rescan: the listing holds dot-files either way.
    pub(super) fn toggle_hidden(&mut self) {
        let held = self.held_rows();
        self.show_hidden = !self.show_hidden;
        self.reproject_rows_carrying(&held);
        self.reveal_lead();
        self.dirty = true;
    }

    // ---- the explorer ----------------------------------------------------

    /// List the folder in `order`. The scan worker owns the sorting, so this
    /// reads the folder again in the new order, aside, and swaps it in whole
    /// as a refresh does: the old order stays on screen meanwhile, the
    /// selection rides across on its entries, and a large folder is never
    /// sorted on the event loop.
    pub(super) fn set_order(&mut self, order: Order) {
        if order == self.order {
            return;
        }
        self.order = order;
        self.reveal_on_adopt = true;
        self.refresh_listing();
        self.dirty = true;
    }

    /// Put the keyboard in the header's filter field. A header too narrow to
    /// draw it still takes the text; the status bar shows it then.
    pub(super) fn focus_filter(&mut self) {
        self.focus = Focus::Filter;
        self.dirty = true;
    }

    /// Edit the filter's text and narrow the rows to what it now matches.
    ///
    /// The selection rides along on its entries, as across a rescan. The view
    /// goes back to the top, where the first match is, unless the cursor is on
    /// an entry that still matches: then it stays in view.
    pub(super) fn edit_filter(&mut self, edit: impl FnOnce(&mut TextField)) {
        let changed = self.filter.edit(edit);
        self.dirty = true;
        if changed {
            self.refilter();
        }
    }

    /// Empty the filter field, putting every row back. Says whether there was
    /// a filter to empty.
    pub(super) fn clear_filter(&mut self) -> bool {
        let cleared = self.filter.clear();
        if cleared {
            self.refilter();
        }
        cleared
    }

    /// Narrow the rows to what the filter now lets by.
    ///
    /// Not [`Self::reproject_rows_carrying`]: that one is for a listing that
    /// changed under the selection, where a selected row that went away hands
    /// its place to the next one. A filter hides rows, and a file the user
    /// never picked must not become the selection because it slid into the
    /// place of one they did. So a cursor whose entry is hidden starts over
    /// at the top with nothing selected, unless other selected entries are
    /// still showing.
    fn refilter(&mut self) {
        // The listing does not change, so the marked rows are carried by their
        // listing index. The rows are listing indices in ascending order, so
        // finding one again is a binary search, not a hash of every path.
        let marked: Vec<(usize, usize)> = self
            .sel
            .marked()
            .filter_map(|row| Some((row, *self.rows.get(row)?)))
            .collect();
        let lead = self.sel.lead().and_then(|row| self.rows.get(row).copied());
        self.project_rows();
        let rows = &self.rows;
        let now: HashMap<usize, usize> = marked
            .iter()
            .filter_map(|&(old, i)| Some((old, rows.binary_search(&i).ok()?)))
            .collect();
        let lead_stays = lead.is_some_and(|i| rows.binary_search(&i).is_ok());
        let selection_stays = self.sel.iter().any(|row| now.contains_key(&row));
        let len = self.rows.len();
        if lead_stays || selection_stays {
            self.sel.remap(len, |old| now.get(&old).copied());
        } else {
            self.sel = Selection::new();
            self.sel.set_len(len);
        }
        self.list_scroll = 0.0;
        self.grid_scroll = 0.0;
        if lead_stays {
            self.reveal_lead();
        }
        self.dirty = true;
    }

    /// Whether the explorer shows `entry`: not a dot-file unless those are
    /// shown, and a name the filter lets by. The gallery asks the same.
    pub(super) fn shows(&self, entry: &FsEntry) -> bool {
        (self.show_hidden || !entry.hidden) && self.filter.matches(&entry.name)
    }

    /// Re-project the rows from the listing, and count what the filter left
    /// out: the rows are the listing entries [`Self::shows`] lets by, by index.
    fn project_rows(&mut self) {
        let show_hidden = self.show_hidden;
        let mut total = 0;
        let mut rows = Vec::new();
        for (i, entry) in self.listing.iter().enumerate() {
            if !show_hidden && entry.hidden {
                continue;
            }
            total += 1;
            if self.filter.matches(&entry.name) {
                rows.push(i);
            }
        }
        self.rows = rows;
        self.rows_rev = self.rows_rev.wrapping_add(1);
        self.rows_total = total;
    }

    /// Rebuild the rows the explorer shows from the listing, leaving the selection
    /// on the positions it had. For a folder streaming in, where there is no
    /// selection yet to keep.
    pub(super) fn reproject_rows(&mut self) {
        self.project_rows();
        self.sel.set_len(self.rows.len());
    }

    /// The entries the selection and its cursors are on, each with its row, taken
    /// before the rows change so [`Self::reproject_rows_carrying`] can find them
    /// again afterwards.
    pub(super) fn held_rows(&self) -> HashMap<PathBuf, usize> {
        self.sel
            .marked()
            .filter_map(|row| Some((self.row_entry(row)?.path.clone(), row)))
            .collect()
    }

    /// Rebuild the rows and move the selection to wherever its entries are now.
    /// A selection is rows, and rows are positions: without this a rescan or a
    /// hidden toggle would leave it on whatever slid into its place.
    pub(super) fn reproject_rows_carrying(&mut self, held: &HashMap<PathBuf, usize>) {
        self.project_rows();
        let now: HashMap<usize, usize> = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(row, &i)| Some((*held.get(&self.listing.get(i)?.path)?, row)))
            .collect();
        self.sel
            .remap(self.rows.len(), |old| now.get(&old).copied());
    }

    /// The listing entry row `row` shows.
    pub(super) fn row_entry(&self, row: usize) -> Option<&FsEntry> {
        self.listing.get(*self.rows.get(row)?)
    }

    /// An image as a drawing command, or nothing if the store does not hold it.
    pub(super) fn image_cmd(&self, rect: Rect, key: ImageKey) -> Option<DrawCmd> {
        let generation = self.store.generation(&key)?;
        Some(DrawCmd::Image {
            rect,
            key,
            generation,
        })
    }

    /// The entry the keyboard is on.
    pub(super) fn lead_entry(&self) -> Option<&FsEntry> {
        self.row_entry(self.sel.lead()?)
    }

    /// How much the selection adds up to. Directories count as nothing: their own
    /// size is their inode's, which says nothing about what is inside them, and
    /// walking the tree to find out is not something a status bar may do.
    pub(super) fn selected_bytes(&self) -> u64 {
        self.sel
            .iter()
            .filter_map(|row| self.row_entry(row))
            .filter(|e| !e.is_dir())
            .map(|e| e.size)
            .sum()
    }

    /// Which row shows `path`, if any is showing it.
    pub(super) fn row_of(&self, path: &Path) -> Option<usize> {
        self.rows
            .iter()
            .position(|&i| self.listing.get(i).is_some_and(|e| e.path == path))
    }

    /// Open the directory `dir`, putting the cursor at `focus` once it has been read.
    pub(super) fn open_dir(&mut self, dir: PathBuf, focus: PendingFocus) {
        self.reopen = None;
        self.current_dir = Some(dir.clone());
        self.open_images(Vec::new(), 0);
        self.pending_focus = Some(focus);
        self.request_scan(dir);
    }

    /// Go into `dir`, landing on its first entry. Every navigation the user
    /// chooses comes through here, so this is where the history records it.
    pub(super) fn enter_dir(&mut self, dir: PathBuf) {
        self.history.visit(&self.scan_dir, &dir);
        self.open_dir(dir, PendingFocus::First);
        self.focus = Focus::Entries;
    }

    /// Step back through the history, landing on the folder just left if it is
    /// in the one returned to (as it is when back undoes a step down).
    pub(super) fn go_back(&mut self) {
        // In a picture, the step back is to the folder it was opened from.
        if self.mode == ViewMode::Viewer {
            self.leave_viewer();
            self.focus = Focus::Entries;
            return;
        }
        let leaving = self.scan_dir.clone();
        if let Some(to) = self.history.back(&leaving) {
            self.open_dir(to, PendingFocus::Path(leaving));
            self.focus = Focus::Entries;
        }
    }

    /// Step forward through the history: the mirror of [`Self::go_back`].
    pub(super) fn go_forward(&mut self) {
        // The latest step back was out of a picture: forward is into it again.
        if let Some(path) = self.reopen.take() {
            if let Some(row) = self.row_of(&path) {
                self.sel.click(row);
                self.activate_lead();
                return;
            }
        }
        let leaving = self.scan_dir.clone();
        if let Some(to) = self.history.forward(&leaving) {
            self.open_dir(to, PendingFocus::Path(leaving));
            self.focus = Focus::Entries;
        }
    }

    /// Whether a header button would do anything now. A button that would not
    /// is drawn dimmed, and a click on it is ignored.
    pub(super) fn control_enabled(&self, control: Control) -> bool {
        match control {
            Control::Back => {
                self.mode == ViewMode::Viewer || self.history.can_go_back(&self.scan_dir)
            }
            // Nothing is ahead of a picture; in the explorer, the picture just
            // left is, or else the history.
            Control::Forward => {
                self.mode.is_explorer()
                    && (self.reopen.is_some() || self.history.can_go_forward(&self.scan_dir))
            }
            Control::Up => self.up_target().is_some(),
            Control::Previous | Control::Next => self.images.len() > 1,
            Control::Layout
            | Control::Inspector
            | Control::Close
            | Control::Actual
            | Control::Fit => true,
        }
    }

    /// Do what header button `control` stands for.
    pub(super) fn press_control(&mut self, control: Control) {
        if !self.control_enabled(control) {
            return;
        }
        match control {
            Control::Back => self.go_back(),
            Control::Forward => self.go_forward(),
            Control::Up => self.go_parent(),
            Control::Layout => self.toggle_layout(),
            Control::Inspector => self.toggle_inspector(),
            Control::Close => self.leave_viewer(),
            Control::Actual => self.view_actual(),
            Control::Fit => self.view_fit(),
            Control::Previous => self.navigate(-1),
            Control::Next => self.navigate(1),
        }
        self.dirty = true;
    }

    /// Where Up goes: the parent, or from a folder that is not there, the
    /// nearest folder above it that is, rather than a parent that may be gone
    /// as well.
    pub(super) fn up_target(&self) -> Option<PathBuf> {
        self.unreadable
            .as_ref()
            .and_then(|u| u.nearest.clone())
            .or_else(|| self.scan_dir.parent().map(Path::to_path_buf))
    }

    /// Go up, landing the cursor on the directory just left (or where it was,
    /// if it is gone), so climbing out of a tree and stepping back into it is
    /// symmetric.
    pub(super) fn go_parent(&mut self) {
        let Some(parent) = self.up_target() else {
            return;
        };
        let landing = files::child_toward(&parent, &self.scan_dir);
        self.enter_dir(parent);
        self.pending_focus = Some(landing.map_or(PendingFocus::First, PendingFocus::Path));
    }

    /// Open what the cursor is on: descend into a directory, or open an image in
    /// the viewer with the folder as its gallery.
    ///
    /// Nothing re-reads the directory; the listing the rows were drawn from is the
    /// listing the gallery takes its images off. Anything else is handed to the
    /// application the desktop registers for it.
    pub(super) fn activate_lead(&mut self) {
        // Opening is not a movement: a held key that got here stops repeating, or
        // it would descend a level, or launch an application, at the repeat rate.
        self.stop_repeat();
        let Some(entry) = self.lead_entry() else {
            return;
        };
        let (is_dir, path) = (entry.is_dir(), entry.path.clone());
        if is_dir {
            self.enter_dir(path);
            return;
        }
        if !files::is_image(&path) {
            self.open_externally(&path);
            return;
        }
        let images = files::images_of(&self.listing, |e| self.shows(e));
        let start = images.iter().position(|p| p == &path).unwrap_or(0);
        self.current_dir = Some(self.scan_dir.clone());
        self.viewer_return = self.mode;
        // Opened from the explorer, the picture takes the content column and the
        // sidebar stays where it was.
        self.viewer_framed = true;
        self.open_images(images, start);
        self.enter_viewer();
    }

    /// Hand a file that is not a picture to whatever the desktop says opens it.
    ///
    /// The file's first bytes are read here, on the main thread: one small read on
    /// an explicit key press, never on the frame path or in a listing.
    pub(super) fn open_externally(&mut self, path: &Path) {
        let head = read_head(path);
        let extension = path.extension().and_then(|e| e.to_str());
        let kind = mime::sniff(&head, extension);
        match open::open_path(path, kind) {
            Ok(()) => self.notify(Level::Info, format!("opening {}", files::file_name(path))),
            // The application could not be resolved or could not be started. The
            // explorer carries on; the user gets told why on the notification
            // line rather than in a terminal they may not be looking at.
            Err(e) => self.notify(Level::Error, format!("{}: {e}", files::file_name(path))),
        }
    }

    /// Move the cursor by `delta` rows, optionally extending the selection, and
    /// scroll it into view.
    pub(super) fn move_lead(&mut self, delta: i32, extending: bool) {
        self.sel.move_lead(delta, extending);
        self.reveal_lead();
    }

    /// Move the cursor `rows` rows down (or up, negative): in the grid, to
    /// the cell above or below, which in masonry is the next down its own
    /// column rather than a fixed number of cells along.
    pub(super) fn move_rows(&mut self, rows: i32, extending: bool) {
        let delta = match self.mode {
            ViewMode::Grid => {
                let lead = self.sel.lead().unwrap_or(0);
                let to = self.grid_layout().vertical(self.rows.len(), lead, rows);
                to as i32 - lead as i32
            }
            _ => rows,
        };
        self.move_lead(delta, extending);
    }

    /// Move the cursor one cell left (`dir` -1) or right (1) in the grid: the
    /// next along in squares, the one level with it in masonry.
    pub(super) fn move_across(&mut self, dir: i32, extending: bool) {
        let lead = self.sel.lead().unwrap_or(0);
        let to = self.grid_layout().horizontal(self.rows.len(), lead, dir);
        self.move_lead(to as i32 - lead as i32, extending);
    }

    /// How many rows a page-sized jump covers in the active layout: grid rows
    /// in the grid, entries in the list.
    pub(super) fn page_rows(&self) -> i32 {
        match self.mode {
            ViewMode::Grid => {
                let h = self.content_rect().map_or(0, |r| r.h);
                self.grid_layout().rows_per_page(h)
            }
            _ => {
                let layout = self.explorer_layout();
                (layout.content.h / layout.row.max(1)).max(1)
            }
        }
    }

    /// Scroll the cursor into view in whichever layout is showing.
    pub(super) fn reveal_lead(&mut self) {
        let Some(lead) = self.sel.lead() else { return };
        match self.mode {
            ViewMode::List => self.list_scroll = self.explorer_layout().reveal(lead),
            ViewMode::Grid => {
                let Some(content) = self.content_rect() else {
                    return;
                };
                self.grid_scroll = self.grid_layout().reveal(lead, content.h);
            }
            ViewMode::Viewer => return,
        }
        self.explorer_bar.flash(Instant::now());
        self.dirty = true;
    }

    /// Scroll the explorer by `dy` device pixels in whichever layout is showing.
    pub(super) fn scroll_explorer(&mut self, dy: f32) {
        match self.mode {
            ViewMode::List => {
                let layout = self.explorer_layout();
                self.list_scroll = (self.list_scroll + dy).clamp(0.0, layout.max_scroll);
            }
            ViewMode::Grid => {
                self.grid_scroll = self.grid_layout_at(self.grid_scroll + dy).scroll;
            }
            ViewMode::Viewer => return,
        }
        self.explorer_bar.flash(Instant::now());
        self.dirty = true;
    }

    // ---- the sidebar and the breadcrumb ------------------------------------

    /// Rebuild the sidebar: home, the root, the mounted volumes, the pinned
    /// folders. Called at start-up and whenever a pin changes, not per frame: this
    /// reads `/proc/self/mounts`, which is cheap but is still a read. How full
    /// each volume is goes to the usage worker, since that read can block.
    pub(super) fn refresh_places(&mut self) {
        self.mounts_text = places::read_mounts();
        self.rebuild_places();
        let asks = self.place_asks(|_| true);
        self.usage.request(asks);
    }

    /// Ask whether the pinned folders and the collections are still there:
    /// the places that go away. A mount that does leaves the mount table, and
    /// home and the root do not.
    pub(super) fn check_places(&mut self) {
        let asks = self.place_asks(|p| matches!(p.kind, PlaceKind::Pinned | PlaceKind::Collection));
        self.usage.request(asks);
    }

    /// The usage worker's questions about the places `which` picks. A path
    /// that is a mount is measured whichever row asks about it, so the answer
    /// for a collection on a mount does not take the mount's bar away.
    fn place_asks(&self, which: impl Fn(&places::Place) -> bool) -> Vec<usage::Ask> {
        let mount = |path: &Path| {
            self.places
                .iter()
                .any(|p| p.kind == PlaceKind::Mount && p.path == path)
        };
        self.places
            .iter()
            .filter(|p| which(p))
            .map(|p| usage::Ask {
                path: p.path.clone(),
                measure: mount(&p.path),
            })
            .collect()
    }

    /// Rebuild the places from what is already known, reading nothing. The
    /// keyboard stays on the row it was on, wherever that row now is: a pin
    /// adds rows above the collections, and the row under the keyboard must
    /// not quietly become another one.
    pub(super) fn rebuild_places(&mut self) {
        let was = match self.sidebar_rows().get(self.place_sel) {
            Some(&sidebar::Row::Place(i)) => self
                .places
                .get(i)
                .map(|p| (Some((p.path.clone(), p.kind)), None)),
            Some(&sidebar::Row::Header(section, _)) => Some((None, Some(section))),
            None => None,
        };
        let home = std::env::var_os("HOME").map(PathBuf::from);
        self.places = places::places(
            home.as_deref(),
            &self.mounts_text,
            &self.pinned,
            &self.collections,
        );
        let rows = self.sidebar_rows();
        let found = rows.iter().position(|row| match (row, &was) {
            (sidebar::Row::Place(i), Some((Some((path, kind)), _))) => self
                .places
                .get(*i)
                .is_some_and(|p| p.path == *path && p.kind == *kind),
            (sidebar::Row::Header(s, _), Some((_, Some(section)))) => s == section,
            _ => false,
        });
        self.place_sel = found.unwrap_or(self.place_sel.min(rows.len().saturating_sub(1)));
        self.dirty = true;
    }

    /// The sidebar's rows as they are now: section headers and places.
    pub(super) fn sidebar_rows(&self) -> Vec<sidebar::Row> {
        sidebar::rows(&self.places, &self.folded)
    }

    /// Fold or unfold `section`, keeping the keyboard on the row it was on,
    /// or on the header its place was folded into.
    pub(super) fn toggle_section(&mut self, section: sidebar::Section) {
        let before = self.sidebar_rows();
        sidebar::toggle(&mut self.folded, section);
        self.place_sel = sidebar::follow(&before, self.place_sel, &self.sidebar_rows());
        self.dirty = true;
    }

    /// Do what sidebar row `row` is for: fold a header, or go to a place.
    pub(super) fn activate_sidebar_row(&mut self, row: usize) {
        match self.sidebar_rows().get(row) {
            Some(&sidebar::Row::Header(section, _)) => self.toggle_section(section),
            Some(&sidebar::Row::Place(i)) => self.open_place(i),
            None => {}
        }
    }

    /// The sidebar row standing for the folder being shown, if one does.
    pub(super) fn sidebar_row_here(&self) -> Option<usize> {
        self.sidebar_rows().iter().position(|row| match row {
            sidebar::Row::Place(i) => self.places.get(*i).is_some_and(|p| p.path == self.scan_dir),
            sidebar::Row::Header(..) => false,
        })
    }

    /// Take in the usage worker's answers, if any have come.
    pub(super) fn handle_usage(&mut self) {
        // Most answers, after a folder is read, say what was known already;
        // only one that changes a row is worth a frame.
        for answer in self.usage.take() {
            let gone_changed = if answer.there {
                self.gone_places.remove(&answer.path)
            } else {
                self.gone_places.insert(answer.path.clone())
            };
            let fill_changed = match answer.fill {
                Some(fill) => self.fills.insert(answer.path, fill) != Some(fill),
                None => self.fills.remove(&answer.path).is_some(),
            };
            self.dirty |= gone_changed || fill_changed;
        }
    }

    /// Learn from a finished scan whether the folder is a collection: it is one
    /// while it has a `.bnkfiles/` directory in it. The listing is already in
    /// memory, so this reads nothing.
    pub(super) fn note_collection(&mut self) {
        let marked = self
            .listing
            .iter()
            .any(|e| e.name == COLLECTION_MARKER && e.is_dir());
        let known = self.collections.contains(&self.scan_dir);
        let dir = self.scan_dir.clone();
        match (marked, known) {
            (true, false) => {
                self.collections.push(dir.clone());
                self.collections_forgotten.retain(|c| *c != dir);
                self.collections_learned.push(dir);
            }
            (false, true) => {
                self.collections.retain(|c| *c != dir);
                self.collections_learned.retain(|c| *c != dir);
                self.collections_forgotten.push(dir);
            }
            _ => return,
        }
        self.rebuild_places();
    }

    /// Go to sidebar place `i`.
    pub(super) fn open_place(&mut self, i: usize) {
        if let Some(path) = self.places.get(i).map(|p| p.path.clone()) {
            self.enter_dir(path);
        }
    }

    /// Go to what path-bar segment `i` names: a prefix of the current directory.
    pub(super) fn open_path_segment(&mut self, i: usize) {
        if let Some(target) = self.path_segments().get(i).map(|s| s.target.clone()) {
            // Landing on the directory being left, so stepping back down the path
            // bar and then back up is symmetric with the parent key.
            let leaving = self.scan_dir.clone();
            self.enter_dir(target);
            self.pending_focus = Some(PendingFocus::Path(leaving));
        }
    }

    /// Pin the directory being shown to the sidebar, or unpin it if it is already
    /// there. The one way to change the sidebar, so it is its own toggle.
    pub(super) fn toggle_pin(&mut self) {
        let dir = self.scan_dir.clone();
        let toggle = |pinned: &mut Vec<PathBuf>| match pinned.iter().position(|p| p == &dir) {
            Some(at) => {
                pinned.remove(at);
            }
            None => pinned.push(dir.clone()),
        };
        // Saved now rather than on the way out, so a pin survives the window being
        // killed, and made to the file's own list, so it lands beside whatever
        // another window has pinned since this one started.
        match Settings::update(|settings| toggle(&mut settings.pinned)) {
            Ok(settings) => self.pinned = settings.pinned,
            Err(e) => {
                toggle(&mut self.pinned);
                self.notify(Level::Error, format!("could not save the pin: {e}"));
            }
        }
        self.refresh_places();
        self.dirty = true;
    }

    /// The breadcrumb's segments, measured against the fonts as they are now.
    ///
    /// Recomputed rather than cached: it is a handful of text measurements, and a
    /// cache would be one more thing to invalidate when the directory, the window
    /// width, or the display scale changes.
    pub(super) fn path_segments(&self) -> Vec<Segment> {
        let middle = self.header_layout().middle;
        let size = self.font_px(FONT_STATUS);
        explorer::path_segments(&self.scan_dir, middle, self.scale(), &|text| {
            self.text_width(size, text)
        })
    }

    /// The panel the content pane shows in place of a folder that could not
    /// be read at all, if that is what it shows: what went wrong, and what
    /// can be done about it, each with its key.
    pub(super) fn unreadable_panel(&self) -> Option<unreadable::Panel> {
        if !self.mode.is_explorer() || !self.listing.is_empty() {
            return None;
        }
        let failure = self.unreadable.as_ref()?;
        let up_to = self.up_target();
        let mut actions = Vec::new();
        if up_to.is_some() {
            actions.push((Action::Up, Command::Up.keys()));
        }
        if self.control_enabled(Control::Back) {
            actions.push((Action::Back, Command::Back.keys()));
        }
        actions.push((Action::Retry, Command::Reload.keys()));
        let size = |style: Style| {
            self.font_px(match style {
                Style::Headline => FONT_BROWSE,
                Style::Text => FONT_STATUS,
                Style::Small => FONT_LABEL,
            })
        };
        let line_height = |style: Style| self.fonts.metrics(size(style)).line_height;
        let measure = |style: Style, text: &str| self.text_width(size(style), text);
        Some(unreadable::panel(
            self.explorer_layout().content,
            self.scale(),
            &unreadable::Contents {
                dir: &self.scan_dir,
                failure,
                actions: &actions,
                up_to: up_to.as_deref(),
            },
            &unreadable::Fonts {
                line_height: &line_height,
                measure: &measure,
            },
        ))
    }
}
