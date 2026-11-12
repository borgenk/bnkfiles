//! The thumbnail grid's shape: square cells, or masonry columns that show every
//! picture at its own height (see [`crate::masonry`]).
//!
//! A folder's shape is the one chosen for it or for its nearest ancestor, and
//! `auto` where none was. Auto looks at the shapes of the folder's first
//! pictures, as their thumbnails come in, and settles once a visit: masonry
//! when they differ enough in shape, squares when they do not.

use std::ffi::OsStr;
use std::path::Path;
use std::rc::Rc;

use crate::app::{MasonryFor, State, ViewMode};
use crate::files;
use crate::masonry::{self, Masonry, Shape};
use crate::notice::Level;

/// How many of a folder's first pictures auto judges it by, and how far into
/// the rows it looks for them. Folders come first, so the look goes past them.
/// Few enough that the first screen of even a narrow window holds them, since
/// only what is on screen, and a screen below, is decoded.
const SAMPLE_PICTURES: usize = 12;
const SAMPLE_ROWS: usize = 256;

/// The shape a picture is laid out at before its thumbnail says: 4:3, the
/// commonest shape of a photograph, so most of them stay put when theirs lands.
const UNKNOWN_SHAPE: f32 = 4.0 / 3.0;

impl State {
    /// The shape chosen for the folder on show: its own, or else its nearest
    /// ancestor's, or else auto. Looked up when the folder or a choice
    /// changes, as it is asked for every thumbnail drawn.
    pub(super) fn folder_shape(&self) -> Shape {
        self.shape_here
    }

    /// Look the folder's shape up again: after going somewhere else, or a
    /// choice.
    pub(super) fn refresh_shape_here(&mut self) {
        self.shape_here = self
            .shapes
            .iter()
            .filter(|(dir, _)| self.scan_dir.starts_with(dir))
            .max_by_key(|(dir, _)| dir.components().count())
            .map_or(Shape::Auto, |(_, shape)| *shape);
    }

    /// Whether the grid is in masonry now.
    pub(super) fn masonry_now(&self) -> bool {
        match self.folder_shape() {
            Shape::Square => false,
            Shape::Masonry => true,
            Shape::Auto => self.auto_masonry == Some(true),
        }
    }

    /// The masonry for a content `width`: the one laid out last if nothing it
    /// was laid out for has changed, or laid out again.
    pub(super) fn masonry_columns(&self, width: i32) -> Rc<Masonry> {
        let m = self.grid_metrics().masonry();
        let made_for = MasonryFor {
            width,
            column: m.column,
            label: m.label,
            gap: m.gap,
            pad: m.pad,
            rows_rev: self.rows_rev,
            aspects_rev: self.aspects_rev,
        };
        if let Ok(memo) = self.masonry.try_borrow() {
            if let Some((made, rows)) = memo.as_ref() {
                if *made == made_for {
                    return Rc::clone(rows);
                }
            }
        }
        let ratios: Vec<f32> = (0..self.rows.len()).map(|r| self.row_shape(r)).collect();
        let columns = Rc::new(Masonry::build(&ratios, width, m));
        if let Ok(mut memo) = self.masonry.try_borrow_mut() {
            *memo = Some((made_for, Rc::clone(&columns)));
        }
        columns
    }

    /// The shape, width over height, row `row` is laid out at. Whatever is
    /// not a picture is a square, with its icon in it.
    fn row_shape(&self, row: usize) -> f32 {
        let Some(entry) = self.row_entry(row) else {
            return 1.0;
        };
        if entry.is_dir() || !files::is_image(&entry.path) {
            return 1.0;
        }
        entry
            .path
            .file_name()
            .and_then(|name| self.aspects.get(name))
            .copied()
            .unwrap_or(UNKNOWN_SHAPE)
    }

    /// The thumbnail edge masonry decodes to: half again a square cell's, so
    /// a picture filling a column a little wider than a cell, portrait ones
    /// especially, is not blown up from a thumbnail made for a square.
    pub(super) fn masonry_edge(&self) -> i32 {
        self.grid_metrics().thumb * 3 / 2
    }

    /// Remember the shape of the picture at `path`, which a thumbnail
    /// `width` x `height` has just shown.
    pub(super) fn note_shape(&mut self, path: &Path, width: u32, height: u32) {
        let Some(name) = path.file_name() else {
            return;
        };
        if width == 0 || height == 0 {
            return;
        }
        let shape = width as f32 / height as f32;
        // A thumbnail is rounded to whole pixels; a shape within that of the
        // one known is the same shape, and laying the rows out again for it
        // would only jiggle them.
        let known = self.aspects.get(name).copied();
        if known.is_some_and(|k| (k - shape).abs() < 0.02) {
            return;
        }
        self.aspects.insert(OsStr::to_os_string(name), shape);
        self.aspects_rev = self.aspects_rev.wrapping_add(1);
    }

    /// The cell to hold still while the grid is laid out again, and where
    /// its top is on screen: the cursor's when it is on screen, else the one
    /// in the middle. None at the very top, which stays the top: a folder
    /// just opened keeps its first row in view however its rows reflow.
    pub(super) fn grid_anchor(&self) -> Option<(usize, f32)> {
        if self.mode != ViewMode::Grid {
            return None;
        }
        let content = self.content_rect()?;
        let layout = self.grid_layout();
        if layout.scroll < 0.5 {
            return None;
        }
        let h = content.h as f32;
        let on_screen = |i: usize| layout.tile(i).filter(|t| t.y >= 0.0 && t.y + t.h <= h);
        let lead = self.sel.lead().and_then(|i| Some((i, on_screen(i)?.y)));
        lead.or_else(|| {
            let i = layout.centre(self.rows.len(), content.h);
            Some((i, layout.tile(i)?.y))
        })
    }

    /// Scroll so the cell `grid_anchor` named is where it was on screen.
    pub(super) fn hold_anchor(&mut self, (i, y): (usize, f32)) {
        let Some(now) = self.grid_layout().tile(i) else {
            return;
        };
        if (now.y - y).abs() >= 0.5 {
            self.grid_scroll = self.grid_layout_at(self.grid_scroll + now.y - y).scroll;
            self.dirty = true;
        }
    }

    /// Settle what auto means for this folder, once its first pictures'
    /// shapes are known well enough to say. A folder that is mostly not
    /// pictures is squares.
    pub(super) fn settle_auto_shape(&mut self) {
        if self.auto_masonry.is_some() || self.folder_shape() != Shape::Auto {
            return;
        }
        let (mut files_seen, mut pictures) = (0, Vec::new());
        for row in 0..self.rows.len().min(SAMPLE_ROWS) {
            let Some(entry) = self.row_entry(row) else {
                continue;
            };
            if entry.is_dir() {
                continue;
            }
            files_seen += 1;
            if files::is_image(&entry.path) {
                pictures.push(entry.path.file_name().map(OsStr::to_os_string));
                if pictures.len() == SAMPLE_PICTURES {
                    break;
                }
            }
        }
        // More of the folder may be on its way, and it might be pictures.
        if pictures.len() < SAMPLE_PICTURES && !self.scan_done {
            return;
        }
        // Whatever it settles on, the view stays where the user has it.
        let anchor = self.grid_anchor();
        let masonry = if pictures.len() < 2 || pictures.len() * 2 < files_seen {
            false
        } else {
            let known: Vec<f32> = pictures
                .iter()
                .filter_map(|name| self.aspects.get(name.as_deref()?).copied())
                .collect();
            // Wait for most of them: a few thumbnails decode slowly, and one
            // that cannot be decoded at all must not keep the folder waiting.
            if known.len() * 4 < pictures.len() * 3 {
                return;
            }
            masonry::prefers_masonry(&known)
        };
        self.auto_masonry = Some(masonry);
        if let Some(anchor) = anchor {
            self.hold_anchor(anchor);
        }
        self.dirty = true;
    }

    /// Step the folder on show to the next shape: auto, square, masonry.
    pub(super) fn cycle_shape(&mut self) {
        let dir = self.scan_dir.clone();
        let shape = self.folder_shape().next();
        for list in [&mut self.shapes, &mut self.shapes_chosen] {
            list.retain(|(d, _)| *d != dir);
            list.push((dir.clone(), shape));
        }
        self.refresh_shape_here();
        if shape == Shape::Auto {
            self.auto_masonry = None;
            self.settle_auto_shape();
        }
        let now = if self.masonry_now() {
            Shape::Masonry
        } else {
            Shape::Square
        };
        self.notify(
            Level::Info,
            match shape {
                Shape::Auto => format!("thumbnails: auto, {} here", now.key()),
                _ => format!("thumbnails: {}", shape.key()),
            },
        );
        self.reveal_lead();
        self.dirty = true;
    }
}
