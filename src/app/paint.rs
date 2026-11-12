//! Building the frame: the view's state as a list of drawing commands.
//!
//! Nothing here mutates the world beyond asking for the thumbnails the grid is
//! about to need. The list is a pure function of the state, which is what lets the
//! damage diff say "nothing changed" and skip a present entirely.
//!
//! An image is one quad. The GPU samples the texture across it, so a zoom or a
//! resize costs no CPU pixels at all, and it is the same work whether the picture is
//! shown at 5% or 500%.

use crate::app::{Focus, State, ViewMode, FONT_BROWSE, FONT_LABEL, FONT_STATUS};
use crate::explorer;
use crate::files::{self, FsEntry};
use crate::filter;
use crate::header::{Control, HeaderLayout};
use crate::inspector::{self, Subject};
use crate::notice::{Level, Notice};
use crate::order::SortKey;
use crate::places::PlaceKind;
use crate::platform::freetype::FaceKey;
use crate::platform::freetype::FontStyle;
use crate::platform::geom::Rect;
use crate::platform::pixel;
use crate::platform::scroll;
use crate::prompt::{ConflictChoice, Prompt};
use crate::render::display::{DisplayList, DrawCmd, RoundedCorners};
use crate::render::image::ImageKey;
use crate::sidebar::{Row, Section};
use crate::theme::{
    BACKGROUND, BUTTON_HOVER, CELL_BG, FOCUS_RING, GRID_CURSOR, GRID_SELECT, GRID_SELECT_IDLE,
    ICON_DIR, ICON_FILE, ICON_IMAGE, NOTICE_ERROR, NOTICE_ERROR_BG, NOTICE_INFO, NOTICE_WARNING,
    NOTICE_WARNING_BG, PANEL_BG, ROW_CURSOR, ROW_SELECT, ROW_SELECT_IDLE, SCROLLBAR, SIDEBAR_BG,
    TEXT_DIM, TEXT_DISABLED, TEXT_FG,
};
use crate::unreadable::{Panel, Style};

/// A detail-list column's rectangle on one row.
fn list_column(x: i32, w: i32, y: i32, h: i32) -> Rect {
    Rect { x, y, w, h }
}

/// The scrollbar's resting width, how far it sits from the edge, the width of the
/// lane the pointer has to enter to wake it, and the shortest its thumb may get.
/// Logical pixels.
pub(super) const BAR_WIDTH: i32 = 10;
pub(super) const BAR_MARGIN: i32 = 3;
pub(super) const BAR_LANE: i32 = 28;
pub(super) const BAR_MIN_THUMB: i32 = 32;

/// The help overlay, one line each. Every action the explorer has is reachable
/// from the keyboard, and this is where that is written down.
const HELP_LINES: &[&str] = &[
    "bnkfiles — keys",
    "",
    "explorer",
    "  Arrows        move the cursor      (Shift: extend)",
    "  Ctrl+click    add to the selection",
    "  Enter / →     open  (a folder: go in)",
    "  Backspace/←   up to the parent folder",
    "  Alt+← / Alt+→ back / forward, out of and into a picture  (thumb buttons too)",
    "  Home / End    first / last entry",
    "  PgUp / PgDn   a screenful",
    "  /             filter by name  (Enter: to the matches, Esc: clear)",
    "  Tab           entries / places / path / filter / inspector",
    "  Enter, ← →    in the places: fold / unfold a section",
    "  V             list ⇄ thumbnails",
    "  G             thumbnails: auto, square or masonry (this folder)",
    "  S / Shift+S   sort by the next column / the other way",
    "  I             inspector",
    "  Ctrl+A        select all",
    "  Ctrl+H        show hidden entries",
    "  Ctrl+D        pin this folder to places",
    "  F5 / Ctrl+R   read the folder again",
    "",
    "changing files",
    "  F2            rename",
    "  Delete        move to the trash",
    "  Shift+Delete  delete for good  (asks first)",
    "  Esc           cancel the running job",
    "",
    "viewer",
    "  ← →           previous / next picture",
    "  Esc           back to the explorer",
    "  +  -          zoom in / out",
    "  0  /  1       fit / actual size",
    "  B             bare ⇄ framed (sidebar, header)",
    "  Scroll        next/prev  (Ctrl: zoom)",
    "  Double-click  fit / 100%",
    "",
    "anywhere",
    "  C  /  Y       copy path / image",
    "  F             fullscreen",
    "  D             debug overlay",
    "  Ctrl+P / Menu every command, by name  (right-click: for the entry)",
    "  H  /  ?       this help",
    "  Q             quit",
];

impl State {
    /// The frame's drawing commands, in back-to-front order.
    pub(super) fn build_frame_list(&mut self) -> DisplayList {
        let started = std::time::Instant::now();
        // Re-prioritize queued decodes around where the user is now looking, then ask
        // for what the grid is about to draw. A resident or in-flight thumbnail is not
        // requested again, so this is cheap every frame.
        self.sync_focus();
        if self.mode == ViewMode::Grid {
            self.ensure_thumbs();
        }
        // The inspector's preview, on the same terms as a visible cell's.
        if let Some(row) = self.inspector_thumb_row() {
            self.request_thumb(row, crate::loader::Class::Visible);
        }
        // The viewer asks again for the picture it is waiting on. A request can be
        // dropped without anything else noticing — a folder reopened while a decode
        // was in flight disowns it — and re-asking here means the viewer heals on
        // the next repaint instead of sitting on the previous image forever. Free
        // when the picture is resident or already queued, which it nearly always is.
        if self.mode == ViewMode::Viewer {
            self.ensure_target();
        }

        let (w, h) = (self.width as i32, self.height as i32);
        let mut list: DisplayList = vec![DrawCmd::Fill {
            rect: Rect { x: 0, y: 0, w, h },
            color: BACKGROUND,
        }];

        match self.mode {
            ViewMode::Viewer => {
                self.paint_viewer(&mut list);
                self.paint_viewer_frame(&mut list);
            }
            ViewMode::List | ViewMode::Grid => self.paint_explorer(&mut list),
        }
        self.paint_status(&mut list);
        // A prompt is modal, so it goes over everything: it is the only thing the
        // keyboard is talking to while it is up.
        self.paint_prompt(&mut list);
        self.paint_menu(&mut list);
        if self.show_help {
            self.paint_help(&mut list);
        }
        if self.show_debug {
            self.paint_debug(&mut list);
        }

        // Whatever this frame actually draws has been seen: clear it from the
        // decoded-but-undrawn set so eviction does not later miscount it as wasted.
        if !self.undrawn.is_empty() {
            for cmd in &list {
                if let DrawCmd::Image { key, .. } = cmd {
                    self.undrawn.remove(key);
                }
            }
        }

        self.last_frame_ms = started.elapsed().as_secs_f32() * 1000.0;
        list
    }

    // ---- the views -------------------------------------------------------

    /// The image, as one quad the GPU samples the texture across. Which image that
    /// is — the full decode, or its thumbnail standing in until the decode lands —
    /// is [`State::viewer_source`]'s decision.
    ///
    /// A zoomed picture runs past its canvas. Nothing clips it: the sidebar, the
    /// header and the status bar are opaque and drawn after it, so they cover
    /// whatever spills under them.
    fn paint_viewer(&self, list: &mut DisplayList) {
        let Some(canvas) = self.canvas() else {
            return;
        };
        let Some((key, img)) = self.viewer_source() else {
            return;
        };
        let (scale, ox, oy) =
            self.viewer
                .geometry(canvas.w as u32, canvas.h as u32, img.width, img.height);
        let rect = Rect {
            x: canvas.x + ox.round() as i32,
            y: canvas.y + oy.round() as i32,
            w: (img.width as f32 * scale).round() as i32,
            h: (img.height as f32 * scale).round() as i32,
        };
        list.extend(self.image_cmd(rect, key));
    }

    /// What the framed viewer has around the picture: the sidebar as it was in
    /// the explorer, and a header naming the picture. Nothing in the bare viewer.
    fn paint_viewer_frame(&self, list: &mut DisplayList) {
        let regions = self.regions();
        if regions.header.h == 0 {
            return;
        }
        self.paint_sidebar(list, &self.explorer_layout());
        self.paint_viewer_header(list, regions.header);
        self.paint_inspector(list, regions.inspector);
    }

    /// The viewer's header: its buttons, and between them where the picture
    /// sits in the folder and its name on the left, the zoom on the right.
    fn paint_viewer_header(&self, list: &mut DisplayList, header: Rect) {
        list.push(DrawCmd::Fill {
            rect: header,
            color: PANEL_BG,
        });
        let layout = self.header_layout();
        self.paint_controls(list, &layout);
        let Some(path) = self.images.get(self.index) else {
            return;
        };
        let middle = layout.middle;
        let size = self.font_px(FONT_STATUS);
        let metrics = self.fonts.metrics(size);
        let baseline = header.y + (header.h - metrics.line_height) / 2 + metrics.baseline;
        let face = FaceKey {
            size,
            style: FontStyle::Medium,
        };
        let gap = self.scale().px(explorer::GAP);
        let text = |x: i32, w: i32, color: u32, text: String| DrawCmd::Text {
            bounds: Rect {
                x,
                y: header.y,
                w,
                h: header.h,
            },
            x,
            baseline,
            face,
            color,
            text,
        };

        let zoom = self
            .view_dims()
            .map(|(cw, ch, iw, ih)| {
                let (s, _, _) = self.viewer.geometry(cw, ch, iw, ih);
                format!("{}%", (s * 100.0).round() as i32)
            })
            .unwrap_or_default();
        // Each piece is drawn only if it fits beside the ones already placed,
        // position first, then zoom; the name takes whatever is left.
        let right = middle.x + middle.w;
        let position = format!("{} / {}", self.index + 1, self.images.len());
        let position_w = self.text_width(size, &position);
        let mut x = middle.x;
        if position_w <= middle.w {
            list.push(text(x, position_w, TEXT_DIM, position));
            x += position_w + gap;
        }
        let mut zoom_w = self.text_width(size, &zoom);
        if !zoom.is_empty() && x + zoom_w <= right {
            list.push(text(right - zoom_w, zoom_w, TEXT_DIM, zoom));
        } else {
            zoom_w = 0;
        }
        let name_w = (right - zoom_w - gap - x).max(0);
        let name = explorer::truncate_middle(&files::file_name(path), name_w, &|t| {
            self.text_width(size, t)
        });
        list.push(text(x, name_w, TEXT_FG, name));
    }

    /// The header's buttons: each a glyph in a square, lit under the pointer and
    /// dimmed when it would do nothing.
    fn paint_controls(&self, list: &mut DisplayList, layout: &HeaderLayout) {
        let size = self.font_px(FONT_STATUS);
        let metrics = self.fonts.metrics(size);
        let face = FaceKey {
            size,
            style: FontStyle::Medium,
        };
        for &(control, rect) in &layout.controls {
            let enabled = self.control_enabled(control);
            if enabled && self.hover == Some(control) {
                list.push(DrawCmd::RoundRect {
                    rect,
                    color: BUTTON_HOVER,
                    radius: self.scale().px(4),
                    corners: RoundedCorners::Both,
                });
            }
            let color = if enabled { TEXT_FG } else { TEXT_DISABLED };
            let glyph = match control {
                Control::Back | Control::Close => "←",
                Control::Forward => "→",
                Control::Up => "↑",
                Control::Actual => "1:1",
                Control::Fit => "Fit",
                Control::Previous => "‹",
                Control::Next => "›",
                Control::Layout => {
                    self.paint_layout_icon(list, rect, color);
                    continue;
                }
                Control::Inspector => {
                    self.paint_inspector_icon(list, rect, color);
                    continue;
                }
            };
            let w = self.text_width(size, glyph);
            let x = rect.x + (rect.w - w) / 2;
            list.push(DrawCmd::Text {
                bounds: rect,
                x,
                baseline: rect.y + (rect.h - metrics.line_height) / 2 + metrics.baseline,
                face,
                color,
                text: glyph.to_string(),
            });
        }
    }

    /// The inspector button's icon: a window outline with its right-hand third
    /// filled, solid while the inspector is shown.
    fn paint_inspector_icon(&self, list: &mut DisplayList, button: Rect, color: u32) {
        let side = button.w / 2;
        let frame = Rect {
            x: button.x + (button.w - side) / 2,
            y: button.y + (button.h - side) / 2,
            w: side,
            h: side,
        };
        let t = self.scale().px(2).max(1);
        let panel_w = side / 3;
        let panel = Rect {
            x: frame.x + frame.w - panel_w,
            y: frame.y,
            w: panel_w,
            h: frame.h,
        };
        for edge in [
            Rect { h: t, ..frame },
            Rect {
                y: frame.y + frame.h - t,
                h: t,
                ..frame
            },
            Rect { w: t, ..frame },
        ] {
            list.push(DrawCmd::Fill { rect: edge, color });
        }
        if self.regions().inspector.w > 0 {
            list.push(DrawCmd::Fill { rect: panel, color });
        } else {
            list.push(DrawCmd::Fill {
                rect: Rect { w: t, ..panel },
                color,
            });
            list.push(DrawCmd::Fill {
                rect: Rect {
                    x: panel.x + panel.w - t,
                    w: t,
                    ..panel
                },
                color,
            });
        }
    }

    /// The layout button's icon: the layout a press switches *to*, so the
    /// button says what it does. Four squares for the grid, three bars for the
    /// list.
    fn paint_layout_icon(&self, list: &mut DisplayList, button: Rect, color: u32) {
        let side = button.w / 2;
        let (x0, y0) = (
            button.x + (button.w - side) / 2,
            button.y + (button.h - side) / 2,
        );
        let gap = (side / 6).max(1);
        let rects: Vec<Rect> = if self.mode == ViewMode::Grid {
            let bar = (side - 2 * gap) / 3;
            (0..3)
                .map(|i| Rect {
                    x: x0,
                    y: y0 + i * (bar + gap),
                    w: side,
                    h: bar.max(1),
                })
                .collect()
        } else {
            let cell = (side - gap) / 2;
            (0..4)
                .map(|i| Rect {
                    x: x0 + (i % 2) * (cell + gap),
                    y: y0 + (i / 2) * (cell + gap),
                    w: cell.max(1),
                    h: cell.max(1),
                })
                .collect()
        };
        for rect in rects {
            list.push(DrawCmd::Fill { rect, color });
        }
    }

    // ---- the explorer ----------------------------------------------------

    /// The explorer: the places sidebar, the header over the content column, and
    /// the entries in whichever layout is showing.
    ///
    /// Nothing clips a row or a cell scrolled partly past the content's top,
    /// so the header is painted after the entries: opaque, it covers what
    /// runs up under it, as the list's headings do.
    fn paint_explorer(&self, list: &mut DisplayList) {
        let layout = self.explorer_layout();
        self.paint_sidebar(list, &layout);
        match self.mode {
            ViewMode::Grid => self.paint_entry_grid(list, &layout),
            _ => {
                self.paint_entry_list(list, &layout);
                self.paint_headings(list, &layout);
            }
        }
        self.paint_header(list, self.regions().header);
        self.paint_inspector(list, self.regions().inspector);
    }

    /// The detail list: an icon, a name, and the size and modified columns when the
    /// window is wide enough for them.
    fn paint_entry_list(&self, list: &mut DisplayList, layout: &explorer::ExplorerLayout) {
        let size = self.font_px(FONT_BROWSE);
        let metrics = self.fonts.metrics(size);
        let face = FaceKey {
            size,
            style: FontStyle::Regular,
        };
        let c = layout.columns;
        let bottom = layout.content.y + layout.content.h;

        for row in 0..self.rows.len() {
            let y = layout.row_y(row).round() as i32;
            if y + layout.row < layout.content.y || y > bottom {
                continue; // scrolled out of view
            }
            let Some(entry) = self.row_entry(row) else {
                continue;
            };
            if let Some(color) = self.row_highlight(row, ROW_SELECT, ROW_SELECT_IDLE, ROW_CURSOR) {
                list.push(DrawCmd::Fill {
                    rect: Rect {
                        x: layout.content.x,
                        y,
                        w: layout.content.w,
                        h: layout.row,
                    },
                    color,
                });
            }
            let baseline = y + (layout.row - metrics.line_height) / 2 + metrics.baseline;
            let icon_side = c.icon_w;
            self.paint_entry_icon(
                list,
                Rect {
                    x: c.icon_x,
                    y: y + (layout.row - icon_side) / 2,
                    w: icon_side,
                    h: icon_side,
                },
                entry,
            );

            // The name, with its middle taken out if the column is too narrow for
            // it. A directory keeps its trailing slash even when elided.
            let label = if entry.is_dir() {
                format!("{}/", entry.name)
            } else {
                entry.name.clone()
            };
            let label =
                explorer::truncate_middle(&label, c.name_w, &|text| self.text_width(size, text));
            list.push(DrawCmd::Text {
                bounds: Rect {
                    x: c.name_x,
                    y,
                    w: c.name_w,
                    h: layout.row,
                },
                x: c.name_x,
                baseline,
                face,
                color: if entry.is_dir() { TEXT_DIM } else { TEXT_FG },
                text: label,
            });

            // Size and modified, right-aligned so the digits line up down the
            // column and a long file is visibly longer than a short one.
            if c.size_w > 0 {
                let text = if entry.is_dir() {
                    // A directory's own size is its inode's, which says nothing
                    // about what is in it, so it says nothing at all.
                    "—".to_string()
                } else {
                    explorer::format_size(entry.size)
                };
                self.paint_right_aligned(
                    list,
                    list_column(c.size_x, c.size_w, y, layout.row),
                    &text,
                    size,
                    baseline,
                );
            }
            if c.modified_w > 0 {
                let local = entry.mtime_secs + crate::localtime::local_offset(entry.mtime_secs);
                let text = explorer::format_stamp(local);
                self.paint_right_aligned(
                    list,
                    list_column(c.modified_x, c.modified_w, y, layout.row),
                    &text,
                    size,
                    baseline,
                );
            }
        }

        self.paint_explorer_message(list, layout);
        self.paint_scrollbar(
            list,
            &self.explorer_bar,
            layout.content_height,
            layout.content.h,
            layout.scroll as i32,
        );
    }

    /// The thumbnail grid, over the same rows and the same selection as the list.
    /// What to draw behind `row`: `selected` if it is, `cursor` if it is only where
    /// the keyboard is. Enter and the arrows act on that row whether or not it is
    /// selected, so it has to be visible once Escape or a Ctrl-click deselects it.
    ///
    /// With the keyboard elsewhere (the sidebar, the path, the inspector) the
    /// cursor is not drawn and the selection is drawn `idle`, which is how
    /// the entries show they are not the ones the keys go to. The filter
    /// field counts as theirs: what is typed there narrows them.
    fn row_highlight(&self, row: usize, selected: u32, idle: u32, cursor: u32) -> Option<u32> {
        let active = matches!(self.focus, Focus::Entries | Focus::Filter);
        if self.sel.is_selected(row) {
            Some(if active { selected } else { idle })
        } else {
            (active && self.sel.lead() == Some(row)).then_some(cursor)
        }
    }

    fn paint_entry_grid(&self, list: &mut DisplayList, layout: &explorer::ExplorerLayout) {
        let m = self.grid_metrics();
        let grid = self.grid_layout();
        let masonry = self.masonry_now();
        let ring = self.scale().px(3);
        let (ox, oy) = (layout.content.x, layout.content.y);
        let shown = grid.cells_overlapping(self.rows.len(), 0.0, layout.content.h as f32);

        for row in shown {
            let Some(tile) = grid.tile(row) else {
                continue;
            };
            // Whole pixels, each edge rounded, so neighbouring tiles keep their
            // gap rather than meeting or overlapping by a pixel.
            let x = ox + tile.x.round() as i32;
            let y = oy + tile.y.round() as i32;
            let w = (ox + (tile.x + tile.w).round() as i32 - x).max(1);
            let h = (oy + (tile.y + tile.h).round() as i32 - y).max(1);
            if y + h + m.label < oy || y > oy + layout.content.h {
                continue; // scrolled out of view
            }
            let Some(entry) = self.row_entry(row) else {
                continue;
            };
            if let Some(color) = self.row_highlight(row, GRID_SELECT, GRID_SELECT_IDLE, GRID_CURSOR)
            {
                list.push(DrawCmd::Fill {
                    rect: Rect {
                        x: x - ring,
                        y: y - ring,
                        w: w + 2 * ring,
                        h: h + 2 * ring,
                    },
                    color,
                });
            }
            list.push(DrawCmd::Fill {
                rect: Rect { x, y, w, h },
                color: CELL_BG,
            });
            // A picture shows its thumbnail once one has decoded; everything else
            // shows the icon its kind gets, drawn large in the middle of the cell.
            let thumb = self
                .row_thumb_key(row)
                .and_then(|key| self.store.get(&key).map(|t| (key, t)));
            match thumb {
                Some((key, t)) => {
                    // A square cell shows a thumbnail at its own size; a
                    // masonry tile is its picture's shape and is filled.
                    let (tw, th) = (t.width.max(1) as f32, t.height.max(1) as f32);
                    let fit = (w as f32 / tw).min(h as f32 / th);
                    let scale = if masonry { fit } else { fit.min(1.0) };
                    let (dw, dh) = ((tw * scale).round() as i32, (th * scale).round() as i32);
                    list.extend(self.image_cmd(
                        Rect {
                            x: x + (w - dw) / 2,
                            y: y + (h - dh) / 2,
                            w: dw,
                            h: dh,
                        },
                        key,
                    ));
                }
                None => {
                    let side = w.min(h).min(m.thumb) / 3;
                    self.paint_entry_icon(
                        list,
                        Rect {
                            x: x + (w - side) / 2,
                            y: y + (h - side) / 2,
                            w: side,
                            h: side,
                        },
                        entry,
                    );
                }
            }
            // The name under the cell, elided to the cell's width.
            let size = self.font_px(FONT_LABEL);
            let metrics = self.fonts.metrics(size);
            let label =
                explorer::truncate_middle(&entry.name, w, &|text| self.text_width(size, text));
            list.push(DrawCmd::Text {
                bounds: Rect {
                    x,
                    y: y + h,
                    w,
                    // The band the grid reserved for it, not the font's own line
                    // height: the two must agree or captions collide.
                    h: m.label,
                },
                x,
                baseline: y + h + metrics.baseline,
                face: FaceKey {
                    size,
                    style: FontStyle::Regular,
                },
                color: if entry.is_dir() { TEXT_DIM } else { TEXT_FG },
                text: label,
            });
        }

        self.paint_explorer_message(list, layout);
        self.paint_scrollbar(
            list,
            &self.explorer_bar,
            grid.content_height(layout.content.h),
            layout.content.h,
            grid.scroll as i32,
        );
    }

    /// The detail list's column headings, over the rows and opaque, so a row
    /// scrolled up under them is covered. The column the folder is sorted by
    /// is bright and carries an arrow for its direction.
    fn paint_headings(&self, list: &mut DisplayList, layout: &explorer::ExplorerLayout) {
        let band = layout.heading;
        if band.h == 0 {
            return;
        }
        list.push(DrawCmd::Fill {
            rect: band,
            color: BACKGROUND,
        });
        let rule = self.scale().px(1).max(1);
        list.push(DrawCmd::Fill {
            rect: Rect {
                y: band.y + band.h - rule,
                h: rule,
                ..band
            },
            color: CELL_BG,
        });
        let size = self.font_px(FONT_LABEL);
        let metrics = self.fonts.metrics(size);
        let baseline = band.y + (band.h - metrics.line_height) / 2 + metrics.baseline;
        let c = layout.columns;
        let columns = [
            (SortKey::Name, c.name_x, c.name_w, false),
            (SortKey::Size, c.size_x, c.size_w, true),
            (SortKey::Modified, c.modified_x, c.modified_w, true),
        ];
        for (key, x, w, right) in columns {
            if w == 0 {
                continue;
            }
            let sorted = key == self.order.key;
            // The arrow goes on the inner side, so a right-aligned column's
            // label keeps its edge in line with the values under it.
            let text = match (sorted, self.order.descending) {
                (false, _) => key.label().to_string(),
                (true, descending) => {
                    let arrow = if descending { "▼" } else { "▲" };
                    if right {
                        format!("{arrow} {}", key.label())
                    } else {
                        format!("{} {arrow}", key.label())
                    }
                }
            };
            let tw = self.text_width(size, &text);
            let tx = if right { x + (w - tw).max(0) } else { x };
            list.push(DrawCmd::Text {
                bounds: Rect {
                    x,
                    y: band.y,
                    w,
                    h: band.h,
                },
                x: tx,
                baseline,
                face: FaceKey {
                    size,
                    style: FontStyle::Medium,
                },
                color: if sorted { TEXT_FG } else { TEXT_DIM },
                text,
            });
        }
    }

    /// What to say when there are no rows to draw: that the directory could not be
    /// read, that it is still being read, or that it is genuinely empty. An empty
    /// pane with no explanation reads as all three at once.
    fn paint_explorer_message(&self, list: &mut DisplayList, layout: &explorer::ExplorerLayout) {
        if !self.rows.is_empty() {
            return;
        }
        if let Some(panel) = self.unreadable_panel() {
            self.paint_unreadable(list, &panel);
            return;
        }
        let text = match self.scan_done {
            false => "reading…".to_string(),
            true if self.filter.is_active() && self.rows_total > 0 => format!(
                "nothing here matches “{}” (Esc clears the filter)",
                self.filter.field().text()
            ),
            true if self.show_hidden => "this folder is empty".to_string(),
            true => "nothing here but hidden entries (Ctrl+H shows them)".to_string(),
        };
        let size = self.font_px(FONT_BROWSE);
        let metrics = self.fonts.metrics(size);
        let x = layout.content.x + self.scale().px(explorer::PAD);
        let y = layout.content.y + self.scale().px(explorer::PAD) * 2;
        list.push(DrawCmd::Text {
            bounds: Rect {
                x,
                y,
                w: layout.content.w,
                h: metrics.line_height,
            },
            x,
            baseline: y + metrics.baseline,
            face: FaceKey {
                size,
                style: FontStyle::Regular,
            },
            color: TEXT_DIM,
            text,
        });
    }

    /// A folder that could not be read: why, where, and what to do instead,
    /// each action a key in a cap and the words for it.
    fn paint_unreadable(&self, list: &mut DisplayList, panel: &Panel) {
        let text = |list: &mut DisplayList, line: &crate::unreadable::Line, color: u32| {
            let (size, style) = match line.style {
                Style::Headline => (FONT_BROWSE, FontStyle::Medium),
                Style::Text => (FONT_STATUS, FontStyle::Regular),
                Style::Small => (FONT_LABEL, FontStyle::Regular),
            };
            let size = self.font_px(size);
            list.push(DrawCmd::Text {
                bounds: line.rect,
                x: line.rect.x,
                baseline: line.rect.y + self.fonts.metrics(size).baseline,
                face: FaceKey { size, style },
                color,
                text: line.text.clone(),
            });
        };
        for line in &panel.lines {
            let color = match line.style {
                Style::Headline => TEXT_FG,
                Style::Text | Style::Small => TEXT_DIM,
            };
            text(list, line, color);
        }
        for row in &panel.actions {
            list.push(DrawCmd::RoundRect {
                rect: row.cap,
                color: CELL_BG,
                radius: self.scale().px(4),
                corners: RoundedCorners::Both,
            });
            text(list, &row.key, TEXT_FG);
            text(list, &row.label, TEXT_FG);
        }
    }

    /// The header over the content column: back, forward and up, the current
    /// directory as breadcrumb segments, each one clickable, and the layout
    /// toggle.
    fn paint_header(&self, list: &mut DisplayList, header: Rect) {
        if header.h == 0 {
            return;
        }
        list.push(DrawCmd::Fill {
            rect: header,
            color: PANEL_BG,
        });
        let layout = self.header_layout();
        self.paint_controls(list, &layout);
        if let Some(field) = layout.field {
            self.paint_filter_field(list, field);
        }
        let size = self.font_px(FONT_STATUS);
        let metrics = self.fonts.metrics(size);
        let baseline = header.y + (header.h - metrics.line_height) / 2 + metrics.baseline;
        let face = FaceKey {
            size,
            style: FontStyle::Medium,
        };
        let segments = self.path_segments();
        let focused = self.focus == Focus::Path;

        for (i, segment) in segments.iter().enumerate() {
            // The last segment is where you are; the rest is how you got there.
            // Those that are not there any more say so by fading out.
            let last = i + 1 == segments.len();
            let gone = self
                .unreadable
                .as_ref()
                .is_some_and(|u| u.is_gone(&segment.target));
            if focused && i == self.path_sel.min(segments.len().saturating_sub(1)) {
                list.push(DrawCmd::Fill {
                    rect: segment.rect,
                    color: ROW_SELECT,
                });
            }
            list.push(DrawCmd::Text {
                bounds: segment.rect,
                x: segment.rect.x,
                baseline,
                face,
                color: match (gone, last) {
                    (true, _) => TEXT_DISABLED,
                    (false, true) => TEXT_FG,
                    (false, false) => TEXT_DIM,
                },
                text: segment.label.clone(),
            });
            if !last {
                let gap = self.scale().px(explorer::GAP);
                let x = segment.rect.x + segment.rect.w + gap;
                list.push(DrawCmd::Text {
                    bounds: Rect {
                        x,
                        y: header.y,
                        w: self.text_width(size, explorer::SEPARATOR),
                        h: header.h,
                    },
                    x,
                    baseline,
                    face,
                    color: TEXT_DIM,
                    text: explorer::SEPARATOR.to_string(),
                });
            }
        }
    }

    /// The filter field: what has been typed, or a word saying what the field
    /// is for, and the caret and a ring while it has the keyboard.
    fn paint_filter_field(&self, list: &mut DisplayList, field: Rect) {
        let focused = self.focus == Focus::Filter;
        list.push(DrawCmd::RoundRect {
            rect: field,
            color: CELL_BG,
            radius: self.scale().px(4),
            corners: RoundedCorners::Both,
        });
        let size = self.font_px(FONT_STATUS);
        let metrics = self.fonts.metrics(size);
        let pad = self.scale().px(8);
        let inner = Rect {
            x: field.x + pad,
            y: field.y,
            w: (field.w - 2 * pad).max(0),
            h: field.h,
        };
        let baseline = field.y + (field.h - metrics.line_height) / 2 + metrics.baseline;
        let face = FaceKey {
            size,
            style: FontStyle::Regular,
        };
        let text = self.filter.field().text();
        if text.is_empty() && !focused {
            list.push(DrawCmd::Text {
                bounds: inner,
                x: inner.x,
                baseline,
                face,
                color: TEXT_DISABLED,
                text: "Filter  /".to_string(),
            });
        } else {
            // Scrolled so the caret stays in view, and clipped to the field.
            let measure = |t: &str| self.text_width(size, t);
            let cursor = self.filter.field().cursor();
            let shown = filter::visible(text, cursor, inner.w, &measure);
            let start = shown.start;
            list.push(DrawCmd::Text {
                bounds: inner,
                x: inner.x,
                baseline,
                face,
                color: TEXT_FG,
                text: text[shown].to_string(),
            });
            if focused {
                let x = inner.x + measure(&text[start..cursor.max(start)]);
                list.push(DrawCmd::Fill {
                    rect: Rect {
                        x: x.min(inner.x + inner.w),
                        y: field.y + (field.h - metrics.line_height) / 2,
                        w: self.scale().px(2).max(1),
                        h: metrics.line_height,
                    },
                    color: TEXT_FG,
                });
            }
        }
        if focused {
            self.paint_focus_ring(list, field);
        }
    }

    /// A sidebar section's header: its name, small and dim, with a mark saying
    /// whether it is open or folded.
    fn paint_section_header(
        &self,
        list: &mut DisplayList,
        band: Rect,
        section: Section,
        folded: bool,
    ) {
        let size = self.font_px(FONT_LABEL);
        let metrics = self.fonts.metrics(size);
        let x = band.x + self.scale().px(explorer::PAD);
        let mark = if folded { "▸" } else { "▾" };
        list.push(DrawCmd::Text {
            bounds: Rect {
                x,
                w: (band.x + band.w - x).max(1),
                ..band
            },
            x,
            baseline: band.y + (band.h - metrics.line_height) / 2 + metrics.baseline,
            face: FaceKey {
                size,
                style: FontStyle::Medium,
            },
            color: TEXT_DIM,
            text: format!("{mark}  {}", section.title().to_uppercase()),
        });
    }

    /// The places sidebar: its sections, their places, and how full each
    /// volume is.
    fn paint_sidebar(&self, list: &mut DisplayList, layout: &explorer::ExplorerLayout) {
        if layout.sidebar.w == 0 {
            return;
        }
        list.push(DrawCmd::Fill {
            rect: layout.sidebar,
            color: SIDEBAR_BG,
        });
        let size = self.font_px(FONT_BROWSE);
        let metrics = self.fonts.metrics(size);
        let pad = self.scale().px(explorer::PAD);
        let icon_w = self.scale().px(explorer::ICON);
        let focused = self.focus == Focus::Sidebar;

        for (i, row) in self.sidebar_rows().into_iter().enumerate() {
            let y = layout.place_y(i);
            if y + layout.row > layout.sidebar.y + layout.sidebar.h {
                break; // the window is too short for the rest
            }
            let band = Rect {
                x: layout.sidebar.x,
                y,
                w: layout.sidebar.w,
                h: layout.row,
            };
            let place = match row {
                Row::Header(section, folded) => {
                    if focused && i == self.place_sel {
                        list.push(DrawCmd::Fill {
                            rect: band,
                            color: ROW_SELECT,
                        });
                    }
                    self.paint_section_header(list, band, section, folded);
                    continue;
                }
                Row::Place(p) => match self.places.get(p) {
                    Some(place) => place,
                    None => continue,
                },
            };
            // The place standing for the directory being shown is marked, so the
            // sidebar says where you are and not only where you could go.
            let here = place.path == self.scan_dir;
            if here || (focused && i == self.place_sel) {
                list.push(DrawCmd::Fill {
                    rect: band,
                    color: ROW_SELECT,
                });
            }
            let icon = Rect {
                x: layout.sidebar.x + pad,
                y: y + (layout.row - icon_w) / 2,
                w: icon_w,
                h: icon_w,
            };
            // Pins get the file tint so they read as a choice the user made, and
            // collections the accent; the rest are places the machine has. One
            // whose folder is gone stays, faded: it may be on a drive that
            // comes back.
            let gone = self.gone_places.contains(&place.path);
            let color = match place.kind {
                _ if gone => TEXT_DISABLED,
                PlaceKind::Pinned => ICON_IMAGE,
                PlaceKind::Collection => GRID_SELECT,
                _ => ICON_DIR,
            };
            list.push(DrawCmd::RoundRect {
                rect: icon,
                color,
                radius: self.scale().px(3),
                corners: RoundedCorners::Both,
            });
            let text_x = icon.x + icon_w + self.scale().px(explorer::GAP);
            let width = (layout.sidebar.x + layout.sidebar.w - pad - text_x).max(1);
            let label =
                explorer::truncate_middle(&place.name, width, &|text| self.text_width(size, text));
            list.push(DrawCmd::Text {
                bounds: Rect {
                    x: text_x,
                    y,
                    w: width,
                    h: layout.row,
                },
                x: text_x,
                baseline: y + (layout.row - metrics.line_height) / 2 + metrics.baseline,
                face: FaceKey {
                    size,
                    style: FontStyle::Regular,
                },
                color: match (gone, here) {
                    (true, _) => TEXT_DISABLED,
                    (false, true) => TEXT_FG,
                    (false, false) => TEXT_DIM,
                },
                text: label,
            });
            // A volume says how full it is, in a thin bar along the foot of its
            // row, once the usage worker has asked.
            if let Some(fill) = self.fills.get(&place.path) {
                let h = self.scale().px(3).max(1);
                let track = Rect {
                    x: text_x,
                    y: y + layout.row - h - self.scale().px(3),
                    w: width,
                    h,
                };
                list.push(DrawCmd::Fill {
                    rect: track,
                    color: CELL_BG,
                });
                let used = (track.w as f32 * fill.used()).round() as i32;
                if used > 0 {
                    list.push(DrawCmd::Fill {
                        rect: Rect {
                            w: used.min(track.w),
                            ..track
                        },
                        color: ICON_DIR,
                    });
                }
            }
        }
    }

    /// What the inspector describes now: the selected entry, the selection,
    /// or the folder when nothing is selected; in the viewer, the picture.
    fn inspector_subject(&self) -> Subject<'_> {
        let one = |row: usize| {
            self.row_entry(row).map(|entry| Subject::One {
                entry,
                // Known only once a full decode of it exists.
                dims: self
                    .store
                    .get(&ImageKey::full(&entry.path))
                    .map(|img| (img.width, img.height)),
            })
        };
        let folder = Subject::Folder {
            dir: &self.scan_dir,
            items: self.rows.len(),
        };
        if let Some(subject) = self.inspected_row().and_then(one) {
            return subject;
        }
        match (self.mode, self.sel.count()) {
            (ViewMode::Viewer, _) | (_, 0 | 1) => folder,
            (_, count) => Subject::Many {
                count,
                bytes: self.selected_bytes(),
            },
        }
    }

    /// The inspector: a preview with a kind badge, the name, then the
    /// properties, top to bottom. Everything in it is already in memory.
    fn paint_inspector(&self, list: &mut DisplayList, panel: Rect) {
        if panel.w == 0 {
            return;
        }
        list.push(DrawCmd::Fill {
            rect: panel,
            color: SIDEBAR_BG,
        });
        let scale = self.scale();
        let pad = scale.px(16);
        let inner_w = (panel.w - 2 * pad).max(1);
        let subject = self.inspector_subject();
        let local = |secs: i64| secs + crate::localtime::local_offset(secs);
        let description = inspector::describe(&subject, &local);

        // The preview: a 4:3 well, the picture fitted inside it.
        let well = Rect {
            x: panel.x + pad,
            y: panel.y + pad,
            w: inner_w,
            h: inner_w * 3 / 4,
        };
        list.push(DrawCmd::Fill {
            rect: well,
            color: CELL_BG,
        });
        let picture = match (&subject, self.mode) {
            (_, ViewMode::Viewer) => self
                .viewer_source()
                .map(|(key, img)| (key, img.width, img.height)),
            (Subject::One { entry, .. }, _) => {
                let key = self.thumb_key_of(&entry.path);
                self.store
                    .get(&key)
                    .map(|img| (key.clone(), img.width, img.height))
            }
            _ => None,
        };
        match (picture, &subject) {
            (Some((key, iw, ih)), _) if iw > 0 && ih > 0 => {
                let fit = (well.w as f32 / iw as f32).min(well.h as f32 / ih as f32);
                let (w, h) = (
                    (iw as f32 * fit).round() as i32,
                    (ih as f32 * fit).round() as i32,
                );
                list.extend(self.image_cmd(
                    Rect {
                        x: well.x + (well.w - w) / 2,
                        y: well.y + (well.h - h) / 2,
                        w,
                        h,
                    },
                    key,
                ));
            }
            (_, Subject::One { entry, .. }) => {
                let side = well.h / 3;
                self.paint_entry_icon(
                    list,
                    Rect {
                        x: well.x + (well.w - side) / 2,
                        y: well.y + (well.h - side) / 2,
                        w: side,
                        h: side,
                    },
                    entry,
                );
            }
            (_, Subject::Folder { .. }) => {
                let side = well.h / 3;
                self.paint_folder_icon(
                    list,
                    Rect {
                        x: well.x + (well.w - side) / 2,
                        y: well.y + (well.h - side) / 2,
                        w: side,
                        h: side,
                    },
                );
            }
            _ => {}
        }
        if let Some(badge) = &description.badge {
            let size = self.font_px(FONT_LABEL);
            let metrics = self.fonts.metrics(size);
            let inset = scale.px(6);
            // A long extension is elided rather than run out of the well.
            let badge = &explorer::truncate_middle(badge, well.w - 4 * inset, &|t| {
                self.text_width(size, t)
            });
            let w = self.text_width(size, badge) + 2 * inset;
            let rect = Rect {
                x: well.x + inset,
                y: well.y + inset,
                w,
                h: metrics.line_height + inset,
            };
            list.push(DrawCmd::RoundRect {
                rect,
                color: PANEL_BG,
                radius: scale.px(3),
                corners: RoundedCorners::Both,
            });
            list.push(DrawCmd::Text {
                bounds: rect,
                x: rect.x + inset,
                baseline: rect.y + inset / 2 + metrics.baseline,
                face: FaceKey {
                    size,
                    style: FontStyle::Medium,
                },
                color: TEXT_DIM,
                text: badge.clone(),
            });
        }

        // The name, then one row per property: a dim label, its value beside it.
        let size = self.font_px(FONT_STATUS);
        let metrics = self.fonts.metrics(size);
        let mut y = well.y + well.h + pad;
        let line = |list: &mut DisplayList, x: i32, y: i32, w: i32, color, style, text: &str| {
            list.push(DrawCmd::Text {
                bounds: Rect {
                    x,
                    y,
                    w,
                    h: metrics.line_height,
                },
                x,
                baseline: y + metrics.baseline,
                face: FaceKey { size, style },
                color,
                text: explorer::truncate_middle(text, w, &|t| self.text_width(size, t)),
            });
        };
        let bottom = panel.y + panel.h;
        if y + metrics.line_height <= bottom {
            line(
                list,
                well.x,
                y,
                inner_w,
                TEXT_FG,
                FontStyle::Medium,
                &description.title,
            );
        }
        y += metrics.line_height + pad;
        let label_w = scale.px(96);
        let row_h = metrics.line_height + scale.px(6);
        for (label, value) in &description.rows {
            if y + metrics.line_height > bottom {
                break; // the window is too short for the rest
            }
            line(
                list,
                well.x,
                y,
                label_w,
                TEXT_DIM,
                FontStyle::Regular,
                label,
            );
            let value_x = well.x + label_w;
            line(
                list,
                value_x,
                y,
                (inner_w - label_w).max(1),
                TEXT_FG,
                FontStyle::Regular,
                value,
            );
            y += row_h;
        }
    }

    /// An entry's icon, composed from the display list's own primitives: no icon
    /// theme is loaded, so a folder is a tabbed rectangle and a file is a page,
    /// tinted by what it holds.
    fn paint_entry_icon(&self, list: &mut DisplayList, rect: Rect, entry: &FsEntry) {
        let radius = (rect.w / 8).max(1);
        if entry.is_dir() {
            self.paint_folder_icon(list, rect);
            return;
        }
        let is_image = files::is_image(&entry.path);
        list.push(DrawCmd::RoundRect {
            rect,
            color: if is_image { ICON_IMAGE } else { ICON_FILE },
            radius,
            corners: RoundedCorners::Both,
        });
        // A picture gets a horizon line across it, so an undecoded photograph is
        // still visibly a photograph rather than a coloured rectangle.
        if is_image {
            let bar = (rect.h / 5).max(1);
            list.push(DrawCmd::Fill {
                rect: Rect {
                    x: rect.x + bar,
                    y: rect.y + rect.h - 2 * bar,
                    w: rect.w - 2 * bar,
                    h: bar,
                },
                color: CELL_BG,
            });
        }
    }

    /// A folder: the tab along the top-left, then the body over it. Two
    /// rectangles read as a folder at every size one is drawn at.
    fn paint_folder_icon(&self, list: &mut DisplayList, rect: Rect) {
        let radius = (rect.w / 8).max(1);
        let tab_h = (rect.h / 4).max(1);
        list.push(DrawCmd::RoundRect {
            rect: Rect {
                x: rect.x,
                y: rect.y,
                w: rect.w / 2,
                h: tab_h * 2,
            },
            color: ICON_DIR,
            radius,
            corners: RoundedCorners::Top,
        });
        list.push(DrawCmd::RoundRect {
            rect: Rect {
                x: rect.x,
                y: rect.y + tab_h,
                w: rect.w,
                h: rect.h - tab_h,
            },
            color: ICON_DIR,
            radius,
            corners: RoundedCorners::Both,
        });
    }

    /// An outline marking the filter field while it has the keyboard.
    fn paint_focus_ring(&self, list: &mut DisplayList, rect: Rect) {
        let t = self.scale().px(2).max(1);
        for edge in [
            Rect {
                x: rect.x,
                y: rect.y,
                w: rect.w,
                h: t,
            },
            Rect {
                x: rect.x,
                y: rect.y + rect.h - t,
                w: rect.w,
                h: t,
            },
            Rect {
                x: rect.x,
                y: rect.y,
                w: t,
                h: rect.h,
            },
            Rect {
                x: rect.x + rect.w - t,
                y: rect.y,
                w: t,
                h: rect.h,
            },
        ] {
            list.push(DrawCmd::Fill {
                rect: edge,
                color: FOCUS_RING,
            });
        }
    }

    /// A column's text, pushed against its right edge.
    fn paint_right_aligned(
        &self,
        list: &mut DisplayList,
        column: Rect,
        text: &str,
        size: u32,
        baseline: i32,
    ) {
        let width = self.text_width(size, text);
        let x = column.x + (column.w - width).max(0);
        list.push(DrawCmd::Text {
            bounds: column,
            x,
            baseline,
            face: FaceKey {
                size,
                style: FontStyle::Regular,
            },
            color: TEXT_DIM,
            text: text.to_string(),
        });
    }

    /// The status bar: the picture's name, its place in the folder, and (in the
    /// viewer) the zoom. An opaque strip below the canvas, not a translucent one
    /// over it, so the image never has to be drawn behind text.
    fn paint_status(&self, list: &mut DisplayList) {
        let status = self.regions().status;
        let (y, bar_h) = (status.y, status.h);
        list.push(DrawCmd::Fill {
            rect: status,
            color: PANEL_BG,
        });

        // A running job outranks both: it is the thing changing the disk, and how
        // far along it is is the only question worth answering while it does.
        if let Some(job) = self.jobs.progress() {
            let done = match job.percent() {
                Some(pct) => format!("{pct}%"),
                // No denominator yet: the tree is still being counted.
                None => "measuring".to_string(),
            };
            let queued = match job.queued {
                0 => String::new(),
                n => format!("    {n} queued"),
            };
            let text = format!("{} {}    {done}{queued}", job.kind.verb(), job.current);
            self.paint_status_text(list, &text, y, bar_h);
            return;
        }

        // A notice about something that just happened outranks the standing
        // description, for as long as it is up.
        if let Some(notice) = self.notice_up() {
            self.paint_notice(list, notice, status);
            return;
        }

        // In the viewer the bar names the picture on screen and where it sits in
        // the folder; in the explorer it names what the cursor is on, and how much
        // of the directory is selected.
        let text = match self.mode {
            ViewMode::Viewer => {
                let Some(path) = self.shown.and_then(|i| self.images.get(i)) else {
                    return;
                };
                let zoom = self
                    .view_dims()
                    .map(|(cw, ch, iw, ih)| {
                        let (s, _, _) = self.viewer.geometry(cw, ch, iw, ih);
                        format!("    {}%", (s * 100.0).round() as i32)
                    })
                    .unwrap_or_default();
                format!(
                    "{}    {}/{}{}",
                    files::file_name(path),
                    self.shown.map(|i| i + 1).unwrap_or(0),
                    self.images.len(),
                    zoom
                )
            }
            // A folder that could not be read has no items to count: the
            // pane says what it has instead.
            _ if self.unreadable_panel().is_some() => return,
            _ => {
                // With a filter on, how many of how many: the rows left out
                // are still there, and the bar is where that is said.
                let plural = |n: usize| if n == 1 { "" } else { "s" };
                // A header too narrow for the field does not show the text,
                // so the bar does.
                let items = if self.filter.is_active() {
                    let hidden_field = self.header_layout().field.is_none();
                    format!(
                        "{}{} of {} item{}",
                        if hidden_field {
                            format!("“{}”: ", self.filter.field().text())
                        } else {
                            String::new()
                        },
                        self.rows.len(),
                        self.rows_total,
                        plural(self.rows_total)
                    )
                } else {
                    format!("{} item{}", self.rows.len(), plural(self.rows.len()))
                };
                match (self.sel.count(), self.lead_entry()) {
                    // One thing selected: name it, and say how big it is.
                    (1, Some(entry)) if !entry.is_dir() => format!(
                        "{}    {}    {items}",
                        entry.name,
                        explorer::format_size(entry.size)
                    ),
                    (1, Some(entry)) => format!("{}    {items}", entry.name),
                    (0, _) => items,
                    // Several: how many, and how much, which is the question a
                    // selection is usually being built to answer.
                    (n, _) => format!(
                        "{n} selected    {}    {items}",
                        explorer::format_size(self.selected_bytes())
                    ),
                }
            }
        };
        self.paint_status_text(list, &text, y, bar_h);
    }

    /// The notification line: the status bar, tinted for a warning or an
    /// error, with a mark in the level's colour before the text.
    fn paint_notice(&self, list: &mut DisplayList, notice: &Notice, bar: Rect) {
        let (mark, band) = match notice.level {
            Level::Info => (NOTICE_INFO, PANEL_BG),
            Level::Warning => (NOTICE_WARNING, NOTICE_WARNING_BG),
            Level::Error => (NOTICE_ERROR, NOTICE_ERROR_BG),
        };
        list.push(DrawCmd::Fill {
            rect: bar,
            color: band,
        });
        let pad = self.scale().px(12);
        let side = self.scale().px(8).min(bar.h);
        list.push(DrawCmd::RoundRect {
            rect: Rect {
                x: bar.x + pad,
                y: bar.y + (bar.h - side) / 2,
                w: side,
                h: side,
            },
            color: mark,
            radius: side / 2,
            corners: RoundedCorners::Both,
        });
        let x = bar.x + pad + side + self.scale().px(8);
        self.paint_status_text_at(list, &notice.text, x, bar.y, bar.h);
    }

    /// One line of text along the status bar.
    fn paint_status_text(&self, list: &mut DisplayList, text: &str, y: i32, bar_h: i32) {
        self.paint_status_text_at(list, text, self.scale().px(12), y, bar_h);
    }

    /// One line of text along the status bar, from `x` to the bar's padding.
    fn paint_status_text_at(&self, list: &mut DisplayList, text: &str, x: i32, y: i32, bar_h: i32) {
        let size = self.font_px(FONT_STATUS);
        let metrics = self.fonts.metrics(size);
        let pad = self.scale().px(12);
        let width = (self.width as i32 - x - pad).max(1);
        list.push(DrawCmd::Text {
            bounds: Rect {
                x,
                y,
                w: width,
                h: bar_h,
            },
            x,
            baseline: y + (bar_h - metrics.line_height) / 2 + metrics.baseline,
            face: FaceKey {
                size,
                style: FontStyle::Medium,
            },
            color: TEXT_FG,
            // Elided rather than allowed to run off the edge: a message about a
            // long path would otherwise be a line of text with no end.
            text: explorer::truncate_middle(text, width, &|t| self.text_width(size, t)),
        });
    }

    // ---- overlays --------------------------------------------------------

    fn paint_help(&self, list: &mut DisplayList) {
        let size = self.font_px(FONT_STATUS);
        let metrics = self.fonts.metrics(size);
        let face = FaceKey {
            size,
            style: FontStyle::Regular,
        };
        let widest = HELP_LINES
            .iter()
            .map(|line| self.text_width(size, line))
            .fold(0, i32::max);
        let pad = self.scale().px(24);
        let pw = widest + 2 * pad;
        let ph = HELP_LINES.len() as i32 * metrics.line_height + 2 * pad;
        let px = ((self.width as i32 - pw) / 2).max(0);
        let py = ((self.height as i32 - ph) / 2).max(0);
        self.paint_panel(
            list,
            Rect {
                x: px,
                y: py,
                w: pw,
                h: ph,
            },
        );

        for (i, line) in HELP_LINES.iter().enumerate() {
            let y = py + pad + i as i32 * metrics.line_height;
            list.push(DrawCmd::Text {
                bounds: Rect {
                    x: px + pad,
                    y,
                    w: pw - 2 * pad,
                    h: metrics.line_height,
                },
                x: px + pad,
                baseline: y + metrics.baseline,
                face,
                color: TEXT_FG,
                text: (*line).to_string(),
            });
        }
    }

    /// The question on screen, centred over everything else.
    ///
    /// The choices are a marked column rather than a row of buttons: it needs no
    /// per-choice hit rectangles to lay out, it reads the same at every window
    /// width, and the marker says which answer Enter will take without relying on
    /// a colour to carry it.
    fn paint_prompt(&self, list: &mut DisplayList) {
        let Some(prompt) = self.prompt.as_ref() else {
            return;
        };
        let mut lines: Vec<String> = Vec::new();
        // Which line carries the rename field, and how far into it the cursor is.
        let mut caret: Option<(usize, usize)> = None;
        // The marker for the answer Enter would take.
        let mark = |selected: bool| if selected { "> " } else { "  " };

        match prompt {
            Prompt::Rename { field, .. } => {
                lines.push("Rename".into());
                lines.push(String::new());
                caret = Some((lines.len(), field.cursor()));
                lines.push(field.text().to_string());
                lines.push(String::new());
                lines.push("Enter to rename    Escape to cancel".into());
            }
            Prompt::Conflict { path, choice, all } => {
                lines.push(format!("{} already exists", files::file_name(path)));
                lines.push(String::new());
                for (i, option) in ConflictChoice::ALL.iter().enumerate() {
                    lines.push(format!("{}{}", mark(i == *choice), option.label()));
                }
                lines.push(String::new());
                lines.push(format!(
                    "A: apply to the whole job [{}]    Escape stops it",
                    if *all { "yes" } else { "no" }
                ));
            }
            Prompt::ConfirmDelete { paths, yes } => {
                lines.push(format!(
                    "Delete {} item{} for good?",
                    paths.len(),
                    if paths.len() == 1 { "" } else { "s" }
                ));
                lines.push("This does not go to the trash.".into());
                lines.push(String::new());
                lines.push(format!("{}Cancel", mark(!*yes)));
                lines.push(format!("{}Delete", mark(*yes)));
            }
        }

        let size = self.font_px(FONT_STATUS);
        let metrics = self.fonts.metrics(size);
        let face = FaceKey {
            size,
            style: FontStyle::Regular,
        };
        let widest = lines
            .iter()
            .map(|line| self.text_width(size, line))
            .fold(0, i32::max);
        let pad = self.scale().px(24);
        // A floor on the width so a short name does not make a cramped little box
        // that jumps in size as the user types.
        let pw = widest.max(self.scale().px(320)) + 2 * pad;
        let ph = lines.len() as i32 * metrics.line_height + 2 * pad;
        let px = ((self.width as i32 - pw) / 2).max(0);
        let py = ((self.height as i32 - ph) / 2).max(0);
        self.paint_panel(
            list,
            Rect {
                x: px,
                y: py,
                w: pw,
                h: ph,
            },
        );

        for (i, line) in lines.iter().enumerate() {
            let y = py + pad + i as i32 * metrics.line_height;
            list.push(DrawCmd::Text {
                bounds: Rect {
                    x: px + pad,
                    y,
                    w: pw - 2 * pad,
                    h: metrics.line_height,
                },
                x: px + pad,
                baseline: y + metrics.baseline,
                face,
                color: TEXT_FG,
                text: line.clone(),
            });
            // The text cursor, drawn where the characters before it end rather
            // than at a character count, so a name with an accent in it puts the
            // caret where the glyphs actually stop.
            if let Some((caret_line, offset)) = caret {
                if caret_line == i {
                    let x = px + pad + self.text_width(size, &line[..offset.min(line.len())]);
                    list.push(DrawCmd::Fill {
                        rect: Rect {
                            x,
                            y,
                            w: self.scale().px(2).max(1),
                            h: metrics.line_height,
                        },
                        color: TEXT_FG,
                    });
                }
            }
        }
    }

    /// The command list: a field to narrow it, then each command with its
    /// key, dimmed when it cannot run now.
    fn paint_menu(&self, list: &mut DisplayList) {
        let (Some(menu), Some(layout)) = (self.menu.as_ref(), self.menu_layout()) else {
            return;
        };
        self.paint_panel(list, layout.panel);
        let size = self.font_px(FONT_STATUS);
        let metrics = self.fonts.metrics(size);
        let pad = self.scale().px(8);
        let face = FaceKey {
            size,
            style: FontStyle::Regular,
        };
        let baseline = |r: Rect| r.y + (r.h - metrics.line_height) / 2 + metrics.baseline;
        let measure = |t: &str| self.text_width(size, t);

        let field = layout.field;
        list.push(DrawCmd::RoundRect {
            rect: field,
            color: CELL_BG,
            radius: self.scale().px(4),
            corners: RoundedCorners::Both,
        });
        let inner = Rect {
            x: field.x + pad,
            w: (field.w - 2 * pad).max(0),
            ..field
        };
        let text = menu.field().text();
        if text.is_empty() {
            list.push(DrawCmd::Text {
                bounds: inner,
                x: inner.x,
                baseline: baseline(field),
                face,
                color: TEXT_DISABLED,
                text: "Type to narrow the commands".to_string(),
            });
        }
        let cursor = menu.field().cursor();
        let shown = filter::visible(text, cursor, inner.w, &measure);
        let start = shown.start;
        list.push(DrawCmd::Text {
            bounds: inner,
            x: inner.x,
            baseline: baseline(field),
            face,
            color: TEXT_FG,
            text: text[shown].to_string(),
        });
        list.push(DrawCmd::Fill {
            rect: Rect {
                x: (inner.x + measure(&text[start..cursor.max(start)])).min(inner.x + inner.w),
                y: field.y + (field.h - metrics.line_height) / 2,
                w: self.scale().px(2).max(1),
                h: metrics.line_height,
            },
            color: TEXT_FG,
        });

        let commands = menu.shown();
        for &(i, row) in &layout.rows {
            let Some(&command) = commands.get(i) else {
                continue;
            };
            if i == menu.cursor() {
                list.push(DrawCmd::Fill {
                    rect: row,
                    color: ROW_SELECT,
                });
            }
            let enabled = self.command_enabled(command);
            let keys = command.keys();
            let keys_w = measure(&keys);
            let keys_x = row.x + row.w - pad - keys_w;
            list.push(DrawCmd::Text {
                bounds: Rect {
                    x: keys_x,
                    w: keys_w,
                    ..row
                },
                x: keys_x,
                baseline: baseline(row),
                face,
                color: if enabled { TEXT_DIM } else { TEXT_DISABLED },
                text: keys,
            });
            let title_w = (keys_x - row.x - 2 * pad).max(1);
            list.push(DrawCmd::Text {
                bounds: Rect {
                    x: row.x + pad,
                    w: title_w,
                    ..row
                },
                x: row.x + pad,
                baseline: baseline(row),
                face,
                color: if enabled { TEXT_FG } else { TEXT_DISABLED },
                text: explorer::truncate_middle(command.title(), title_w, &measure),
            });
        }
    }

    fn paint_debug(&self, list: &mut DisplayList) {
        let size = self.font_px(FONT_LABEL);
        let metrics = self.fonts.metrics(size);
        let mut lines = vec![
            format!(
                "mode {}  {}",
                match self.mode {
                    ViewMode::Grid => "grid",
                    ViewMode::Viewer => "viewer",
                    ViewMode::List => "list",
                },
                if self.fullscreen { "fs" } else { "win" }
            ),
            format!(
                "win {}x{} @ {}%",
                self.width,
                self.height,
                self.scaling.factor_120 * 100 / 120
            ),
            format!("frame {:.2} ms build", self.last_frame_ms),
            format!("imgs {}  resident {}", self.images.len(), self.store.len()),
        ];
        // A dropped progress update means the loop stopped draining the job
        // channel. It costs nothing to display and it should never be anything
        // but zero, which is exactly what makes it worth showing.
        if self.jobs.dropped > 0 {
            lines.push(format!("job updates dropped {}", self.jobs.dropped));
        }
        if let Some(img) = self.shown_image() {
            lines.push(format!("img {}x{}", img.width, img.height));
        }
        // Decode-pipeline counters: readiness on display, decode volume and waste,
        // and the memory high-water.
        let s = &self.pipe_stats;
        let shows = s.show_hits + s.show_misses;
        let hit_pct = (s.show_hits * 100).checked_div(shows).unwrap_or(0);
        lines.push(format!("ready {}/{} ({}%)", s.show_hits, shows, hit_pct));
        lines.push(format!(
            "dec f{} t{}  waste s{} c{} u{}",
            s.full_decoded,
            s.thumb_decoded,
            s.queued_skipped,
            s.active_stale_completed,
            s.decoded_then_unused
        ));
        lines.push(format!(
            "peak {:.0} MB  q{}",
            s.peak_resident_bytes as f64 / (1024.0 * 1024.0),
            s.peak_inflight
        ));
        let pad = self.scale().px(8);
        let widest = lines
            .iter()
            .map(|l| self.text_width(size, l))
            .fold(0, i32::max);
        let pw = widest + 2 * pad;
        let ph = lines.len() as i32 * metrics.line_height + 2 * pad;
        self.paint_panel(
            list,
            Rect {
                x: 0,
                y: 0,
                w: pw,
                h: ph,
            },
        );

        for (i, line) in lines.into_iter().enumerate() {
            let y = pad + i as i32 * metrics.line_height;
            list.push(DrawCmd::Text {
                bounds: Rect {
                    x: pad,
                    y,
                    w: pw - 2 * pad,
                    h: metrics.line_height,
                },
                x: pad,
                baseline: y + metrics.baseline,
                face: FaceKey {
                    size,
                    style: FontStyle::Regular,
                },
                color: 0x008F_E08F,
                text: line,
            });
        }
    }

    fn paint_panel(&self, list: &mut DisplayList, rect: Rect) {
        list.push(DrawCmd::Fill {
            rect,
            color: PANEL_BG,
        });
    }

    // ---- the scrollbar ---------------------------------------------------

    /// The overlay scrollbar: a thumb over the content's right edge, lit by a
    /// scroll and faded out after it, grown into something grabbable when the
    /// pointer comes near. Drawn only while it has any light in it at all.
    fn paint_scrollbar(
        &self,
        list: &mut DisplayList,
        bar: &scroll::Scrollbar,
        content: i32,
        viewport: i32,
        scroll_y: i32,
    ) {
        let lit = bar.lit();
        if lit <= 0.0 || !scroll::scrollable(content, viewport) {
            return;
        }
        let Some(track) = self.bar_track() else {
            return;
        };
        let min_thumb = self.scale().px(BAR_MIN_THUMB);
        let Some(thumb) = scroll::thumb(track, viewport, content, scroll_y, min_thumb) else {
            return;
        };
        // The bar grows from a thin resting indicator to the full grabbable slider.
        let thin = (track.w as f32 * 0.45).round() as i32;
        let w = thin + ((track.w - thin) as f32 * bar.wide()).round() as i32;
        // The display list draws only opaque fills, so a translucent bar blends
        // itself against the surface it is known to sit on, and its fade is that
        // blend falling back to the background.
        let color = pixel::tint(BACKGROUND, SCROLLBAR, lit * 0.85);
        list.push(DrawCmd::Fill {
            rect: Rect {
                x: track.x + (track.w - w),
                y: thumb.y,
                w,
                h: thumb.h,
            },
            color,
        });
    }

    // ---- text ------------------------------------------------------------

    /// The width `text` will take at `size`, for laying a panel out around it.
    pub(super) fn text_width(&self, size: u32, text: &str) -> i32 {
        let key = FaceKey {
            size,
            style: FontStyle::Regular,
        };
        self.fonts.text_advance(key, text).ceil() as i32
    }
}
