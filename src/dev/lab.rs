//! The pipeline lab (`bnkfiles --perf-lab`): end-to-end timings for whole-session
//! goals a microbenchmark cannot answer — how fast a grid of many images fills, how
//! fast one large image lands.
//!
//! Unlike the gate ([`crate::dev::perf`], which times pure functions), these drive
//! the *real* [`Loader`]: its worker pool, its channel, real files decoded off
//! disk. So the number moves when the pipeline's architecture changes (parallel
//! workers, request priority, cancellation) even though no single function got
//! faster.
//!
//! Out of scope, because they are not reproducibly measurable here: the
//! perceived-latency wins (showing a thumbnail before the full image, async GPU
//! upload), which the runtime counters judge instead. What is measured is the
//! decode pipeline with the page cache warm, so the number is the CPU/scheduling
//! cost, not disk seek time (cold I/O is machine-specific noise, and readahead is
//! a separate lever).
//!
//! Wall-clock, single-shot, machine- and load-dependent, so the regression band is
//! wider than the gate's and this is a baseline to improve against rather than a CI
//! tripwire. `--save` accepts the current numbers. Always build `--release`.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use crate::loader::{Class, Job, Loader, Req};
use crate::platform::error::{Error, Result};
use crate::platform::ffi::PollSet;
use crate::render::image::ImageKey;

const BASELINE_PATH: &str = "perf/lab-baseline.txt";

/// Wall-clock end-to-end is noisier than the gate's best-of-N micro timing, so the
/// band is wider: this catches a real slowdown, not run-to-run jitter.
const REGRESSION_RATIO: f64 = 1.15;

/// The grid corpus: many mid-size images, the shape of a photo folder. Identical
/// bytes across the files is fine — each decode is the same real CPU work, which is
/// exactly what a throughput number wants to hold constant.
const GRID_COUNT: usize = 48;
const GRID_DIM: (u32, u32) = (1600, 1200);
/// The one-large-image corpus: a 24-megapixel photo.
const LARGE_DIM: (u32, u32) = (6000, 4000);
/// Thumbnail edge, matching a typical grid cell in device pixels.
const THUMB_SIZE: u32 = 256;

/// Run the lab: build a corpus, time the scenarios against the real loader, then
/// compare to (or establish) the baseline.
pub fn run_lab(save: bool) -> Result<()> {
    eprintln!(
        "bnkfiles perf-lab: end-to-end pipeline timings (build --release for real numbers)..."
    );

    let dir = std::env::temp_dir().join(format!("bnkfiles-lab-{}", std::process::id()));
    let measured = measure_lab(&dir);
    // The corpus is scratch; drop it whether the run succeeded or not.
    let _ = std::fs::remove_dir_all(&dir);
    let samples = measured?;

    let baseline = load_baseline()?;
    println!();
    println!("{:<28} {:>12} {:>9}", "scenario", "time", "vs base");
    let mut regressed = Vec::new();
    for (name, ns) in &samples {
        let (mark, reg) = compare(baseline.get(*name).copied(), *ns);
        if reg {
            regressed.push(*name);
        }
        println!("{:<28} {:>12} {:>9}", name, fmt_time(*ns), mark);
    }

    if save || baseline.is_empty() {
        write_baseline(&samples)?;
        let verb = if baseline.is_empty() {
            "Wrote"
        } else {
            "Updated"
        };
        println!("\n{verb} {BASELINE_PATH}. Re-run after a change to see the delta.");
        return Ok(());
    }
    if regressed.is_empty() {
        println!(
            "\nNo regressions over {:.0}% vs baseline (negatives are speed-ups).",
            (REGRESSION_RATIO - 1.0) * 100.0
        );
        return Ok(());
    }
    Err(Error::msg(format!(
        "perf-lab regression over {:.0}% in: {}. Re-run with --save to accept.",
        (REGRESSION_RATIO - 1.0) * 100.0,
        regressed.join(", ")
    )))
}

/// Build the corpus under `dir`, warm the page cache, and time each scenario.
fn measure_lab(dir: &Path) -> Result<Vec<(&'static str, u64)>> {
    std::fs::create_dir_all(dir)?;

    // Encode each size once, then write the grid image to N files: writing is cheap
    // and the decode work per file is identical either way.
    let grid_png = encode_png(GRID_DIM.0, GRID_DIM.1);
    let mut grid_paths = Vec::with_capacity(GRID_COUNT);
    for i in 0..GRID_COUNT {
        let p = dir.join(format!("grid{i:04}.png"));
        std::fs::write(&p, &grid_png)?;
        grid_paths.push(p);
    }
    let large_path = dir.join("large.png");
    std::fs::write(&large_path, encode_png(LARGE_DIM.0, LARGE_DIM.1))?;

    // Warm the page cache: we are measuring the decode pipeline, not the disk.
    for p in grid_paths.iter().chain(std::iter::once(&large_path)) {
        let _ = std::fs::read(p)?;
    }

    // One pool for the whole lab, as the app has one for its lifetime: creating a
    // fresh pool per scenario would charge the single-image scenario the full
    // thread-startup contention of a pool it barely uses.
    let loader = Loader::new()?;
    let grid = time_pipeline(&loader, |loader| {
        for (i, p) in grid_paths.iter().enumerate() {
            loader.request(Req {
                key: ImageKey::thumb(p, THUMB_SIZE),
                index: i,
                path: p.clone(),
                job: Job::Thumb { size: THUMB_SIZE },
                generation: 0,
                class: Class::Visible,
            });
        }
        grid_paths.len()
    })?;
    let large = time_pipeline(&loader, |loader| {
        loader.request(Req {
            key: ImageKey::full(&large_path),
            index: 0,
            path: large_path.clone(),
            job: Job::Full,
            generation: 0,
            class: Class::Target,
        });
        1
    })?;

    Ok(vec![
        ("grid_thumbs_48_1600x1200", grid),
        ("large_full_6000x4000", large),
    ])
}

/// Drive `loader` through `queue` (which enqueues the requests and returns how many
/// responses to expect), and time from the first request to the last response. The
/// loader is shared across scenarios and idle between them, so this measures the
/// pipeline's steady behaviour, not a cold pool spinning up.
fn time_pipeline(loader: &Loader, queue: impl FnOnce(&Loader) -> usize) -> Result<u64> {
    let t0 = Instant::now();
    let expected = queue(loader);
    drain(loader, expected)?;
    Ok(t0.elapsed().as_nanos() as u64)
}

/// Block until `n` decode responses have come back, waiting on the loader's wake fd
/// exactly as the real event loop does (drain the fd, drain the channel, then poll).
fn drain(loader: &Loader, n: usize) -> Result<()> {
    let mut poll = PollSet::new();
    let mut got = 0usize;
    while got < n {
        loader.drain_wake();
        while loader.try_recv().is_some() {
            got += 1;
        }
        if got >= n {
            break;
        }
        poll.clear();
        poll.add(loader.wake_fd());
        poll.wait(None)?;
    }
    Ok(())
}

/// A synthetic RGBA gradient encoded to a real PNG, so the pipeline runs the actual
/// decode path on real bytes without a committed fixture. (A gradient decodes a
/// touch faster than a noisy photo, so the absolute number is optimistic; the
/// ratio to the baseline, which is what the lab compares, is unaffected.)
fn encode_png(w: u32, h: u32) -> Vec<u8> {
    use zune_core::bit_depth::BitDepth;
    use zune_core::colorspace::ColorSpace;
    use zune_core::options::EncoderOptions;
    use zune_png::PngEncoder;

    let (ww, hh) = (w as usize, h as usize);
    let mut rgba = Vec::with_capacity(ww * hh * 4);
    for y in 0..hh {
        for x in 0..ww {
            rgba.push((x & 0xff) as u8);
            rgba.push((y & 0xff) as u8);
            rgba.push(((x + y) & 0xff) as u8);
            rgba.push(0xff);
        }
    }
    let opts = EncoderOptions::new(ww, hh, ColorSpace::RGBA, BitDepth::Eight);
    let mut sink = Vec::new();
    PngEncoder::new(&rgba, opts)
        .encode(&mut sink)
        .expect("encode lab fixture");
    sink
}

/// A measured time against its baseline: the ratio as a signed percentage (negative
/// is a speed-up), and whether it regressed past [`REGRESSION_RATIO`].
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

fn fmt_time(ns: u64) -> String {
    let ms = ns as f64 / 1e6;
    if ms >= 1000.0 {
        format!("{:.2} s", ms / 1000.0)
    } else {
        format!("{ms:.1} ms")
    }
}

fn load_baseline() -> Result<HashMap<String, u64>> {
    let text = match std::fs::read_to_string(BASELINE_PATH) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(e.into()),
    };
    let mut map = HashMap::new();
    for line in text.lines() {
        let mut f = line.split('\t');
        if let (Some(name), Some(ns)) = (f.next(), f.next()) {
            if let Ok(ns) = ns.parse::<u64>() {
                map.insert(name.to_string(), ns);
            }
        }
    }
    Ok(map)
}

fn write_baseline(samples: &[(&'static str, u64)]) -> Result<()> {
    if let Some(parent) = Path::new(BASELINE_PATH).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = String::new();
    for (name, ns) in samples {
        out.push_str(&format!("{name}\t{ns}\n"));
    }
    std::fs::write(BASELINE_PATH, out)?;
    Ok(())
}
