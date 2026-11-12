//! The performance gate (`bnkfiles --perf`): a handful of hot-path microbenchmarks
//! measured CPU-only (no Wayland, no GPU submission) against a committed baseline
//! in `perf/baseline.txt`, so a regression past [`REGRESSION_RATIO`] fails the run.
//!
//! The units a viewer spends its CPU on that are *ours to regress* — this is a gate
//! on our code, not a benchmark of the decoder crates:
//!
//! - **decode** — our in-tree BMP decoder via [`Image::decode`] (format sniff, row
//!   walk, BGR→ARGB): the one decoder we own. The vendored PNG/JPEG/GIF/WebP paths
//!   are a dependency's throughput, an audit-on-version-bump concern, not something
//!   a per-commit gate over our code should measure.
//! - **downscale** — the area filter the thumbnail worker runs
//!   ([`pixel::resample_argb`]), entirely ours, and the heaviest of the three.
//! - **frame batch** — the CPU half of a frame: a display list batched into GPU
//!   vertices against warm atlases ([`gpu::build_frame`]), what every repaint pays.
//! - **thumb cache** — parsing a directory's saved thumbnails back
//!   ([`crate::thumbcache::decode`]), which is what a warm start pays before it can
//!   draw anything. Every length in that file is bounds-checked, so this measures
//!   the validation as much as the copying.
//! - **scan + merge** — a directory read on the scan worker, streamed back in
//!   batches and folded into one sorted listing ([`crate::scan`], [`files::merge_sorted`]).
//!   Wall-clock here spans two threads and includes the kernel's directory read, so
//!   it is indicative rather than precise; the allocation count is the sharp signal,
//!   and it is what catches a per-entry copy creeping into the streaming path.
//!
//! (The per-frame damage diff is deliberately absent: over a viewer's handful of
//! display-list commands it is a sub-microsecond operation, so a ratio gate on it
//! would measure timing noise, not a regression.)
//!
//! Each scenario reports its best-of-N wall-clock time and, built with
//! `--features perf-alloc` (what `make perf` does), the allocations one operation
//! costs — a deterministic, machine-independent signal the gate can trust where
//! wall-clock is only indicative. `--save` accepts the current numbers as the new
//! baseline. Always build `--release`: a debug build's numbers are meaningless and
//! not comparable to a release baseline.
//!
//! The fixtures ride in no committed binary: the decode fixture is a BMP built in
//! code, the downscale source a synthetic ARGB buffer, so the gate carries its own
//! inputs and stays deterministic.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::dev::alloc;
use crate::files;
use crate::order::Order;
use crate::platform::error::{Error, Result};
use crate::platform::ffi::PollSet;
use crate::platform::freetype::{FaceKey, FontStyle, Fonts};
use crate::platform::geom::Rect;
use crate::platform::pixel;
use crate::render::display::DrawCmd;
use crate::render::gpu::{self, GlyphCache};
use crate::render::image::{Image, ImageKey, ImageStore, Limits};
use crate::scan::{Payload, Scanner};
use crate::thumbcache::{self, Cached};

/// Entries in the synthetic directory the scan scenario reads. Big enough to cross
/// the scanner's batch boundary many times (so the streaming and the merge are what
/// is measured, not one batch handed straight over), small enough that building and
/// removing it does not dominate a `make perf` run.
const SCAN_ENTRIES: usize = 10_000;

/// Thumbnails in the cache-load fixture: a well-stocked photo folder.
const THUMB_CACHE_ENTRIES: usize = 1_000;
/// The edge of each, matching the grid's cell size at unity scale.
const THUMB_EDGE: u16 = 160;

/// Where the committed baseline lives, and where each run drops its numbers.
const BASELINE_PATH: &str = "perf/baseline.txt";
const LATEST_PATH: &str = "perf/latest.txt";

/// A scenario counts as a regression when it is at least this much slower (or, for
/// allocations, this much larger) than the baseline. Wide enough to absorb
/// run-to-run timing noise, tight enough to catch the order-of-magnitude
/// regressions the gate exists to prevent.
const REGRESSION_RATIO: f64 = 1.10;

/// Font sizes opened for the frame-batch scenario: the viewer's label, status bar,
/// and browser sizes (`crate::app::FONT_*`), so the batched glyphs are the real
/// ones.
const FONT_SIZES: &[u32] = &[13, 16, 18];

/// One measured scenario: its best time in nanoseconds per operation, and the
/// allocations one operation cost (0 with the counting allocator absent).
struct Sample {
    name: &'static str,
    ns: u64,
    allocs: u64,
}

/// A baseline entry: the recorded time, and allocations when the baseline was saved
/// with the counting allocator on.
#[derive(Clone, Copy)]
struct Base {
    ns: u64,
    allocs: Option<u64>,
}

/// Run the gate: measure, write `latest`, then compare to (or establish) the
/// baseline. Returns an error when a scenario regressed and `--save` was not given,
/// so the process exit code signals the regression to a script.
pub fn run(save: bool) -> Result<()> {
    eprintln!("bnkfiles perf: measuring (build --release for meaningful numbers)...");
    if !alloc::ENABLED {
        eprintln!(
            "  allocations unmeasured; build --features perf-alloc (or `make perf`) to gate them"
        );
    }
    let samples = measure()?;
    write_samples(LATEST_PATH, &samples)?;

    let Some(baseline) = load_baseline(BASELINE_PATH)? else {
        write_samples(BASELINE_PATH, &samples)?;
        print_table(&samples, &HashMap::new());
        println!("\nNo baseline existed; wrote {BASELINE_PATH}. Commit it to compare future runs.");
        return Ok(());
    };

    let regressed = print_table(&samples, &baseline);
    if save {
        write_samples(BASELINE_PATH, &samples)?;
        println!("\nUpdated baseline at {BASELINE_PATH}.");
        return Ok(());
    }
    if regressed.is_empty() {
        println!(
            "\nNo regressions over {:.0}% vs baseline. Latest written to {LATEST_PATH}.",
            (REGRESSION_RATIO - 1.0) * 100.0
        );
        return Ok(());
    }
    Err(Error::msg(format!(
        "perf regression over {:.0}% in: {}. Re-run with --save to accept.",
        (REGRESSION_RATIO - 1.0) * 100.0,
        regressed.join(", ")
    )))
}

/// Measure every scenario against its self-contained fixture.
fn measure() -> Result<Vec<Sample>> {
    let mut out = Vec::new();

    // Decode: our in-tree BMP decoder, the one we own. `Image::decode` sniffs the
    // format and walks the rows (BGR→ARGB, bottom-up flip) with no decoder crate in
    // the path, so this number moves only when our code does.
    let bmp = synth_bmp(1024, 768);
    out.push(sample("decode_bmp_1024x768", 30, || {
        Image::decode(black_box(&bmp), Limits::default()).expect("decode fixture")
    }));

    // Downscale: the area filter the thumbnail worker runs, a 2048x1536 source
    // squeezed into a 320x240 grid cell.
    let (sw, sh) = (2048u32, 1536u32);
    let src = synth_argb(sw, sh);
    out.push(sample("downscale_2048_to_320", 30, || {
        pixel::resample_argb(black_box(&src), sw, sh, 320, 240)
    }));

    // Frame batch: the CPU half of a viewer frame, an image quad under a status bar
    // of text, batched into GPU vertices against warm atlases. No GPU is touched;
    // this is the batcher cost the frame path pays on every repaint.
    let fonts = Fonts::new(FONT_SIZES)?;
    let mut images = ImageStore::new();
    images.insert(ImageKey::full("photo"), synth_image(640, 480));
    let list = frame_list();
    let mut cache = GlyphCache::new();
    // Warm the atlas so the steady state (no glyph or image uploads) is measured.
    let _ = gpu::build_frame(&fonts, &images, &list, &mut cache, &|_| true);
    out.push(sample("frame_batch", 500, || {
        gpu::build_frame(&fonts, &images, &list, &mut cache, &|_| true)
    }));

    // Thumb cache: a warm start's first act. A thousand thumbnails at the grid's
    // cell size is a well-stocked photo folder, and this is the parse the window
    // waits on before it can paint any of them.
    let cache_bytes = synth_cache(THUMB_CACHE_ENTRIES);
    out.push(sample("thumb_cache_load_1k", 10, || {
        thumbcache::decode(black_box(&cache_bytes)).expect("the fixture parses")
    }));

    // Scan + merge: the whole listing path, worker and all. The directory is built
    // once and read repeatedly, so what varies between runs is our streaming and
    // merging rather than the cost of creating ten thousand inodes.
    let scan_dir = synth_dir(SCAN_ENTRIES)?;
    let scanner = Scanner::new(None)?;
    let mut generation = 0u64;
    out.push(sample("scan_merge_10k", 5, || {
        generation += 1;
        scan_to_done(&scanner, &scan_dir, generation)
    }));
    // Leave nothing behind, whatever the scenario did.
    let _ = std::fs::remove_dir_all(&scan_dir);

    Ok(out)
}

/// Read a directory the way the event loop does — block on the scanner's wake fd,
/// drain, merge — and return the size of the listing it produced.
fn scan_to_done(scanner: &Scanner, dir: &Path, generation: u64) -> usize {
    scanner.scan(dir.to_path_buf(), generation, Order::default());
    let mut listing = Vec::new();
    let mut poll = PollSet::new();
    let mut settled = false;
    while !settled {
        poll.clear();
        poll.add(scanner.wake_fd());
        // The timeout is a safety net, not a pacer: a scan that never reports would
        // otherwise hang the gate rather than fail it.
        if poll.wait(Some(Duration::from_secs(10))).is_err() {
            break;
        }
        scanner.drain_wake();
        while let Some(msg) = scanner.try_recv() {
            if msg.generation != generation {
                continue;
            }
            match msg.payload {
                // The gate runs the scanner without a cache, so no thumbnails
                // ever come back; the arm is here so adding a payload variant is
                // a compile error rather than a silently ignored message.
                Payload::Cached(_) => {}
                Payload::Batch(batch) => files::merge_sorted(&mut listing, batch, Order::default()),
                Payload::Done { .. } | Payload::Failed(_) => settled = true,
            }
        }
    }
    listing.len()
}

/// A viewer-representative display list: the image quad, an opaque status bar, and
/// a few text runs across it (so the batch exercises the glyph atlas path).
fn frame_list() -> Vec<DrawCmd> {
    let mut list = vec![
        DrawCmd::Image {
            rect: Rect {
                x: 0,
                y: 0,
                w: 900,
                h: 640,
            },
            key: ImageKey::full("photo"),
            generation: 0,
        },
        DrawCmd::Fill {
            rect: Rect {
                x: 0,
                y: 640,
                w: 900,
                h: 28,
            },
            color: 0xFF1E_1E1E,
        },
    ];
    let face = FaceKey {
        size: 16,
        style: FontStyle::Regular,
    };
    for i in 0..3 {
        let x = 8 + i * 180;
        list.push(DrawCmd::Text {
            bounds: Rect {
                x,
                y: 644,
                w: 176,
                h: 20,
            },
            x,
            baseline: 660,
            face,
            color: 0xFFEC_ECEC,
            text: format!("photo_{i:03}.jpg  ({}/42)", i + 1),
        });
    }
    list
}

// ---- fixtures ------------------------------------------------------------

/// A synthetic ARGB gradient, opaque; the raw pixels a decode would produce, for
/// the downscale source and the frame-batch image.
fn synth_argb(w: u32, h: u32) -> Vec<u32> {
    let (w, h) = (w as usize, h as usize);
    let mut px = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let r = (x & 0xff) as u32;
            let g = (y & 0xff) as u32;
            let b = ((x + y) & 0xff) as u32;
            px.push(0xff00_0000 | (r << 16) | (g << 8) | b);
        }
    }
    px
}

/// A directory's worth of saved thumbnails, in the cache's on-disk form. Built
/// in code like every other fixture here, so the gate carries its own inputs.
fn synth_cache(entries: usize) -> Vec<u8> {
    let (w, h) = (THUMB_EDGE, THUMB_EDGE * 3 / 4);
    let pixels = synth_argb(w as u32, h as u32);
    let packed = thumbcache::pack_pixels(&pixels, w, h).expect("pack the fixture");
    let cached: Vec<Cached> = (0..entries)
        .map(|i| Cached {
            name: format!("img_{i:06}.png").into(),
            mtime_secs: 1_700_000_000 + i as i64,
            size: 1024 * (i as u64 + 1),
            edge: THUMB_EDGE,
            width: w,
            height: h,
            pixels: packed.clone(),
        })
        .collect();
    thumbcache::encode(&cached)
}

/// A synthetic directory for the scan scenario: mostly images, with directories and
/// files of other types mixed through it, so the sort has both groups to order and
/// the projections have something to filter. Names are zero-padded so the natural
/// comparison walks a digit run on every one of them, which is its slow path.
fn synth_dir(entries: usize) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("bnkfiles-perf-scan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    for i in 0..entries {
        match i % 16 {
            0 => std::fs::create_dir(dir.join(format!("dir_{i:06}")))?,
            1 => std::fs::write(dir.join(format!("notes_{i:06}.txt")), b"")?,
            _ => std::fs::write(dir.join(format!("img_{i:06}.png")), b"")?,
        }
    }
    Ok(dir)
}

fn synth_image(w: u32, h: u32) -> Image {
    Image {
        pixels: synth_argb(w, h),
        width: w,
        height: h,
    }
}

/// A 24-bit BMP gradient built in memory: the 54-byte header the in-tree decoder
/// parses, then bottom-up BGR rows padded to a 4-byte stride. No encoder, so the
/// decode scenario runs purely our code on real BMP bytes without a committed file.
fn synth_bmp(w: u32, h: u32) -> Vec<u8> {
    let (ww, hh) = (w as usize, h as usize);
    let stride = (ww * 3 + 3) & !3;
    let data_size = stride * hh;
    let mut b = Vec::with_capacity(54 + data_size);
    b.extend_from_slice(b"BM");
    b.extend_from_slice(&((54 + data_size) as u32).to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes()); // reserved
    b.extend_from_slice(&54u32.to_le_bytes()); // pixel data offset
    b.extend_from_slice(&40u32.to_le_bytes()); // header size (BITMAPINFOHEADER)
    b.extend_from_slice(&(w as i32).to_le_bytes());
    b.extend_from_slice(&(h as i32).to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // planes
    b.extend_from_slice(&24u16.to_le_bytes()); // bits per pixel
    b.extend_from_slice(&0u32.to_le_bytes()); // compression = BI_RGB
    b.extend_from_slice(&(data_size as u32).to_le_bytes());
    b.extend_from_slice(&2835i32.to_le_bytes()); // x pixels/meter
    b.extend_from_slice(&2835i32.to_le_bytes()); // y pixels/meter
    b.extend_from_slice(&0u32.to_le_bytes()); // colors used
    b.extend_from_slice(&0u32.to_le_bytes()); // important colors
    for y in 0..hh {
        let row_start = b.len();
        for x in 0..ww {
            b.push(((x + y) & 0xff) as u8); // blue
            b.push((y & 0xff) as u8); // green
            b.push((x & 0xff) as u8); // red
        }
        b.resize(row_start + stride, 0); // pad the row to the 4-byte stride
    }
    b
}

// ---- measurement ---------------------------------------------------------

/// Measure one scenario: warm it, take a deterministic allocation count over one
/// operation, then the best per-operation time over five rounds. Best-of, not mean:
/// the fastest round is the one least perturbed by scheduling and interrupts.
fn sample<T>(name: &'static str, iters: u64, mut f: impl FnMut() -> T) -> Sample {
    for _ in 0..iters.min(8) {
        black_box(f());
    }
    alloc::reset();
    black_box(f());
    let allocs = alloc::snapshot();

    let mut best = u64::MAX;
    for _ in 0..5 {
        let start = Instant::now();
        for _ in 0..iters.max(1) {
            black_box(f());
        }
        best = best.min(start.elapsed().as_nanos() as u64 / iters.max(1));
    }
    Sample {
        name,
        ns: best,
        allocs,
    }
}

// ---- reporting -----------------------------------------------------------

/// Print the table and return the names that regressed (empty when none did).
fn print_table(samples: &[Sample], baseline: &HashMap<String, Base>) -> Vec<&'static str> {
    let alloc_col = alloc::ENABLED;
    println!();
    if alloc_col {
        println!(
            "{:<24} {:>12} {:>8}   {:>10} {:>8}",
            "scenario", "time", "vs base", "allocs", "vs base"
        );
    } else {
        println!("{:<24} {:>12} {:>8}", "scenario", "time", "vs base");
    }

    let mut regressed = Vec::new();
    for s in samples {
        let base = baseline.get(s.name);
        let (tmark, treg) = compare(base.map(|b| b.ns), s.ns);
        let mut line = format!("{:<24} {:>12} {:>8}", s.name, fmt_ns(s.ns), tmark);
        if alloc_col {
            let (amark, areg) = compare(base.and_then(|b| b.allocs), s.allocs);
            write!(line, "   {:>10} {:>8}", s.allocs, amark).ok();
            if areg {
                regressed.push(s.name);
            }
        }
        if treg {
            regressed.push(s.name);
        }
        println!("{line}");
    }
    regressed.sort_unstable();
    regressed.dedup();
    regressed
}

/// A measured value against its baseline: the ratio as a signed percentage, and
/// whether it counts as a regression (at least [`REGRESSION_RATIO`] worse).
fn compare(base: Option<u64>, now: u64) -> (String, bool) {
    match base {
        None | Some(0) => ("—".to_string(), false),
        Some(b) => {
            let ratio = now as f64 / b as f64;
            (
                format!("{:+.0}%", (ratio - 1.0) * 100.0),
                ratio >= REGRESSION_RATIO,
            )
        }
    }
}

fn fmt_ns(ns: u64) -> String {
    if ns >= 1_000_000 {
        format!("{:.2} ms", ns as f64 / 1e6)
    } else if ns >= 1_000 {
        format!("{:.2} us", ns as f64 / 1e3)
    } else {
        format!("{ns} ns")
    }
}

fn write_samples(path: &str, samples: &[Sample]) -> Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = String::new();
    for s in samples {
        writeln!(out, "{}\t{}\t{}", s.name, s.ns, s.allocs).ok();
    }
    std::fs::write(path, out)?;
    Ok(())
}

/// Load the baseline, or `None` when the file does not exist yet (the first run
/// establishes it). Lines are `name\tns\tallocs`; a missing allocs field means the
/// baseline was saved without the counting allocator, so those go uncompared.
fn load_baseline(path: &str) -> Result<Option<HashMap<String, Base>>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut map = HashMap::new();
    for line in text.lines() {
        let mut f = line.split('\t');
        let (Some(name), Some(ns)) = (f.next(), f.next()) else {
            continue;
        };
        let Ok(ns) = ns.parse::<u64>() else {
            continue;
        };
        let allocs = f.next().and_then(|a| a.parse::<u64>().ok());
        map.insert(name.to_string(), Base { ns, allocs });
    }
    Ok(Some(map))
}
