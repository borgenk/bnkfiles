//! bnkfiles: a file explorer for Wayland with a built-in image viewer.
//!
//! It speaks the Wayland wire protocol directly, with no toolkit, shapes and
//! rasterizes text with the system HarfBuzz and FreeType, and draws with Vulkan
//! into dmabufs it hands to the compositor. The image decoders are its only
//! external crates.
//!
//! Three layers, bottom-up:
//!
//! - `platform`: Wayland, input, fonts, filesystem watching, syscalls.
//! - `render`: the display list, the batcher, the Vulkan backend.
//! - `app` and its leaves (`viewer`, `grid`, `explorer`, `files`, `loader`, ...):
//!   what the program does.
//!
//! `main` picks the mode from the arguments: the performance gate, the pipeline
//! lab, the table generator, the GPU probe, or a window. A folder opens the
//! explorer, an image file the viewer.

mod platform;
mod render;

mod app;
mod config;
mod dev;
mod explorer;
mod files;
mod filter;
mod grid;
mod header;
mod history;
mod inspector;
mod jobs;
mod keymap;
mod loader;
mod localtime;
mod masonry;
mod menu;
mod mime;
mod notice;
mod open;
mod order;
mod places;
mod prompt;
mod regions;
mod scan;
mod select;
mod settings;
mod sidebar;
mod theme;
mod thumbcache;
mod trash;
mod unreadable;
mod usage;
mod viewer;

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

fn main() {
    // As the OS hands them over: a file name is bytes, and a path that is not UTF-8
    // is still a path someone can ask to open.
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let result = if has("--perf-lab") {
        // End-to-end decode-pipeline timings; opens no window (see src/dev/lab.rs).
        dev::lab::run_lab(has("--save"))
    } else if has("--perf") {
        // The offline performance gate; opens no window (see src/dev/perf.rs).
        dev::perf::run(has("--save"))
    } else if has("--gen-tables") {
        dev::gen_tables::run()
    } else if has("--gpu-probe") {
        app::gpu_probe()
    } else {
        // The first non-flag argument is the file, or the folder, to open. It is
        // resolved here, before the Wayland connection: a path that is not there
        // then reports itself once and exits, rather than opening a blank window
        // and failing later, twice, somewhere down in the decoder.
        match args.iter().find(|a| !a.as_bytes().starts_with(b"--")) {
            Some(arg) => files::resolve_arg(Path::new(arg)).and_then(|open| app::run(Some(open))),
            None => app::run(None),
        }
    };
    if let Err(e) = result {
        eprintln!("bnkfiles: {e}");
        std::process::exit(1);
    }
}
