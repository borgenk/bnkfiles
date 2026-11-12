//! Development tooling, off the viewer's runtime path.
//!
//! Two measurement tools and a generator:
//!
//! - `perf` (`--perf`) — the gate: hot-path microbenchmarks (our BMP decode,
//!   thumbnail downscale, frame batching) against a committed baseline, reporting
//!   allocations through the counting allocator (`alloc`) under `perf-alloc`. It
//!   guards against a pure function getting slower.
//! - `lab` (`--perf-lab`) — end-to-end pipeline timings (grid fill, large-image
//!   load) against the real `Loader`, for the architectural wins (parallel decode,
//!   request priority) that a microbenchmark cannot see.
//! - `gen_tables` (`--gen-tables`) — rewrites the Unicode grapheme tables from
//!   the data vendored in `ucd/`.
//!
//! All are reached only via their CLI flags; the shipping viewer never calls into
//! any of it.

pub(crate) mod alloc;
pub(crate) mod gen_tables;
pub(crate) mod lab;
pub(crate) mod perf;
