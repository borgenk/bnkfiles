//! The viewer's colours and fixed dimensions, in one place rather than scattered
//! through the code that draws with them.
//!
//! Colours are `0x00RRGGBB` (the top byte is unused: the surface is opaque
//! XRGB8888, and a GPU clear takes the RGB channels only). Dimensions are
//! *logical* pixels, scaled to device pixels through `platform::geom::Scale`, so
//! the chrome is the same physical size on a 100% and a 200% display.

/// The window background: what surrounds an image that does not fill the window,
/// and what a transparent pixel of one composites over.
pub const BACKGROUND: u32 = 0x001c_1c1f;

/// The status bar, the explorer's header band, and the help and debug overlays:
/// panels that sit over the content rather than beside it.
pub const PANEL_BG: u32 = 0x0014_1417;
/// A grid cell, behind a thumbnail that has not decoded yet, and the ring drawn
/// around the selected one.
pub const CELL_BG: u32 = 0x0026_262A;
pub const GRID_SELECT: u32 = 0x004D_80E6;
/// The ring around a selected cell while the keyboard is in another pane.
pub const GRID_SELECT_IDLE: u32 = 0x004A_5266;
/// The ring around the cell the keyboard is on, when that cell is not selected.
pub const GRID_CURSOR: u32 = 0x0058_5860;
/// The selected row in the detail list.
pub const ROW_SELECT: u32 = 0x0038_4C73;
/// A selected row while the keyboard is in another pane.
pub const ROW_SELECT_IDLE: u32 = 0x0032_3644;
/// The row the keyboard is on, when that row is not selected.
pub const ROW_CURSOR: u32 = 0x002A_2B31;
/// The overlay scrollbar's thumb, blended against what it sits on (see
/// `platform::pixel::tint`) as it fades.
pub const SCROLLBAR: u32 = 0x00C8_C8CE;

/// The places sidebar: a panel beside the content rather than over it, so it is
/// distinguished by being slightly darker than the listing, not by a border.
pub const SIDEBAR_BG: u32 = 0x0018_181B;
/// The outline drawn around the filter field while it has the keyboard.
pub const FOCUS_RING: u32 = 0x004D_80E6;

/// The drawn icons standing in for a file type. No icon theme is loaded, which
/// would mean finding one and rasterizing its SVGs; these are shapes composed
/// from the display list's own primitives, tinted so the three kinds are told
/// apart at a glance.
pub const ICON_DIR: u32 = 0x006E_9BD8;
pub const ICON_IMAGE: u32 = 0x0079_C08A;
pub const ICON_FILE: u32 = 0x0078_7880;

/// A header button under the pointer, behind its glyph.
pub const BUTTON_HOVER: u32 = 0x002A_2B31;
/// A header button that would do nothing now: back with no history, up at the
/// root. Also a place, or a breadcrumb segment, whose folder is gone.
pub const TEXT_DISABLED: u32 = 0x0058_5860;

/// The notification line, by level: the mark at its start, and the band
/// behind it. A confirmation keeps the status bar's own colour and is told
/// apart by its mark; a warning and an error tint the whole band, since
/// those are the ones a person should not miss.
pub const NOTICE_INFO: u32 = 0x004D_80E6;
pub const NOTICE_WARNING: u32 = 0x00E0_A43A;
pub const NOTICE_ERROR: u32 = 0x00E2_5D55;
pub const NOTICE_WARNING_BG: u32 = 0x0033_2A16;
pub const NOTICE_ERROR_BG: u32 = 0x0038_1D1F;

pub const TEXT_FG: u32 = 0x00EA_EAEA;
/// Secondary text: the breadcrumb, a size or date column, a directory row.
pub const TEXT_DIM: u32 = 0x00A0_A0A4;

/// Mask gamma applied to glyph coverage in the fragment shader. Linear-light
/// compositing renders light-on-dark text heavier than the gamma-space stacks
/// beside it; raising coverage to this power thins the anti-aliased edges back to
/// a matching weight. Higher is thinner, 1.0 disables it.
pub const GLYPH_GAMMA: f32 = 1.5;
