//! The app, used: the real binary in a headless compositor, driven by key
//! presses and clicks and judged by what it draws and what it says.
//!
//! These need labwc, grim and a GPU, and each starts a compositor, so they are
//! ignored by default and run with `make test-ui`. Nothing here is linked into
//! the shipped binary. See `tests/ui/rig.rs` for how a test reaches the app.
//!
//! A failed test keeps its directory (the panic message names it), with every
//! screenshot taken and the logs of the app and the compositor. Set
//! `BNKFILES_UI_KEEP` to keep the directories of passing tests too.

// The input driver speaks Wayland through the app's own connection code. The
// binary has no library target to import it from, so the four files are
// compiled into this test directly. Most of what they hold is unused here.
#[allow(dead_code)]
#[path = "../src/platform"]
mod platform {
    pub mod conn;
    pub mod error;
    pub mod ffi;
    pub mod wire;
}

#[path = "ui/input.rs"]
mod input;
#[path = "ui/rig.rs"]
mod rig;

use std::path::Path;
use std::time::Duration;

use input::key;
use rig::Rig;

const GRID: &str = "width=900\nheight=600\ngrid_view=true\n";

/// Write a 24-bit BMP of `w` x `h` whose pixels come from `colour(x, y)`.
/// The app decodes BMP itself, so a fixture needs no encoder.
fn write_bmp(path: &Path, w: u32, h: u32, colour: impl Fn(u32, u32) -> [u8; 3]) {
    let row = (w as usize * 3).div_ceil(4) * 4;
    let mut bytes = Vec::with_capacity(54 + row * h as usize);
    bytes.extend_from_slice(b"BM");
    bytes.extend_from_slice(&(54 + (row * h as usize) as u32).to_le_bytes());
    bytes.extend_from_slice(&[0; 4]);
    bytes.extend_from_slice(&54u32.to_le_bytes());
    bytes.extend_from_slice(&40u32.to_le_bytes());
    bytes.extend_from_slice(&(w as i32).to_le_bytes());
    // A negative height stores the rows top-down, as `colour` produces them.
    bytes.extend_from_slice(&(-(h as i32)).to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&24u16.to_le_bytes());
    bytes.extend_from_slice(&[0; 24]);
    for y in 0..h {
        let start = bytes.len();
        for x in 0..w {
            let [r, g, b] = colour(x, y);
            bytes.extend_from_slice(&[b, g, r]);
        }
        bytes.resize(start + row, 0);
    }
    std::fs::write(path, bytes).expect("write a fixture picture");
}

/// The flat colour of small picture `i`: saturated enough to tell from the
/// app's greys, and different from its neighbours'.
fn colour_of(i: usize) -> [u8; 3] {
    // Offset by one so that no picture is the grey that all three channels
    // start from.
    let i = i + 1;
    [
        (60 + i * 83 % 181) as u8,
        (60 + i * 29 % 181) as u8,
        (60 + i * 151 % 181) as u8,
    ]
}

/// `count` small pictures, each one flat colour, named so they sort in order.
fn small_pictures(dir: &Path, count: usize) {
    for i in 0..count {
        let colour = colour_of(i);
        write_bmp(&dir.join(format!("picture-{i:04}.bmp")), 64, 48, |_, _| {
            colour
        });
    }
}

/// `count` photographs of 3000 x 2000, each a flat colour. Large enough that
/// decoding and uploading one takes real time.
fn large_pictures(dir: &Path, count: usize) {
    for i in 0..count {
        let colour = colour_of(i);
        write_bmp(&dir.join(format!("photo-{i}.bmp")), 3000, 2000, |_, _| {
            colour
        });
    }
}

/// How many pixels of picture `i`'s colour a screenshot must hold for the
/// picture to count as drawn. A thumbnail of a small picture is 64 x 48.
const DRAWN: usize = 2000;

fn assert_quiet(rig: &Rig) {
    assert_eq!(rig.app_log(), "", "nothing should be reported on stderr");
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn a_folder_of_pictures_opens_as_a_grid_of_thumbnails() {
    let mut rig = Rig::start("grid", GRID, |dir| small_pictures(dir, 12));
    let probe = rig.ready();
    assert_eq!(probe.mode, "grid");
    assert_eq!(probe.rows, 12);
    assert_eq!(probe.dir, rig.folder().to_string_lossy());

    let shot = rig.screenshot("grid");
    for i in 0..8 {
        assert!(
            shot.count_near(colour_of(i), 2) >= DRAWN,
            "picture {i} should be drawn: {}",
            shot.path.display()
        );
    }
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn the_window_survives_a_storm_of_resizes() {
    let mut rig = Rig::start("resize", GRID, |dir| small_pictures(dir, 12));
    let before = rig.ready();

    // Each chord asks the compositor for a different size, faster than the
    // app can draw one, so configures pile up and buffers are replaced while
    // the compositor still holds the old ones.
    for _ in 0..15 {
        for size in [key::N1, key::N2, key::N3] {
            rig.input.super_chord(size);
        }
    }
    // The last chord of the storm asks for 700 x 500.
    let after = rig.wait("the last size to be drawn", |p| p.size == (700, 500));
    assert_ne!(after.size, before.size);

    // A replaced buffer is freed once the compositor lets go of it. Nothing
    // reports that by itself, so ask for one more frame and read its line.
    rig.drain(Duration::from_millis(300));
    rig.input.tap(key::DOWN);
    let settled = rig.wait("a frame after the storm", |p| p.frame > after.frame);
    assert_eq!(settled.size, (700, 500), "the size should have settled");
    assert_eq!(settled.retired, 0, "every replaced buffer should be freed");

    let shot = rig.screenshot("after-resizes");
    assert!(
        shot.count_near(colour_of(0), 2) >= DRAWN,
        "the grid should still be drawn: {}",
        shot.path.display()
    );
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn a_minimised_window_catches_up_when_it_is_restored() {
    let mut rig = Rig::start("minimise", GRID, |dir| small_pictures(dir, 4));
    rig.ready();

    rig.input.super_chord(key::D);
    rig.drain(Duration::from_millis(300));
    let hidden = rig.screenshot("minimised");
    assert_eq!(
        hidden.count_near(colour_of(0), 2),
        0,
        "the window should be out of sight: {}",
        hidden.path.display()
    );
    // The folder changes while nobody can see the window.
    write_bmp(&rig.folder().join("picture-9999.bmp"), 64, 48, |_, _| {
        colour_of(99)
    });
    rig.drain(Duration::from_millis(500));

    rig.input.super_chord(key::D);
    rig.wait("the new picture to be listed", |p| p.rows == 5 && p.idle());
    let shot = rig.screenshot("restored");
    assert!(
        shot.count_near(colour_of(99), 2) >= DRAWN,
        "the picture added while minimised should be drawn: {}",
        shot.path.display()
    );
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn a_window_on_another_workspace_catches_up_when_it_is_shown_again() {
    let mut rig = Rig::start("workspace", GRID, |dir| small_pictures(dir, 4));
    rig.ready();

    rig.input.super_chord(key::RIGHT);
    rig.drain(Duration::from_millis(300));
    let hidden = rig.screenshot("other-workspace");
    assert_eq!(
        hidden.count_near(colour_of(0), 2),
        0,
        "the window should be out of sight: {}",
        hidden.path.display()
    );
    write_bmp(&rig.folder().join("picture-9999.bmp"), 64, 48, |_, _| {
        colour_of(99)
    });
    rig.drain(Duration::from_millis(500));

    rig.input.super_chord(key::RIGHT);
    rig.wait("the new picture to be listed", |p| p.rows == 5 && p.idle());
    let shot = rig.screenshot("shown-again");
    assert!(
        shot.count_near(colour_of(99), 2) >= DRAWN,
        "the picture added while hidden should be drawn: {}",
        shot.path.display()
    );
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn paging_through_large_photos_ends_on_a_drawn_one() {
    let mut rig = Rig::start("paging", GRID, |dir| large_pictures(dir, 6));
    rig.ready();

    rig.input.tap(key::ENTER);
    rig.wait("the first photo", |p| {
        p.mode == "viewer" && p.shown == Some(0)
    });

    // Held long enough for key repeat to run through the folder several
    // times, asking for photos faster than they decode.
    rig.input.hold(key::RIGHT, Duration::from_millis(2500));
    // Neighbours are still decoding when the photo itself is up, and finish
    // without a frame, so only the photo on screen is waited for.
    // The lines printed during the hold are history; skip to the present.
    rig.drain(Duration::from_millis(300));
    let end = rig.wait("the photo it stopped on", |p| p.shown == Some(p.index));
    assert_eq!(end.mode, "viewer");

    let shot = rig.screenshot("after-paging");
    assert!(
        shot.count_near(colour_of(end.index), 2) >= 100_000,
        "photo {} should fill the view: {}",
        end.index,
        shot.path.display()
    );
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn scrolling_a_big_grid_leaves_no_cell_empty() {
    let count = 600;
    let mut rig = Rig::start("scroll", GRID, |dir| small_pictures(dir, count));
    rig.ready();

    rig.input.hold(key::PAGEDOWN, Duration::from_millis(1500));
    rig.drain(Duration::from_millis(300));
    rig.input.tap(key::END);
    let end = rig.wait("the last picture", |p| {
        p.lead == Some(count - 1) && p.idle()
    });
    assert_eq!(end.rows, count);

    // The window is four cells wide and the count divides by four, so the
    // last row is full.
    let shot = rig.screenshot("end-of-grid");
    for i in count - 4..count {
        assert!(
            shot.count_near(colour_of(i), 2) >= DRAWN,
            "picture {i} should be drawn: {}",
            shot.path.display()
        );
    }
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn the_mouse_selects_a_cell_and_the_wheel_scrolls() {
    let mut rig = Rig::start("mouse", GRID, |dir| small_pictures(dir, 60));
    rig.ready();

    // The window is centred on the output; this is the middle of the second
    // cell of the first row.
    rig.input.click(644, 213, rig::OUTPUT);
    rig.wait("the click to select the second cell", |p| p.lead == Some(1));

    let before_frame = rig.last.frame;
    let before = rig.screenshot("before-wheel");
    for _ in 0..3 {
        rig.input.wheel(1);
    }
    rig.wait("the scrolled rows to be drawn", |p| {
        p.frame > before_frame && p.idle()
    });
    rig.drain(Duration::from_millis(300));
    let after = rig.screenshot("after-wheel");
    assert!(
        before.difference(&after) > 0.01,
        "the wheel should scroll the grid: {}",
        after.path.display()
    );
    assert_eq!(
        rig.last.lead,
        Some(1),
        "scrolling should not move the cursor"
    );
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn a_grid_row_scrolled_up_goes_under_the_header_not_over_it() {
    let mut rig = Rig::start("grid-under-header", GRID, |dir| small_pictures(dir, 60));
    let ready = rig.ready();

    // One notch scrolls less than a row, so the first row's cells are cut
    // by the content's top edge, right under the header.
    rig.input.move_to(640, 400, rig::OUTPUT);
    rig.input.wheel(1);
    rig.wait("the scrolled grid to be drawn", |p| {
        p.frame > ready.frame && p.idle()
    });
    rig.drain(Duration::from_millis(300));
    let shot = rig.screenshot("scrolled");

    // The header's band on the output: beside the sidebar, 44 tall under
    // the title bar. The first row's pictures have gone up under it; the
    // second row's still show.
    let header = ((1280 - 900) / 2 + 190, 73, 900 - 190, 44);
    for i in 4..8 {
        assert!(
            shot.count_near(colour_of(i), 2) >= DRAWN,
            "picture {i} should show below the header: {}",
            shot.path.display()
        );
    }
    for i in 0..4 {
        assert_eq!(
            shot.count_near_in(header, colour_of(i), 2),
            0,
            "picture {i} is drawn over the header: {}",
            shot.path.display()
        );
    }
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn a_covered_window_catches_up_when_it_is_uncovered() {
    let mut rig = Rig::start("covered", GRID, |dir| small_pictures(dir, 4));
    rig.ready();

    // A second window, made fullscreen, sits over the first and hides all of
    // it. The compositor stops asking a window nobody can see for frames.
    let alone = rig.screenshot("alone");
    rig.open_second_window();
    rig.wait_for_screen_to_change(&alone);
    rig.input.super_chord(key::F);
    rig.drain(Duration::from_millis(500));
    let covered = rig.screenshot("covered");
    assert_eq!(
        covered.count_near(colour_of(0), 2),
        0,
        "the first window should be covered: {}",
        covered.path.display()
    );

    write_bmp(&rig.folder().join("picture-9999.bmp"), 64, 48, |_, _| {
        colour_of(99)
    });
    rig.drain(Duration::from_millis(500));

    // Closing the window on top uncovers the first.
    rig.input.super_chord(key::X);
    rig.wait("the new picture to be listed", |p| p.rows == 5 && p.idle());
    rig.drain(Duration::from_millis(300));
    let shot = rig.screenshot("uncovered");
    assert!(
        shot.count_near(colour_of(99), 2) >= DRAWN,
        "the picture added while covered should be drawn: {}",
        shot.path.display()
    );
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn the_window_follows_the_output_scale() {
    // Pictures larger than a thumbnail, so each one fills its cell and its
    // size on screen is the cell's.
    let mut rig = Rig::start("scale", GRID, |dir| {
        for i in 0..12 {
            let colour = colour_of(i);
            write_bmp(
                &dir.join(format!("picture-{i:04}.bmp")),
                400,
                400,
                |_, _| colour,
            );
        }
    });
    let at_one = rig.ready();
    assert_eq!(at_one.size, (900, 600));
    let area_at_one = rig.screenshot("scale-1").count_near(colour_of(0), 2);
    assert!(area_at_one >= DRAWN);

    // Integer, fractional, and back: each is a different path through the
    // app, and each replaces the buffers and reopens the fonts. The window
    // outgrows the output at the larger scales, so only the first picture is
    // sure to be on it.
    for (scale, size, tag) in [
        (2.0, (1800, 1200), "scale-2"),
        (1.5, (1350, 900), "scale-1.5"),
        (1.0, (900, 600), "scale-1-again"),
    ] {
        rig.input.set_output_scale(scale);
        rig.wait(&format!("the window at scale {scale}"), |p| {
            p.size == size && p.idle()
        });
        rig.drain(Duration::from_millis(300));
        let shot = rig.screenshot(tag);
        let area = shot.count_near(colour_of(0), 2) as f64;
        let expected = area_at_one as f64 * scale * scale;
        assert!(
            (area - expected).abs() <= expected * 0.05,
            "the first picture should cover {expected} pixels at scale {scale}, not {area}: {}",
            shot.path.display()
        );
    }
    assert_quiet(&rig);
}

/// Every file under `dir`, with its size and modification time.
fn snapshot(dir: &Path) -> Vec<(std::path::PathBuf, u64, std::time::SystemTime)> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .expect("read the folder")
        .map(|entry| {
            let entry = entry.expect("a folder entry");
            let meta = entry.metadata().expect("its metadata");
            (
                entry.path(),
                meta.len(),
                meta.modified().expect("its mtime"),
            )
        })
        .collect();
    files.sort();
    files
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn nothing_the_keyboard_can_do_changes_the_folder() {
    let mut rig = Rig::start("read-only", GRID, |dir| small_pictures(dir, 4));
    let mut seen = rig.ready();
    let before = snapshot(&rig.folder());

    // Rename, trash, and delete for good, each followed by Enter in case a
    // question had been asked that Enter would answer yes to. Each key is
    // refused with a line in the status bar, which is a frame to wait for.
    for press in ["rename", "trash", "delete"] {
        match press {
            "rename" => rig.input.tap(key::F2),
            "trash" => rig.input.tap(key::DELETE),
            _ => rig.input.shift(key::DELETE),
        }
        seen = rig.wait(&format!("{press} to be refused"), |p| p.frame > seen.frame);
        assert!(!seen.prompt, "{press} should not open a prompt");
    }

    assert_eq!(
        snapshot(&rig.folder()),
        before,
        "the folder must be untouched"
    );
    assert!(!rig.trash().exists(), "nothing should reach the trash");
    assert!(
        !rig.thumbnail_cache().exists(),
        "no thumbnail cache should be written"
    );
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn a_photo_shown_small_is_smooth() {
    // One-pixel black and white squares: the finest detail a picture can
    // hold. Shown at a fraction of its size, each screen pixel covers dozens
    // of them and should come out an even grey. Sampled carelessly it comes
    // out as bands of light and dark instead.
    let mut rig = Rig::start("smooth", GRID, |dir| {
        write_bmp(&dir.join("checker.bmp"), 4096, 4096, |x, y| {
            if (x + y) % 2 == 0 {
                [255, 255, 255]
            } else {
                [0, 0, 0]
            }
        });
    });
    rig.ready();
    rig.input.tap(key::ENTER);
    rig.wait("the photo", |p| p.mode == "viewer" && p.shown == Some(0));

    let shot = rig.screenshot("checker");
    let (cx, cy) = (shot.width / 2, shot.height / 2);
    let (darkest, lightest) = shot.range_in(cx - 100, cy - 100, 200, 200);
    assert!(
        lightest - darkest <= 16,
        "the photo should be an even grey, but runs from {darkest} to {lightest}: {}",
        shot.path.display()
    );
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn q_exits_cleanly() {
    let mut rig = Rig::start("quit", GRID, |dir| small_pictures(dir, 4));
    rig.ready();
    rig.input.tap(key::Q);
    assert!(rig.exit_status().success());
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn closing_the_window_exits_cleanly() {
    let mut rig = Rig::start("close", GRID, |dir| small_pictures(dir, 4));
    rig.ready();
    rig.input.super_chord(key::X);
    assert!(rig.exit_status().success());
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn the_cursor_is_visible_in_both_layouts() {
    let mut rig = Rig::start("cursor", GRID, |dir| small_pictures(dir, 12));
    rig.ready();
    let at_first = rig.screenshot("grid-cursor-first");
    rig.input.tap(key::RIGHT);
    rig.wait("the cursor to move", |p| p.lead == Some(1));
    let moved = rig.screenshot("grid-cursor-second");
    assert!(
        at_first.difference(&moved) > 0.0005,
        "moving the cursor should change what the grid shows"
    );

    rig.input.tap(key::V);
    rig.wait("the list", |p| p.mode == "list");
    let at_second = rig.screenshot("list-cursor-second");
    rig.input.tap(key::DOWN);
    rig.wait("the cursor to move", |p| p.lead == Some(2));
    let at_third = rig.screenshot("list-cursor-third");
    assert!(
        at_second.difference(&at_third) > 0.0005,
        "moving the cursor should change what the list shows"
    );
    assert_quiet(&rig);
}

/// A panorama three times as wide as it is tall: fitted to any canvas this
/// app draws, its width is the canvas's width, so where it lands says exactly
/// how wide the canvas is. Its right half is a second colour, so a zoom can be
/// told to have kept its anchor.
fn panorama(dir: &Path) {
    let (left, right) = (colour_of(7), colour_of(8));
    write_bmp(&dir.join("panorama.bmp"), 3000, 1000, |x, _| {
        if x < 1500 {
            left
        } else {
            right
        }
    });
}

/// Where the whole panorama is on screen: the union of its two halves.
fn panorama_bounds(shot: &rig::Shot) -> (usize, usize, usize, usize) {
    let (lx, ly, lw, lh) = shot
        .bounds_of(colour_of(7), 2)
        .expect("the left half is drawn");
    let (rx, _, rw, _) = shot
        .bounds_of(colour_of(8), 2)
        .expect("the right half is drawn");
    // The seam is filtered into a column or two of the two colours mixed, so
    // the picture runs from the left half's left edge to the right half's
    // right one.
    assert!(lx + lw <= rx, "the halves overlap: {}", shot.path.display());
    (lx, ly, rx + rw - lx, lh)
}

/// The header, the status bar and the overlays' background.
const PANEL_BG: [u8; 3] = [0x14, 0x14, 0x17];

/// The sidebar's own background, which nothing else in the window is drawn in.
const SIDEBAR_BG: [u8; 3] = [0x18, 0x18, 0x1b];

/// Whether the pixel at `(x, y)` is within `tolerance` of `colour`.
fn pixel_is(shot: &rig::Shot, x: usize, y: usize, colour: [u8; 3]) -> bool {
    shot.pixel(x, y)
        .iter()
        .zip(colour)
        .all(|(a, b)| a.abs_diff(b) <= 2)
}

/// Assert the panorama is fitted to the framed viewer's canvas: the content
/// column beside the sidebar, under the header. Returns its bounds.
fn assert_framed(shot: &rig::Shot, bare_y: Option<usize>) -> (usize, usize, usize, usize) {
    let (x, y, w, h) = panorama_bounds(shot);
    let at = shot.path.display();
    assert_eq!(w, 900 - 190, "the picture fits the content column: {at}");
    assert_eq!(x, (1280 - 900) / 2 + 190, "beside the sidebar: {at}");
    assert!(
        shot.count_near(SIDEBAR_BG, 0) > 10_000,
        "the sidebar is drawn: {at}"
    );
    // The header is 44 high and the picture is centred in the canvas below
    // it, so against the bare viewer its centre drops by half of that.
    if let Some(bare_y) = bare_y {
        let (bare_mid, framed_mid) = (2 * bare_y + 300, 2 * y + h);
        assert!(
            (framed_mid as i64 - bare_mid as i64 - 44).abs() <= 2,
            "the picture should sit centred under the header: {at}"
        );
    }
    (x, y, w, h)
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn a_picture_named_on_the_command_line_opens_bare() {
    let mut rig = Rig::start_on("bare", GRID, panorama, Some("panorama.bmp"));
    let probe = rig.wait("the picture", |p| p.mode == "viewer" && p.shown == Some(0));
    assert!(!probe.framed);

    // The picture spans the window from edge to edge, and nothing but the
    // status bar is drawn beside it: no sidebar, no header.
    let shot = rig.screenshot("bare");
    let (_, bare_y, w, h) = panorama_bounds(&shot);
    assert_eq!(
        w,
        900,
        "the picture should span the window: {}",
        shot.path.display()
    );
    assert_eq!(h, 300);
    assert_eq!(
        shot.count_near(SIDEBAR_BG, 0),
        0,
        "the bare viewer has no sidebar: {}",
        shot.path.display()
    );
    // The panel colour is the status bar's alone in the bare viewer.
    let status_area = shot.count_near(PANEL_BG, 0);

    // B frames it, and B again takes the frame away.
    rig.input.tap(key::B);
    rig.wait("the framed viewer", |p| p.framed);
    rig.drain(Duration::from_millis(200));
    let framed = rig.screenshot("framed");
    assert_framed(&framed, Some(bare_y));
    assert!(
        framed.count_near(PANEL_BG, 0) >= status_area + 600 * 40,
        "the framed viewer has a header the bare one has not: {}",
        framed.path.display()
    );

    rig.input.tap(key::B);
    rig.wait("the bare viewer again", |p| !p.framed);
    rig.drain(Duration::from_millis(200));
    let again = rig.screenshot("bare-again");
    assert_eq!(
        again.difference(&shot),
        0.0,
        "bare again should look exactly as it opened: {}",
        again.path.display()
    );
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn a_picture_opened_from_the_explorer_keeps_the_sidebar() {
    let mut rig = Rig::start("framed", GRID, panorama);
    rig.ready();
    rig.input.tap(key::ENTER);
    let probe = rig.wait("the picture", |p| p.mode == "viewer" && p.shown == Some(0));
    assert!(probe.framed);
    rig.drain(Duration::from_millis(200));
    let shot = rig.screenshot("framed");
    let (x, y, w, h) = assert_framed(&shot, None);

    // Ctrl+wheel zooms about the pointer: the point under it stays under it.
    // Just left of the seam. An anchor that forgot the canvas's origin would
    // sit 190 to the right of the pointer, and the zoom would pull the right
    // half under it.
    let (px, py) = (x + w / 2 - 20, y + h / 2);
    assert!(pixel_is(&shot, px, py, colour_of(7)));
    let before = rig.last.frame;
    rig.input
        .ctrl_wheel_at(px as u32, py as u32, rig::OUTPUT, -1);
    rig.wait("the zoom", |p| p.frame > before);
    rig.drain(Duration::from_millis(200));
    let zoomed = rig.screenshot("zoomed");
    assert!(
        zoomed.difference(&shot) > 0.01,
        "the wheel should have zoomed: {}",
        zoomed.path.display()
    );
    assert!(
        pixel_is(&zoomed, px, py, colour_of(7)),
        "the zoom should keep the left half under the pointer: {}",
        zoomed.path.display()
    );
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn the_thumb_buttons_step_out_of_a_picture_and_back_into_it() {
    // A folder opened straight from the command line: no folder history at
    // all, so stepping back can only mean leaving the picture.
    let mut rig = Rig::start("thumb-viewer", GRID, |dir| small_pictures(dir, 8));
    rig.ready();
    rig.input.tap(key::RIGHT);
    rig.input.tap(key::ENTER);
    rig.wait("the second picture", |p| {
        p.mode == "viewer" && p.shown == Some(1)
    });

    let (x, y) = (640, 400);
    rig.input.click_with(input::BTN_SIDE, x, y, rig::OUTPUT);
    let grid = rig.wait("back to the grid", |p| p.mode == "grid");
    assert_eq!(grid.lead, Some(1), "on the picture just left");
    assert_eq!(grid.forward, 0, "no folder ahead; the picture is");

    rig.input.click_with(input::BTN_EXTRA, x, y, rig::OUTPUT);
    rig.wait("forward into the picture again", |p| {
        p.mode == "viewer" && p.shown == Some(1)
    });

    // The keyboard's Alt+Left is the same step back.
    rig.input.alt(key::LEFT);
    rig.wait("back to the grid by the keyboard", |p| p.mode == "grid");
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn escape_returns_to_the_grid_on_the_picture_last_shown() {
    let mut rig = Rig::start("escape", GRID, |dir| small_pictures(dir, 8));
    rig.ready();
    rig.input.tap(key::RIGHT);
    rig.wait("the cursor on the second picture", |p| p.lead == Some(1));
    rig.input.tap(key::ENTER);
    rig.wait("the second picture", |p| {
        p.mode == "viewer" && p.shown == Some(1)
    });
    rig.input.tap(key::RIGHT);
    rig.input.tap(key::RIGHT);
    rig.wait("the fourth picture", |p| p.shown == Some(3));

    rig.input.tap(key::ESC);
    let back = rig.wait("the grid", |p| p.mode == "grid");
    assert_eq!(
        back.lead,
        Some(3),
        "the cursor should be on the picture last shown"
    );
    assert_quiet(&rig);
}

/// Where the header's buttons are on the output, for a 900 x 600 window
/// centred on it with the sidebar shown: the header starts beside the
/// sidebar, under the title bar, and its buttons are 32 square, 4 apart, 8 in
/// from either end.
mod header_at {
    const HEADER_X: u32 = (1280 - 900) / 2 + 190;
    const HEADER_RIGHT: u32 = (1280 - 900) / 2 + 900;
    /// The middle of the header's height on the output.
    pub const Y: u32 = 73 + 22;

    /// The middle of left-hand button `i` (0 is the first).
    pub fn left(i: u32) -> (u32, u32) {
        (HEADER_X + 8 + i * 36 + 16, Y)
    }

    /// The middle of the viewer's right-hand button `i`, counted from the
    /// right end.
    pub fn right(i: u32) -> (u32, u32) {
        (HEADER_RIGHT - 8 - i * 36 - 16, Y)
    }

    /// The explorer's right-hand buttons stand left of the filter field,
    /// which is 200 wide with 8 either side of it.
    pub fn explorer_right(i: u32) -> (u32, u32) {
        let (x, y) = right(i);
        (x - 208, y)
    }

    /// [`explorer_right`] with the inspector open: the header stops 280 short,
    /// and in the 430 left the field gets what the buttons and the
    /// breadcrumb's 96 leave it, 122.
    pub fn explorer_right_beside_inspector(i: u32) -> (u32, u32) {
        let (x, y) = right(i);
        (x - 280 - 130, y)
    }

    /// The middle of the filter field, with the inspector closed.
    pub fn filter_field() -> (u32, u32) {
        (HEADER_RIGHT - 8 - 100, Y)
    }
}

/// A folder with a subfolder of pictures in it, so there is somewhere to go
/// and come back from.
fn nested(dir: &Path) {
    let sub = dir.join("sub");
    std::fs::create_dir(&sub).expect("make the subfolder");
    small_pictures(&sub, 3);
    small_pictures(dir, 2);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn back_and_forward_retrace_the_way_through_folders() {
    let mut rig = Rig::start("history", GRID, nested);
    let top = rig.ready();
    let folder = rig.folder().to_string_lossy().into_owned();
    let sub = rig.folder().join("sub").to_string_lossy().into_owned();
    assert_eq!((top.back, top.forward), (0, 0));

    // Into the subfolder, which sorts first.
    rig.input.tap(key::ENTER);
    rig.wait("the subfolder", |p| p.dir == sub && p.idle());

    // The header's back button: out again, onto the folder just left.
    let (x, y) = header_at::left(0);
    rig.input.click(x, y, rig::OUTPUT);
    let back = rig.wait("back to the folder", |p| p.dir == folder && p.idle());
    assert_eq!((back.back, back.forward), (0, 1));
    assert_eq!(back.lead, Some(0), "the cursor lands on the subfolder");

    // Alt+Right goes forward again, and Alt+Left back.
    rig.input.alt(key::RIGHT);
    let fwd = rig.wait("forward to the subfolder", |p| p.dir == sub && p.idle());
    assert_eq!((fwd.back, fwd.forward), (1, 0));
    rig.input.alt(key::LEFT);
    rig.wait("back again", |p| {
        p.dir == folder && p.forward == 1 && p.idle()
    });

    // The mouse's thumb buttons do the same, wherever the pointer is.
    let (x, y) = (640, 400);
    rig.input.click_with(input::BTN_EXTRA, x, y, rig::OUTPUT);
    rig.wait("forward by the thumb button", |p| {
        p.dir == sub && p.back == 1 && p.idle()
    });
    rig.input.click_with(input::BTN_SIDE, x, y, rig::OUTPUT);
    rig.wait("back by the thumb button", |p| {
        p.dir == folder && p.forward == 1 && p.idle()
    });

    // Up from the folder is a new step, which drops what was ahead.
    let (x, y) = header_at::left(2);
    rig.input.click(x, y, rig::OUTPUT);
    let up = rig.wait("the parent", |p| p.dir != folder && p.idle());
    assert_eq!((up.back, up.forward), (1, 0));
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn the_layout_button_lights_under_the_pointer_and_switches_the_view() {
    let mut rig = Rig::start("layout-button", GRID, |dir| small_pictures(dir, 4));
    rig.ready();
    // Second from the right: the inspector's toggle is the last.
    let (x, y) = header_at::explorer_right(1);

    let before = rig.screenshot("before-hover");
    let frame = rig.last.frame;
    rig.input.point_at(x, y, rig::OUTPUT);
    rig.wait("the hover to be drawn", |p| p.frame > frame);
    let lit = rig.screenshot("hover");
    assert!(
        before.difference(&lit) > 0.0,
        "the button under the pointer should light up: {}",
        lit.path.display()
    );

    rig.input.click(x, y, rig::OUTPUT);
    rig.wait("the list", |p| p.mode == "list");
    rig.input.click(x, y, rig::OUTPUT);
    rig.wait("the grid again", |p| p.mode == "grid");
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn the_viewer_header_pages_and_closes() {
    let mut rig = Rig::start("viewer-buttons", GRID, |dir| small_pictures(dir, 3));
    rig.ready();
    rig.input.tap(key::ENTER);
    rig.wait("the first picture", |p| {
        p.mode == "viewer" && p.shown == Some(0)
    });

    // Next is the rightmost button, previous beside it.
    let (x, y) = header_at::right(0);
    rig.input.click(x, y, rig::OUTPUT);
    rig.wait("the second picture", |p| p.shown == Some(1));
    let (x, y) = header_at::right(1);
    rig.input.click(x, y, rig::OUTPUT);
    rig.wait("the first again", |p| p.shown == Some(0));

    // The left-hand button goes back to the listing.
    let (x, y) = header_at::left(0);
    rig.input.click(x, y, rig::OUTPUT);
    let back = rig.wait("the grid", |p| p.mode == "grid");
    assert_eq!(back.lead, Some(0));
    assert_quiet(&rig);
}

const LIST: &str = "width=900\nheight=600\ngrid_view=false\n";

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn the_inspector_shows_the_picture_under_the_cursor_and_the_folder_without_one() {
    // The list draws no thumbnails, so a picture's colour on screen can only
    // be the inspector's preview.
    let mut rig = Rig::start("inspector", LIST, |dir| small_pictures(dir, 3));
    let first = rig.ready();
    assert!(!first.inspector, "off by default in a plain folder");
    let off = rig.screenshot("off");
    assert_eq!(off.count_near(colour_of(0), 2), 0);

    rig.input.tap(key::I);
    rig.wait("the inspector", |p| p.inspector && p.idle());
    rig.drain(Duration::from_millis(200));
    let on = rig.screenshot("on");
    assert!(
        on.count_near(colour_of(0), 2) >= DRAWN,
        "the preview should show the picture under the cursor: {}",
        on.path.display()
    );

    // It follows the cursor.
    rig.input.tap(key::DOWN);
    rig.wait("the second picture", |p| p.lead == Some(1) && p.idle());
    rig.drain(Duration::from_millis(200));
    let next = rig.screenshot("next");
    assert!(
        next.count_near(colour_of(1), 2) >= DRAWN,
        "{}",
        next.path.display()
    );
    assert_eq!(next.count_near(colour_of(0), 2), 0);

    // With nothing selected it describes the folder, which has no preview.
    rig.input.tap(key::ESC);
    rig.wait("nothing selected", |p| p.selected == 0);
    rig.drain(Duration::from_millis(200));
    let folder = rig.screenshot("folder");
    assert_eq!(
        folder.count_near(colour_of(1), 2),
        0,
        "{}",
        folder.path.display()
    );

    // The header's toggle hides it, and the choice outlives the window. The
    // header ends at the inspector, so the toggle moves when it goes.
    let (x, y) = header_at::explorer_right_beside_inspector(0);
    rig.input.click(x, y, rig::OUTPUT);
    rig.wait("the inspector hidden", |p| !p.inspector);
    let (x, y) = header_at::explorer_right(0);
    rig.input.click(x, y, rig::OUTPUT);
    rig.wait("the inspector shown again", |p| p.inspector);
    rig.input.tap(key::Q);
    assert!(rig.exit_status().success());
    assert!(
        rig.settings().contains("inspector=true"),
        "{}",
        rig.settings()
    );
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn a_narrowing_window_drops_the_inspector_before_the_sidebar() {
    let mut rig = Rig::start(
        "narrow",
        "width=900\nheight=600\ngrid_view=true\ninspector=true\n",
        |dir| small_pictures(dir, 4),
    );
    let wide = rig.ready();
    assert!(wide.inspector && wide.sidebar);

    // 700 wide: room for the sidebar, not for both.
    rig.input.super_chord(key::N3);
    let mid = rig.wait("700 wide", |p| p.size == (700, 500));
    assert!(!mid.inspector && mid.sidebar, "the inspector goes first");

    // 520 wide: under the sidebar's own threshold too.
    rig.input.super_chord(key::N1);
    let narrow = rig.wait("520 wide", |p| p.size == (520, 400));
    assert!(!narrow.inspector && !narrow.sidebar);

    // Wide again, and both come back: narrowing hid them, it did not turn
    // them off.
    rig.input.super_chord(key::N2);
    let back = rig.wait("1000 wide", |p| p.size == (1000, 640));
    assert!(back.inspector && back.sidebar);
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn a_click_in_the_inspector_beside_a_long_list_goes_to_the_inspector() {
    // Long enough to scroll, so the list has a scrollbar at its right edge,
    // right beside the inspector.
    let mut rig = Rig::start(
        "inspector-click",
        "width=900\nheight=600\ngrid_view=false\ninspector=true\n",
        |dir| small_pictures(dir, 60),
    );
    let ready = rig.ready();
    assert!(ready.inspector);
    // Just inside the inspector, level with the top of the list, where the
    // scrollbar's thumb is.
    let inspector_x = (1280 - 900) / 2 + 900 - 280;
    rig.input.click(inspector_x + 10, 73 + 44 + 20, rig::OUTPUT);
    // Either way the press changes what is drawn: a focus ring, or a grabbed
    // scrollbar, so the next frame says which.
    let after = rig.wait("the click to be drawn", |p| p.frame > ready.frame);
    assert_eq!(
        after.focus, "inspector",
        "the click should land in the inspector"
    );
    assert_eq!(
        after.lead,
        Some(0),
        "and should not scroll or move the list"
    );
    assert_quiet(&rig);
}

/// `count` pictures, landscape 3:2 and portrait 2:3 by turns, each a flat
/// colour: a folder whose shapes differ enough for masonry.
fn mixed_shapes(dir: &Path, count: usize) {
    for i in 0..count {
        let (w, h) = if i % 2 == 0 { (96, 64) } else { (48, 72) };
        let colour = colour_of(i);
        write_bmp(&dir.join(format!("picture-{i:04}.bmp")), w, h, |_, _| {
            colour
        });
    }
}

/// The box picture `i`'s colour fills on screen, as `(x, y, w, h)`.
fn drawn_box(shot: &rig::Shot, i: usize) -> (usize, usize, usize, usize) {
    shot.bounds_of(colour_of(i), 2)
        .unwrap_or_else(|| panic!("picture {i} is not drawn: {}", shot.path.display()))
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn pictures_of_mixed_shapes_lay_out_in_masonry_and_g_changes_it() {
    let mut rig = Rig::start("masonry", GRID, |dir| mixed_shapes(dir, 12));
    let settled = rig.wait("auto to settle on masonry", |p| {
        p.shape == "auto:masonry" && p.idle()
    });
    rig.drain(Duration::from_millis(300));
    let shot = rig.screenshot("masonry");

    // The first two side by side along the top: one width, each at its own
    // height, and larger than the pictures themselves, which a square cell
    // would show at their own size.
    let (x0, y0, w0, h0) = drawn_box(&shot, 0);
    let (x1, y1, w1, h1) = drawn_box(&shot, 1);
    let path = shot.path.display();
    // Each edge is rounded to a whole pixel on its own, so widths may differ
    // by one.
    assert_eq!(y0, y1, "one top: {path}");
    assert!(w0.abs_diff(w1) <= 1, "one width: {path}");
    assert!(w0 > 96, "filled to the column's width: {path}");
    let ratio = |w: usize, h: usize| w as f32 / h as f32;
    assert!((ratio(w0, h0) - 1.5).abs() < 0.05, "landscape kept: {path}");
    assert!(
        (ratio(w1, h1) - 2.0 / 3.0).abs() < 0.05,
        "portrait kept: {path}"
    );
    assert!(x1 > x0 + w0, "in order, left to right: {path}");

    // Down goes down the same column.
    rig.input.tap(key::DOWN);
    let down = rig.wait("the cursor down a step", |p| p.lead != settled.lead);
    let (x_down, y_down, _, _) = drawn_box(&shot, down.lead.expect("a cursor"));
    assert_eq!(x_down, x0, "the same column: {path}");
    assert!(y_down > y0 + h0, "below it: {path}");

    // G: square cells, where each picture is shown at its own size.
    rig.input.tap(key::G);
    rig.wait("square cells", |p| p.shape == "square:square" && p.idle());
    rig.drain(Duration::from_millis(300));
    let shot = rig.screenshot("square");
    let (_, _, w0, h0) = drawn_box(&shot, 0);
    let (_, _, w1, h1) = drawn_box(&shot, 1);
    assert_eq!(
        (w0, h0, w1, h1),
        (96, 64, 48, 72),
        "{}",
        shot.path.display()
    );

    // G again: masonry, chosen rather than settled on, and remembered.
    rig.input.tap(key::G);
    rig.wait("masonry by choice", |p| p.shape == "masonry:masonry");
    rig.input.tap(key::Q);
    assert!(rig.exit_status().success());
    let settings = rig.settings();
    let folder = rig.folder().to_string_lossy().into_owned();
    assert!(
        settings.contains(&format!("shape=masonry {folder}\n")),
        "{settings}"
    );
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn auto_settles_in_a_narrow_column_without_scrolling_and_keeps_the_top() {
    // The inspector leaves the grid two cells wide, so the first screen and
    // the one below hold only a few rows' thumbnails: auto has to judge by
    // those, and settling must not move the view off the first row.
    let mut rig = Rig::start(
        "masonry-narrow",
        "width=900\nheight=600\ngrid_view=true\ninspector=true\n",
        |dir| mixed_shapes(dir, 60),
    );
    let settled = rig.wait("auto to settle without a scroll", |p| {
        p.shape == "auto:masonry" && p.idle()
    });
    assert_eq!(settled.lead, Some(0));
    rig.drain(Duration::from_millis(300));
    let shot = rig.screenshot("settled");
    let (_, y0, _, _) = drawn_box(&shot, 0);
    // The first row sits right under the header (73 + 44), padding apart.
    assert!(
        y0 < 73 + 44 + 40,
        "the first row stays in view: {}",
        shot.path.display()
    );
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn pictures_of_one_shape_stay_in_square_cells() {
    let mut rig = Rig::start("one-shape", GRID, |dir| small_pictures(dir, 12));
    rig.wait("auto to settle on squares", |p| {
        p.shape == "auto:square" && p.idle()
    });
    assert_quiet(&rig);
}

/// A folder of four pictures named for fruit and a folder named like one of
/// them, so a filter has something to tell apart.
fn fruit(dir: &Path) {
    for (i, name) in ["apple", "apricot", "banana", "cherry"].iter().enumerate() {
        let colour = colour_of(i);
        write_bmp(&dir.join(format!("{name}.bmp")), 64, 48, |_, _| colour);
    }
    let archive = dir.join("archive");
    std::fs::create_dir(&archive).expect("make the subfolder");
    small_pictures(&archive, 3);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn typing_in_the_filter_narrows_the_folder_and_escape_brings_it_back() {
    let mut rig = Rig::start("filter", GRID, fruit);
    let all = rig.ready();
    assert_eq!((all.rows, all.filter), (5, false));

    rig.input.tap(key::SLASH);
    rig.wait("the field", |p| p.focus == "filter");
    rig.input.tap(key::A);
    rig.input.tap(key::P);
    let ap = rig.wait("two matches", |p| p.rows == 2);
    assert!(ap.filter);
    assert_eq!(ap.total, 5, "the count still knows the whole folder");
    // The archive folder was selected and is now hidden. Nothing the user
    // did not pick takes its place in the selection.
    assert_eq!((ap.selected, ap.lead), (0, Some(0)));
    rig.screenshot("ap");

    // The field takes the letters that are commands elsewhere: Q is text
    // here, not quit.
    rig.input.tap(key::Q);
    rig.wait("no match for apq", |p| p.rows == 0);
    rig.screenshot("nothing");
    rig.input.tap(key::BACKSPACE);
    rig.wait("ap again", |p| p.rows == 2);

    // Enter goes to the first match, and opening it pages through the
    // matches only.
    rig.input.tap(key::ENTER);
    let entries = rig.wait("the entries", |p| p.focus == "entries");
    assert_eq!(entries.lead, Some(0));
    rig.input.tap(key::ENTER);
    let viewer = rig.wait("apple", |p| p.mode == "viewer" && p.shown == Some(0));
    assert_eq!(viewer.images, 2, "the gallery is what the filter left");
    rig.input.tap(key::RIGHT);
    rig.wait("apricot", |p| p.shown == Some(1));
    rig.input.tap(key::RIGHT);
    rig.wait("round to apple", |p| p.shown == Some(0));

    // Back in the grid the filter is still on. Escape lets go of the
    // selection, then of the filter, and only then would it quit.
    rig.input.tap(key::ESC);
    let grid = rig.wait("the grid", |p| p.mode == "grid");
    assert_eq!((grid.rows, grid.selected), (2, 1));
    rig.input.tap(key::ESC);
    rig.wait("nothing selected", |p| p.selected == 0);
    rig.input.tap(key::ESC);
    let back = rig.wait("every row back", |p| p.rows == 5);
    assert!(!back.filter);
    rig.drain(Duration::from_millis(300));
    assert!(
        rig.last.frame >= back.frame,
        "the window should still be open"
    );
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn a_click_on_the_filter_field_takes_the_keys_and_a_new_folder_drops_the_filter() {
    let mut rig = Rig::start("filter-click", GRID, fruit);
    rig.ready();
    let (x, y) = header_at::filter_field();
    rig.input.click(x, y, rig::OUTPUT);
    rig.wait("the field", |p| p.focus == "filter");
    rig.input.tap(key::A);
    rig.input.tap(key::R);
    let ar = rig.wait("the archive alone", |p| p.rows == 1);
    assert!(ar.filter);

    // Tab leaves the field with the filter still on. The inspector is not
    // shown, so the field is the last pane and Tab comes round to the
    // entries.
    rig.input.tap(key::TAB);
    let tabbed = rig.wait("out of the field", |p| p.focus != "filter");
    assert_eq!((tabbed.focus.as_str(), tabbed.rows), ("entries", 1));

    // Into the folder it found: the filter was about the folder left.
    rig.input.tap(key::ENTER);
    let inside = rig.wait("the archive", |p| p.dir.ends_with("archive") && p.idle());
    assert_eq!((inside.rows, inside.filter), (3, false));
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn the_sidebar_folds_its_sections_and_remembers_a_collection() {
    // A `.bnkfiles/` directory makes the folder a collection.
    let mut rig = Rig::start("sections", GRID, |dir| {
        fruit(dir);
        std::fs::create_dir(dir.join(".bnkfiles")).expect("make the marker");
    });
    let open = rig.wait("the collection seen", |p| p.collections == 1 && p.idle());

    // Tab lands on the row for this folder, the collection; Up is its
    // section's header, and Enter folds it to the header alone.
    rig.input.tap(key::TAB);
    rig.wait("the sidebar", |p| p.focus == "sidebar");
    rig.input.tap(key::UP);
    rig.input.tap(key::ENTER);
    let folded = rig.wait("collections folded", |p| p.places == open.places - 1);
    rig.screenshot("folded");

    // A click on the first row, the Places header, folds Home and
    // Filesystem away (the rig inherits HOME, so both are there).
    let x = (1280 - 900) / 2 + 40;
    rig.input.click(x, 73 + 8 + 10, rig::OUTPUT);
    rig.wait("places folded", |p| p.places == folded.places - 2);
    rig.screenshot("both-folded");

    rig.input.tap(key::Q);
    assert!(rig.exit_status().success());
    let settings = rig.settings();
    assert!(settings.contains("folded=collections,places"), "{settings}");
    let folder = rig.folder().to_string_lossy().into_owned();
    assert!(
        settings.contains(&format!("collection={folder}\n")),
        "{settings}"
    );
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn a_folded_section_stays_folded_and_a_lost_marker_drops_the_collection() {
    // Started with Places folded and this folder remembered as a collection,
    // though it has no marker any more. The rig's folder is at a known place,
    // so the settings can name it before the rig exists.
    let folder = std::env::temp_dir()
        .join(format!(
            "bnkfiles-ui-{}-sections-restored",
            std::process::id()
        ))
        .join("folder");
    let settings = format!(
        "width=900\nheight=600\ngrid_view=true\nfolded=places\ncollection={}\n",
        folder.display()
    );
    let mut rig = Rig::start_on("sections-restored", &settings, fruit, None);
    assert_eq!(rig.folder(), folder);
    let first = rig.ready();
    assert_eq!(first.collections, 0, "the scan found no marker");
    rig.input.tap(key::TAB);
    rig.wait("the sidebar", |p| p.focus == "sidebar");
    // Right on the folded Places header opens it: two more rows.
    rig.input.tap(key::HOME);
    rig.input.tap(key::RIGHT);
    rig.wait("places open", |p| p.places == first.places + 2);
    rig.input.tap(key::Q);
    assert!(rig.exit_status().success());
    let saved = rig.settings();
    assert!(!saved.contains("collection="), "forgotten: {saved}");
    assert!(!saved.contains("folded="), "nothing folded now: {saved}");
    assert_quiet(&rig);
}

/// Three pictures whose names, sizes and ages each put them in a different
/// order: by name a-small, b-big, c-mid; by size small, mid, big; by age
/// small, mid, big, oldest first.
fn sizes(dir: &Path) {
    let day = Duration::from_secs(86_400);
    let now = std::time::SystemTime::now();
    for (i, (name, side, age)) in [("a-small", 20, 3), ("b-big", 200, 1), ("c-mid", 100, 2)]
        .into_iter()
        .enumerate()
    {
        let path = dir.join(format!("{name}.bmp"));
        let colour = colour_of(i);
        write_bmp(&path, side, side, |_, _| colour);
        let file = std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open the fixture");
        file.set_modified(now - day * age)
            .expect("date the fixture");
    }
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn the_list_sorts_by_a_column_from_the_keyboard_or_its_heading() {
    let mut rig = Rig::start("sort", LIST, sizes);
    let first = rig.ready();
    assert_eq!((first.sort.as_str(), first.lead), ("name:asc", Some(0)));
    rig.screenshot("by-name");

    // The cursor is on a-small and stays on it, so where it lands says where
    // a-small sorts. S: by size, biggest first, so a-small is last.
    rig.input.tap(key::S);
    let size = rig.wait("by size", |p| p.sort == "size:desc" && p.idle());
    assert_eq!((size.lead, size.selected), (Some(2), 1));
    rig.screenshot("by-size");
    // Shift+S: the other way, smallest first.
    rig.input.shift(key::S);
    let up = rig.wait("smallest first", |p| p.sort == "size:asc" && p.idle());
    assert_eq!(up.lead, Some(0));

    // The Modified heading: newest first, so the oldest, a-small, is last;
    // a second click turns it round.
    let (x, y) = (1000, 73 + 44 + 14);
    rig.input.click(x, y, rig::OUTPUT);
    let newest = rig.wait("newest first", |p| p.sort == "modified:desc" && p.idle());
    assert_eq!(newest.lead, Some(2));
    rig.input.click(x, y, rig::OUTPUT);
    let oldest = rig.wait("oldest first", |p| p.sort == "modified:asc" && p.idle());
    assert_eq!(oldest.lead, Some(0));

    // Opening a picture pages in the listing's order: oldest first, so the
    // next after a-small is c-mid, shown second.
    rig.input.tap(key::ENTER);
    rig.wait("a-small", |p| p.mode == "viewer" && p.shown == Some(0));
    rig.input.tap(key::RIGHT);
    rig.wait("c-mid", |p| p.shown == Some(1));
    rig.drain(Duration::from_millis(200));
    let shot = rig.screenshot("c-mid");
    assert!(
        shot.count_near(colour_of(2), 2) >= DRAWN,
        "c-mid should be on screen: {}",
        shot.path.display()
    );

    rig.input.tap(key::Q);
    assert!(rig.exit_status().success());
    let saved = rig.settings();
    assert!(
        saved.contains("sort=modified\nsort_descending=false\n"),
        "{saved}"
    );
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn a_click_on_a_heading_beside_the_scrollbar_sorts_rather_than_scrolls() {
    // Long enough to scroll, so the scrollbar's lane runs down the right edge,
    // where the Modified heading ends.
    let mut rig = Rig::start("heading-click", LIST, |dir| small_pictures(dir, 60));
    let ready = rig.ready();
    assert_eq!(ready.sort, "name:asc");
    rig.input.click(1075, 73 + 44 + 14, rig::OUTPUT);
    let after = rig.wait("the click to land", |p| p.frame > ready.frame && p.idle());
    assert_eq!(
        after.sort, "modified:desc",
        "the heading should take the click"
    );
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn ctrl_p_lists_the_commands_and_typing_narrows_them() {
    let mut rig = Rig::start("commands", GRID, |dir| small_pictures(dir, 4));
    rig.ready();
    rig.input.ctrl(key::P);
    let open = rig.wait("the command list", |p| p.menu.is_some());
    assert!(open.menu > Some(10), "{:?}", open.menu);
    rig.screenshot("palette");

    // Escape closes it and does nothing else.
    rig.input.tap(key::ESC);
    let closed = rig.wait("closed", |p| p.menu.is_none());
    assert_eq!((closed.mode.as_str(), closed.selected), ("grid", 1));

    // Typing narrows it. I and S are commands elsewhere, and are text here.
    rig.input.ctrl(key::P);
    rig.wait("open again", |p| p.menu.is_some());
    for k in [key::L, key::I, key::S, key::T] {
        rig.input.tap(k);
    }
    let narrowed = rig.wait("one command", |p| p.menu == Some(1));
    assert!(!narrowed.inspector, "I went into the field");
    assert_eq!(narrowed.sort, "name:asc", "so did S");
    rig.screenshot("narrowed");
    // Enter runs it: list or thumbnails.
    rig.input.tap(key::ENTER);
    let ran = rig.wait("the list", |p| p.menu.is_none() && p.mode == "list");
    assert_eq!(ran.rows, 4);
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn a_right_click_on_a_picture_selects_it_and_offers_what_to_do_with_it() {
    let mut rig = Rig::start("context-menu", GRID, |dir| small_pictures(dir, 4));
    rig.ready();
    rig.drain(Duration::from_millis(200));
    let shot = rig.screenshot("grid");
    let (x, y, w, h) = shot
        .bounds_of(colour_of(2), 2)
        .expect("the third picture is drawn");
    let (cx, cy) = ((x + w / 2) as u32, (y + h / 2) as u32);
    rig.input.right_click(cx, cy, rig::OUTPUT);
    let menu = rig.wait("the context menu", |p| p.menu.is_some());
    assert_eq!(
        (menu.lead, menu.selected),
        (Some(2), 1),
        "it takes the selection"
    );
    rig.screenshot("context-menu");

    // A press outside the list (on the status bar, in the window) closes it
    // and goes no further: the cursor stays on the third picture.
    rig.input.click(250, 655, rig::OUTPUT);
    let closed = rig.wait("closed", |p| p.menu.is_none());
    assert_eq!(closed.lead, Some(2));

    // Opened again, the entry's own commands come first: Enter opens it.
    rig.input.right_click(cx, cy, rig::OUTPUT);
    rig.wait("open again", |p| p.menu.is_some());
    rig.input.tap(key::ENTER);
    rig.wait("the picture", |p| p.mode == "viewer" && p.shown == Some(2));
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn alt_left_with_nowhere_to_go_back_does_not_repeat_as_left() {
    // In the list, a plain Left goes up a folder. Alt+Left with no history
    // goes nowhere, and must not keep repeating as Left once Alt is let go.
    let mut rig = Rig::start("alt-repeat", LIST, |dir| small_pictures(dir, 3));
    let first = rig.ready();
    assert_eq!(first.back, 0);
    rig.input
        .alt_then_hold(key::LEFT, Duration::from_millis(900));
    let after = rig.drain(Duration::from_millis(300));
    assert_eq!(
        after.dir, first.dir,
        "a held Left climbed out of the folder"
    );
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn a_right_click_inside_the_selection_keeps_it_and_acts_on_the_row_clicked() {
    let mut rig = Rig::start("context-selection", GRID, |dir| small_pictures(dir, 4));
    rig.ready();
    rig.drain(Duration::from_millis(200));
    // All four selected, the cursor on the last.
    rig.input.ctrl(key::A);
    rig.wait("all selected", |p| p.selected == 4);
    let shot = rig.screenshot("selected");
    let (x, y, w, h) = shot
        .bounds_of(colour_of(1), 2)
        .expect("the second picture is drawn");
    rig.input
        .right_click((x + w / 2) as u32, (y + h / 2) as u32, rig::OUTPUT);
    let menu = rig.wait("the context menu", |p| p.menu.is_some());
    assert_eq!((menu.selected, menu.lead), (4, Some(1)));
    // Open is first, and opens the picture clicked, not the cursor's old one.
    rig.input.tap(key::ENTER);
    rig.wait("the second picture", |p| {
        p.mode == "viewer" && p.shown == Some(1)
    });
    assert_quiet(&rig);
}

/// The notification line's band and mark for a warning, as drawn: the
/// theme's `NOTICE_WARNING_BG` and `NOTICE_WARNING`.
const WARNING_BAND: [u8; 3] = [0x33, 0x2A, 0x16];
const WARNING_MARK: [u8; 3] = [0xE0, 0xA4, 0x3A];
/// A pinned place's icon, and a key cap, as drawn: `ICON_IMAGE`, `CELL_BG`.
const PIN_ICON: [u8; 3] = [0x79, 0xC0, 0x8A];
const KEY_CAP: [u8; 3] = [0x26, 0x26, 0x2A];
/// Where the sidebar and the content pane are on the output, for a 900 x 600
/// window centred on it: the content starts under the header.
const SIDEBAR: (usize, usize, usize, usize) = ((1280 - 900) / 2, (720 - 600) / 2, 190, 560);
const CONTENT: (usize, usize, usize, usize) =
    ((1280 - 900) / 2 + 190, (720 - 600) / 2 + 44, 710, 500);

/// Three folders with pictures in them, and two pictures beside them, so
/// the one in the middle has a neighbour either side.
fn three_folders(dir: &Path) {
    for name in ["day", "night", "zoo"] {
        let sub = dir.join(name);
        std::fs::create_dir(&sub).expect("make a subfolder");
        small_pictures(&sub, 2);
    }
    small_pictures(dir, 2);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn a_folder_removed_while_open_climbs_to_its_parent_and_says_so() {
    let mut rig = Rig::start_on("climb", GRID, three_folders, Some("night"));
    let folder = rig.folder().to_string_lossy().into_owned();
    let night = rig.folder().join("night");
    let inside = rig.ready();
    assert_eq!(inside.dir, night.to_string_lossy());
    assert_eq!(inside.notice, None);

    std::fs::remove_dir_all(&night).expect("remove the folder on screen");
    let out = rig.wait("the folder above it", |p| p.dir == folder && p.idle());
    assert_eq!(
        out.unreadable, None,
        "it reads the folder above, not the error"
    );
    assert_eq!(out.lead, Some(1), "on zoo, which took night's place");
    assert_eq!(out.back, 0, "climbing out is not a step to go back over");
    assert_eq!(out.notice.as_deref(), Some("warning"), "and it says so");

    let shot = rig.screenshot("climbed");
    assert!(
        shot.count_near(WARNING_BAND, 2) > 5_000,
        "the notification line is tinted as a warning"
    );
    assert!(shot.count_near(WARNING_MARK, 8) > 10, "with its mark");
    assert_quiet(&rig);
}

/// `a` holding `aa`, `b` and `zz`, and `b` holding `c`: a folder in the
/// middle with a folder in it, to take away from under the history.
fn a_tree(dir: &Path) {
    for sub in ["a/aa", "a/b/c", "a/zz"] {
        std::fs::create_dir_all(dir.join(sub)).expect("make the tree");
    }
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn a_folder_that_is_not_there_says_why_and_up_and_f5_find_a_way_on() {
    let mut rig = Rig::start_on("gone", LIST, a_tree, Some("a/b/c"));
    let a = rig.folder().join("a");
    let b = a.join("b");
    let (a_dir, b_dir) = (
        a.to_string_lossy().into_owned(),
        b.to_string_lossy().into_owned(),
    );
    rig.ready();
    rig.input.tap(key::BACKSPACE);
    rig.wait("up to a/b", |p| p.dir == b_dir && p.idle());
    rig.input.tap(key::BACKSPACE);
    rig.wait("up to a", |p| p.dir == a_dir && p.idle());

    // b goes, c and all, while a is on screen: a loses a row, nothing more.
    std::fs::remove_dir_all(&b).expect("remove a/b");
    let fewer = rig.wait("a without b", |p| p.rows == 2 && p.idle());
    assert_eq!((fewer.unreadable, fewer.notice), (None, None));

    // Back is to a/b, which is not there. The pane says so instead of
    // listing nothing, with what can be done about it in key caps.
    rig.input.alt(key::LEFT);
    let gone = rig.wait("a/b, gone", |p| p.dir == b_dir && p.scan_done);
    assert_eq!(gone.unreadable.as_deref(), Some("missing"));
    assert_eq!(gone.rows, 0);
    let shot = rig.screenshot("gone");
    assert!(
        shot.count_near_in(CONTENT, KEY_CAP, 1) > 300,
        "the panel's key caps are drawn"
    );

    // Up goes to the nearest folder that is there, onto where b was.
    rig.input.tap(key::BACKSPACE);
    let up = rig.wait("up to a", |p| p.dir == a_dir && p.idle());
    assert_eq!(up.unreadable, None);
    assert_eq!(up.lead, Some(1), "on zz, which took b's place");

    // Back to it again; it is put back, and F5 reads it.
    rig.input.alt(key::LEFT);
    rig.wait("a/b, gone again", |p| {
        p.dir == b_dir && p.unreadable.is_some() && p.scan_done
    });
    std::fs::create_dir_all(b.join("c")).expect("put a/b back");
    rig.input.tap(key::F5);
    let read = rig.wait("a/b read again", |p| {
        p.dir == b_dir && p.rows == 1 && p.idle()
    });
    assert_eq!(read.unreadable, None);
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn a_pinned_folder_that_goes_away_fades_in_the_sidebar_until_it_is_back() {
    // The rig's folder is at a known place, so the settings can pin a folder
    // in it before the rig exists.
    let folder = std::env::temp_dir()
        .join(format!("bnkfiles-ui-{}-pin-gone", std::process::id()))
        .join("folder");
    let keep = folder.join("keep");
    let settings = format!("{LIST}pin={}\n", keep.display());
    let mut rig = Rig::start_on(
        "pin-gone",
        &settings,
        |dir| {
            std::fs::create_dir(dir.join("keep")).expect("make the pinned folder");
            small_pictures(dir, 2);
        },
        None,
    );
    assert_eq!(rig.folder(), folder);
    let first = rig.ready();
    assert_eq!(first.gone, 0);
    let shot = rig.screenshot("pinned");
    assert!(
        shot.count_near_in(SIDEBAR, PIN_ICON, 3) > 50,
        "the pin's icon is drawn in its colour"
    );

    std::fs::remove_dir(&keep).expect("remove the pinned folder");
    rig.wait("the pin known to be gone", |p| p.gone == 1 && p.rows == 2);
    let shot = rig.screenshot("faded");
    assert_eq!(
        shot.count_near_in(SIDEBAR, PIN_ICON, 3),
        0,
        "the pin's icon is faded"
    );

    std::fs::create_dir(&keep).expect("put the pinned folder back");
    rig.wait("the pin back", |p| p.gone == 0 && p.rows == 3);
    assert_quiet(&rig);
}

#[test]
#[ignore = "needs a headless compositor and a GPU; run with `make test-ui`"]
fn a_picture_whose_folder_goes_stays_in_the_bare_viewer_and_it_says_so() {
    let mut rig = Rig::start_on(
        "viewer-gone",
        GRID,
        |dir| small_pictures(dir, 3),
        Some("picture-0001.bmp"),
    );
    rig.wait("the picture", |p| {
        p.mode == "viewer" && p.shown == Some(1) && p.idle()
    });
    let before = rig.screenshot("shown");
    let drawn = before.count_near(colour_of(1), 2);
    assert!(drawn > 1_000, "the picture is drawn: {drawn}");

    std::fs::remove_dir_all(rig.folder()).expect("remove the picture's folder");
    let after = rig.wait("the warning", |p| p.notice.as_deref() == Some("warning"));
    assert_eq!(after.mode, "viewer", "no climb out from under the picture");
    assert_eq!(after.shown, Some(1));
    assert_eq!(after.unreadable.as_deref(), Some("missing"));
    let shot = rig.screenshot("gone");
    assert!(
        shot.count_near(colour_of(1), 2) * 10 >= drawn * 9,
        "the picture stays up"
    );
    assert!(
        shot.count_near(WARNING_BAND, 2) > 5_000,
        "and the status bar says why"
    );
    assert_quiet(&rig);
}
