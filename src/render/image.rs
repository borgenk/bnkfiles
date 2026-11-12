//! Decoding image files into a flat ARGB8888 buffer, and the store the renderer
//! looks images up in. This is the single seam over the external decoders: the
//! rest of the app speaks only [`Image`] and never imports a decoder crate, so a
//! backend can be swapped by rewriting this one file.
//!
//! Decoders parse hostile, attacker-controlled bytes, so this is the one place
//! the app leans on memory-safe third-party code rather than an in-tree decoder.
//! The exception is BMP, which is a header and a pixel array (no compression, no
//! entropy coding) and has no maintained decoder worth a dependency.
//!
//! The format is sniffed from the leading bytes rather than the file extension,
//! which is both more robust (a mislabelled `.png` that is really a JPEG still
//! opens) and the safer default. [`Limits`] guards against decompression bombs:
//! the declared dimensions are read from the header and refused before a pixel
//! buffer is allocated.
//!
//! **Alpha is straight, not premultiplied.** The Vulkan pipeline blends
//! source-over with `SRC_ALPHA`/`ONE_MINUS_SRC_ALPHA` (see
//! `render::vulkan::renderer`), so a texture's colour channels must be
//! independent of its alpha. Every decoder funnels through [`pack`], which is the
//! one place that convention is established.

use std::collections::HashMap;
use std::ffi::c_int;
use std::fmt;
use std::io::{Cursor, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use zune_core::options::DecoderOptions;

use crate::platform::error::{Error, Result};

/// A decoded image: straight-alpha ARGB8888 pixels (`0xAARRGGBB`) plus
/// dimensions. One heap allocation, uploaded to a texture without further copying.
pub struct Image {
    pub pixels: Vec<u32>,
    pub width: u32,
    pub height: u32,
}

/// Caps applied before decoding, to bound work and memory on hostile or malformed
/// input. An image whose declared dimensions exceed these is rejected before its
/// pixel buffer is allocated.
#[derive(Clone, Copy)]
pub struct Limits {
    pub max_width: u32,
    pub max_height: u32,
}

impl Default for Limits {
    fn default() -> Self {
        // 16384 square: the zune decoders' own backstop, and the usual Vulkan
        // `maxImageDimension2D`, so an image that decodes is an image that can
        // become a texture. A viewer is handed real photographs and the odd
        // panorama, so the cap has to be generous; past this it is a
        // decompression bomb or a mistake, not a picture.
        Self {
            max_width: 16384,
            max_height: 16384,
        }
    }
}

/// The largest encoded file that will be read for decoding.
///
/// The decoders work on the whole file in memory, so something has to bound the
/// file before it is read: nothing about a name ending in `.png` says the file is
/// a picture, or small. A 16384x16384 image (the dimension cap in [`Limits`]) is
/// about a gigabyte of raw pixels and compresses to a fraction of that; real
/// photographs, including camera originals and large panoramas, come in well
/// under a hundred megabytes. 256 MiB leaves all of them room and still bounds
/// what a single decode worker can be made to hold by a file that is only
/// pretending.
pub const MAX_ENCODED_BYTES: u64 = 256 * 1024 * 1024;

/// Return from `open` at once rather than waiting for the other end of a FIFO.
/// (The value on every architecture this runs on; only alpha, mips and sparc
/// differ.)
const O_NONBLOCK: c_int = 0o4_000;

/// Never let an opened terminal become this process's controlling one.
const O_NOCTTY: c_int = 0o400;

/// Why an image file's bytes were not read.
#[derive(Debug)]
pub enum ReadError {
    /// A FIFO, a device, a directory, a socket: something with a picture's name
    /// that is not a file with a picture's bytes in it.
    NotRegular,
    /// More than [`MAX_ENCODED_BYTES`].
    TooLarge,
    Io(std::io::Error),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::NotRegular => f.write_str("not a regular file"),
            ReadError::TooLarge => write!(
                f,
                "too large (over {} MiB)",
                MAX_ENCODED_BYTES / (1024 * 1024)
            ),
            ReadError::Io(e) => e.fmt(f),
        }
    }
}

/// Read the encoded bytes of the image file at `path`, refusing anything that is
/// not a regular file of a sane size.
///
/// The one way file bytes reach a decoder. A directory listing is hostile input
/// as much as the bytes are: `x.png` can be a symlink to `/dev/zero`, which reads
/// forever; a FIFO, whose `open` blocks until somebody writes to it, taking a
/// decode worker out of the pool for good; or simply eight gigabytes long. So:
///
/// - the open cannot block (`O_NONBLOCK` returns from a writerless FIFO at once),
/// - what was opened is examined through the descriptor, so it is the thing that
///   will be read and not whatever the path names a moment later,
/// - and the read stops one byte past the cap, so a file growing underneath it is
///   caught rather than followed.
pub fn read_encoded(path: &Path) -> std::result::Result<Vec<u8>, ReadError> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK | O_NOCTTY)
        .open(path)
        .map_err(ReadError::Io)?;
    let meta = file.metadata().map_err(ReadError::Io)?;
    if !meta.is_file() {
        return Err(ReadError::NotRegular);
    }
    if meta.len() > MAX_ENCODED_BYTES {
        return Err(ReadError::TooLarge);
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(MAX_ENCODED_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(ReadError::Io)?;
    if bytes.len() as u64 > MAX_ENCODED_BYTES {
        return Err(ReadError::TooLarge);
    }
    Ok(bytes)
}

impl Image {
    /// The decoded size in bytes: four (ARGB) per pixel. What a resident image costs
    /// in memory, for the pipeline's memory high-water counter.
    pub fn byte_len(&self) -> usize {
        self.pixels.len() * 4
    }

    /// Read and decode the file at `path`.
    pub fn load(path: &Path) -> Result<Image> {
        let bytes = read_encoded(path).map_err(|e| {
            Error::msg(format!(
                "could not read {}: {e}",
                path.file_name()
                    .unwrap_or(path.as_os_str())
                    .to_string_lossy()
            ))
        })?;
        Image::decode(&bytes, Limits::default())
    }

    /// The image's declared dimensions, read from its header only, without decoding
    /// the pixels. The decode pool uses this to size a memory reservation *before*
    /// allocating the full buffer, so a hostile or huge image is bounded up front
    /// rather than after it has already been decoded. `None` if the format is
    /// unrecognised or its header does not parse.
    pub fn probe(bytes: &[u8]) -> Option<(u32, u32)> {
        match Format::sniff(bytes)? {
            Format::Png => {
                // IHDR is always the first chunk, and its first two fields are the
                // dimensions: [8 signature][4 length][4 "IHDR"][4 width BE][4 height
                // BE]. Reading them directly is instant; going through the decoder's
                // `decode_headers` is not (it is tens of milliseconds on a large PNG).
                let w = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
                let h = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
                (w > 0 && h > 0).then_some((w, h))
            }
            Format::Jpeg => {
                let mut d = zune_jpeg::JpegDecoder::new(Cursor::new(bytes));
                d.decode_headers().ok()?;
                d.dimensions().map(|(w, h)| (w as u32, h as u32))
            }
            Format::Bmp => {
                let h: &[u8; BMP_HEADER] = bytes.get(..BMP_HEADER)?.try_into().ok()?;
                let w = i32::from_le_bytes([h[18], h[19], h[20], h[21]]);
                let ht = i32::from_le_bytes([h[22], h[23], h[24], h[25]]);
                (w > 0 && ht != 0).then(|| (w.unsigned_abs(), ht.unsigned_abs()))
            }
            Format::Gif => {
                let d = gif::DecodeOptions::new()
                    .read_info(Cursor::new(bytes))
                    .ok()?;
                Some((d.width() as u32, d.height() as u32))
            }
            Format::Webp => image_webp::WebPDecoder::new(Cursor::new(bytes))
                .ok()
                .map(|d| d.dimensions()),
        }
    }

    /// The most memory decoding `bytes` holds at once, estimated from the header.
    ///
    /// What the decode pool reserves before it lets a decode start. The finished
    /// image is four bytes a pixel, but that is where a decode ends up, not where
    /// it peaks: every path holds the encoded file, the decoder's own output, and
    /// for a moment both that output and the packed pixels made from it. Sized to
    /// the finished image alone, the reservation under-counts each worker several
    /// times over.
    ///
    /// Per pixel, on top of the encoded bytes:
    ///
    /// ```text
    ///   PNG    inflated scanlines + decoder output, each channels x sample bytes
    ///          (16 for 16-bit RGBA), plus a second copy of the compressed data,
    ///          and a second output buffer if interlaced; then output + 4 packed
    ///   JPEG   4 RGBA out + 4 packed; a file whose scans do not each carry
    ///          every component (all progressive ones) also holds every
    ///          coefficient until the last scan (up to 8) alongside the output
    ///   GIF    4 screen canvas + 4 packed, plus 4 per pixel of the frame
    ///   WebP   4 RGBA out + 4 packed, plus the decoder's canvas, frame, alpha
    ///          and YUV planes for an animated or alpha file (up to 8 more)
    ///   BMP    4: rows are packed straight from the file
    /// ```
    ///
    /// An upper bound per format rather than an exact trace, erring high. `None`
    /// if the format is unrecognised or its header does not parse, in which case
    /// the decode fails before allocating anything worth reserving for.
    pub fn decode_peak(bytes: &[u8]) -> Option<usize> {
        let format = Format::sniff(bytes)?;
        let (w, h) = Image::probe(bytes)?;
        let (w, h) = (w as usize, h as usize);
        let pixels = w.saturating_mul(h);
        let transient = match format {
            Format::Png => {
                // IHDR again: [8 signature][4 length][4 "IHDR"][4 w][4 h][1 bit
                // depth][1 colour type][1 compression][1 filter][1 interlace].
                let depth = *bytes.get(24)?;
                let colour = *bytes.get(25)?;
                // Adam7 passes are assembled into a second full-size buffer
                // while the first is still held.
                let outputs = if *bytes.get(28)? == 0 { 1 } else { 2 };
                // Channels after the alpha channel `decode_png` asks for is added:
                // grey becomes grey + alpha, everything else RGBA.
                let channels = if matches!(colour, 0 | 4) { 2 } else { 4 };
                // The decoder inflates and unfilters at the file's own depth even
                // when asked to hand back 8 bits, so 16-bit samples cost two
                // bytes each until the very end.
                let sample = if depth == 16 { 2 } else { 1 };
                let output = pixels.saturating_mul(channels * sample);
                // Scanlines as inflated: the samples plus a filter byte per row.
                let inflated = output.saturating_add(h);
                let decoding = bytes
                    .len()
                    .saturating_add(inflated)
                    .saturating_add(output.saturating_mul(outputs));
                let packing = output.saturating_add(pixels.saturating_mul(4));
                decoding.max(packing)
            }
            Format::Jpeg if jpeg_holds_coefficients(bytes) => pixels.saturating_mul(12),
            Format::Jpeg => pixels.saturating_mul(8),
            Format::Gif => {
                let frame = gif_first_frame_pixels(bytes).unwrap_or(pixels);
                pixels
                    .saturating_mul(8)
                    .saturating_add(frame.saturating_mul(4))
            }
            Format::Webp => pixels.saturating_mul(16),
            Format::Bmp => pixels.saturating_mul(4),
        };
        Some(bytes.len().saturating_add(transient))
    }

    /// Decode `bytes`, sniffing the format from its leading magic number.
    pub fn decode(bytes: &[u8], limits: Limits) -> Result<Image> {
        match Format::sniff(bytes) {
            Some(Format::Png) => decode_png(bytes, limits),
            Some(Format::Jpeg) => decode_jpeg(bytes, limits),
            Some(Format::Bmp) => decode_bmp(bytes, limits),
            Some(Format::Gif) => decode_gif(bytes, limits),
            Some(Format::Webp) => decode_webp(bytes, limits),
            None => Err(Error::msg("unsupported or unrecognised image format")),
        }
    }
}

/// The formats decoded here, identified by their leading bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Format {
    Png,
    Jpeg,
    Bmp,
    Gif,
    Webp,
}

impl Format {
    fn sniff(bytes: &[u8]) -> Option<Format> {
        const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF];
        if bytes.starts_with(PNG) {
            Some(Format::Png)
        } else if bytes.starts_with(JPEG) {
            Some(Format::Jpeg)
        } else if bytes.starts_with(b"BM") {
            Some(Format::Bmp)
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some(Format::Gif)
        // RIFF....WEBP: the size field between the two tags is not part of the
        // signature, so both ends are checked.
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
            Some(Format::Webp)
        } else {
            None
        }
    }

    /// The MIME type, for putting the source file on the clipboard.
    fn mime(self) -> &'static str {
        match self {
            Format::Png => "image/png",
            Format::Jpeg => "image/jpeg",
            Format::Bmp => "image/bmp",
            Format::Gif => "image/gif",
            Format::Webp => "image/webp",
        }
    }
}

/// Whether decoding a JPEG keeps every coefficient of the image in memory,
/// read from its frame and scan headers.
///
/// A JPEG whose first scan carries all of its components decodes a row of blocks
/// at a time. One that spreads the image over several scans (every progressive
/// file, and a baseline one written a component per scan) cannot produce a pixel
/// until the last scan is in, so the decoder holds the whole image as 16-bit
/// coefficients meanwhile: the difference between the two reservations in
/// [`Image::decode_peak`]. Anything this cannot make sense of is answered `true`,
/// the larger reservation, since the decoder is more forgiving than this is.
fn jpeg_holds_coefficients(bytes: &[u8]) -> bool {
    // Segments after the two-byte start marker: 0xFF, a code, and for most codes
    // a big-endian length that counts itself.
    let mut at = 2usize;
    let mut frame_components = None;
    loop {
        let (Some(&lead), Some(&code)) = (bytes.get(at), bytes.get(at + 1)) else {
            return true;
        };
        // Junk between segments, or a fill byte before the real marker. The
        // decoder steps over both, so this has to as well.
        if lead != 0xFF || code == 0xFF {
            at += 1;
            continue;
        }
        match code {
            // The progressive start-of-frame markers.
            0xC2 | 0xC6 | 0xCA | 0xCE => return true,
            // The end of the image with no scan seen.
            0xD9 => return true,
            // Markers that stand alone, with no length after them.
            0x00 | 0x01 | 0xD0..=0xD8 => {
                at += 2;
                continue;
            }
            _ => {}
        }
        let (Some(&hi), Some(&lo)) = (bytes.get(at + 2), bytes.get(at + 3)) else {
            return true;
        };
        match code {
            // Baseline frame header: [2 marker][2 length][1 precision][2 height]
            // [2 width][1 component count].
            0xC0 | 0xC1 => frame_components = bytes.get(at + 9).copied(),
            // The first scan: [2 marker][2 length][1 component count].
            0xDA => {
                return match (frame_components, bytes.get(at + 4)) {
                    (Some(frame), Some(&scan)) => scan < frame,
                    _ => true,
                }
            }
            _ => {}
        }
        // The length is at least its own two bytes, so this always advances.
        at += 2 + usize::from(u16::from_be_bytes([hi, lo])).max(2);
    }
}

/// The pixel count of a GIF's first frame, which is sized independently of the
/// screen it is drawn on and can be far larger than it.
fn gif_first_frame_pixels(bytes: &[u8]) -> Option<usize> {
    let mut decoder = gif::DecodeOptions::new()
        .read_info(Cursor::new(bytes))
        .ok()?;
    let frame = decoder.next_frame_info().ok()??;
    Some(usize::from(frame.width) * usize::from(frame.height))
}

/// Reject a zero or over-limit image up front, so [`pack`] and the decoders only
/// ever see dimensions we are willing to allocate a buffer for.
fn check_dimensions(w: usize, h: usize, limits: Limits) -> Result<()> {
    if w == 0 || h == 0 {
        return Err(Error::msg("image has a zero dimension"));
    }
    if w > limits.max_width as usize || h > limits.max_height as usize {
        return Err(Error::msg(format!(
            "image {w}x{h} exceeds the {}x{} limit",
            limits.max_width, limits.max_height
        )));
    }
    Ok(())
}

fn decode_png(bytes: &[u8], limits: Limits) -> Result<Image> {
    use zune_core::result::DecodingResult;
    use zune_png::PngDecoder;

    // `add_alpha` keeps palette/RGB transparency (tRNS) by emitting an alpha
    // channel rather than dropping it; `pack` then handles whatever channel
    // count comes out. `strip_to_8bit` has the decoder hand back the high byte of
    // each 16-bit sample, in place: the renderer draws at 8 bits, and narrowing
    // here would mean a second full-size buffer beside the 16-bit one.
    let opts = DecoderOptions::default()
        .png_set_add_alpha_channel(true)
        .png_set_strip_to_8bit(true);
    let mut decoder = PngDecoder::new_with_options(Cursor::new(bytes), opts);
    decoder
        .decode_headers()
        .map_err(|_| Error::msg("invalid png"))?;
    let (w, h) = decoder
        .dimensions()
        .ok_or_else(|| Error::msg("png has no dimensions"))?;
    check_dimensions(w, h, limits)?;
    let pixels = match decoder
        .decode()
        .map_err(|_| Error::msg("png decode failed"))?
    {
        DecodingResult::U8(px) => px,
        _ => return Err(Error::msg("unsupported png sample type")),
    };
    pack(pixels, w as u32, h as u32)
}

fn decode_jpeg(bytes: &[u8], limits: Limits) -> Result<Image> {
    use zune_core::colorspace::ColorSpace;
    use zune_jpeg::JpegDecoder;

    // Ask zune for RGBA so it does the YCbCr/CMYK -> RGB conversion (the gnarly,
    // security-sensitive part) internally and we always pack a 4-channel buffer.
    let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut decoder = JpegDecoder::new_with_options(Cursor::new(bytes), opts);
    decoder
        .decode_headers()
        .map_err(|_| Error::msg("invalid jpeg"))?;
    let (w, h) = decoder
        .dimensions()
        .ok_or_else(|| Error::msg("jpeg has no dimensions"))?;
    check_dimensions(w, h, limits)?;
    let pixels = decoder
        .decode()
        .map_err(|_| Error::msg("jpeg decode failed"))?;
    pack(pixels, w as u32, h as u32)
}

/// Bytes of BMP header parsed: the 14-byte file header + a 40-byte
/// BITMAPINFOHEADER.
const BMP_HEADER: usize = 54;

/// Decode an uncompressed (BI_RGB) 24- or 32-bit BMP. That covers essentially
/// every BMP a viewer meets; palette, RLE, and 16-bit variants are rejected rather
/// than mis-decoded. Rows are bottom-up unless the height is negative.
///
/// The one decoder written in-tree, because a BMP is a header followed by the
/// pixels: no compression, no entropy coding, and so none of the parsing surface
/// that makes an image decoder worth a dependency.
fn decode_bmp(bytes: &[u8], limits: Limits) -> Result<Image> {
    // Take the header as a fixed-size array, so the field reads below cannot run
    // off the end of a truncated file.
    let header: &[u8; BMP_HEADER] = bytes
        .get(..BMP_HEADER)
        .and_then(|h| h.try_into().ok())
        .ok_or_else(|| Error::msg("bmp header truncated"))?;
    let u16le = |i: usize| u16::from_le_bytes([header[i], header[i + 1]]);
    let u32le =
        |i: usize| u32::from_le_bytes([header[i], header[i + 1], header[i + 2], header[i + 3]]);
    let i32le =
        |i: usize| i32::from_le_bytes([header[i], header[i + 1], header[i + 2], header[i + 3]]);

    let data_offset = u32le(10) as usize;
    let width = i32le(18);
    let height = i32le(22);
    let bpp = u16le(28);
    let compression = u32le(30);
    if compression != 0 {
        return Err(Error::msg("unsupported bmp (compressed)"));
    }
    if bpp != 24 && bpp != 32 {
        return Err(Error::msg("unsupported bmp bit depth (24/32 only)"));
    }
    if width <= 0 || height == 0 {
        return Err(Error::msg("bmp has a zero dimension"));
    }
    // A BMP's dimensions come straight off the wire with no decoder of its own to
    // sanity-check them, so they meet the same cap as everything else here before
    // anything is allocated.
    let (w, h) = (
        width.unsigned_abs() as usize,
        height.unsigned_abs() as usize,
    );
    check_dimensions(w, h, limits)?;

    let bottom_up = height > 0;
    let bpx = (bpp / 8) as usize;
    // Each pixel row is padded up to a multiple of four bytes.
    let stride = (w * bpx + 3) & !3;
    // Validate the file actually holds the pixel data, guarding against overflow.
    let span = stride
        .checked_mul(h)
        .and_then(|s| data_offset.checked_add(s))
        .ok_or_else(|| Error::msg("bmp dimensions too large"))?;
    if bytes.len() < span {
        return Err(Error::msg("bmp pixel data truncated"));
    }

    let mut pixels = Vec::with_capacity(w * h);
    for y in 0..h {
        // Bottom-up files store the last row first; emit top-to-bottom either way.
        let src_row = if bottom_up { h - 1 - y } else { y };
        let row = data_offset + src_row * stride;
        for x in 0..w {
            let p = row + x * bpx;
            // BMP stores BGR(X); the 4th byte of a BI_RGB 32-bit pixel is padding,
            // not alpha, so every pixel is opaque rather than risking a fully
            // transparent image.
            pixels.push(argb(255, bytes[p + 2], bytes[p + 1], bytes[p]));
        }
    }
    Ok(Image {
        pixels,
        width: w as u32,
        height: h as u32,
    })
}

/// Decode the first frame of a GIF. Animation is not played; the first frame is
/// shown as a still.
///
/// A GIF is a logical screen plus a sequence of sub-images that patch it, and the
/// first frame need not cover the whole screen: it has its own size and a `(left,
/// top)` offset into the screen. So the frame is composited where it belongs on a
/// transparent canvas of the *screen's* dimensions, rather than shown as a
/// bare image the frame's own size, which would misplace and mis-size any GIF whose
/// first frame is smaller than or offset within its screen.
fn decode_gif(bytes: &[u8], limits: Limits) -> Result<Image> {
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    let mut decoder = options
        .read_info(Cursor::new(bytes))
        .map_err(|_| Error::msg("invalid gif"))?;
    let (cw, ch) = (decoder.width() as usize, decoder.height() as usize);
    check_dimensions(cw, ch, limits)?;
    let frame = decoder
        .read_next_frame()
        .map_err(|_| Error::msg("gif decode failed"))?
        .ok_or_else(|| Error::msg("gif has no frames"))?;

    let (fw, fh) = (frame.width as usize, frame.height as usize);
    let (fx, fy) = (frame.left as usize, frame.top as usize);
    // The transparent screen, straight-alpha RGBA (every byte zero: alpha 0).
    let mut canvas = vec![0u8; cw * ch * 4];
    let row_bytes = fw * 4;
    for row in 0..fh {
        let dy = fy + row;
        if dy >= ch {
            break;
        }
        // Clip a frame that overhangs the right edge (a malformed offset/size); a
        // frame within the screen copies its full width.
        let copy_w = fw.min(cw.saturating_sub(fx));
        if copy_w == 0 {
            continue;
        }
        let src_off = row * row_bytes;
        // A frame buffer shorter than its declared size stops the copy rather than
        // indexing past its end.
        let Some(src) = frame.buffer.get(src_off..src_off + copy_w * 4) else {
            break;
        };
        // `dy < ch` and `fx + copy_w <= cw`, so this destination range is in bounds.
        let dst_off = (dy * cw + fx) * 4;
        canvas[dst_off..dst_off + copy_w * 4].copy_from_slice(src);
    }
    pack(canvas, cw as u32, ch as u32)
}

fn decode_webp(bytes: &[u8], limits: Limits) -> Result<Image> {
    let mut decoder =
        image_webp::WebPDecoder::new(Cursor::new(bytes)).map_err(|_| Error::msg("invalid webp"))?;
    let (w, h) = decoder.dimensions();
    check_dimensions(w as usize, h as usize, limits)?;
    let size = decoder
        .output_buffer_size()
        .ok_or_else(|| Error::msg("webp dimensions too large"))?;
    let mut buf = vec![0u8; size];
    decoder
        .read_image(&mut buf)
        .map_err(|_| Error::msg("webp decode failed"))?;
    // read_image fills RGB8 or RGBA8 (per has_alpha); pack infers from length.
    pack(buf, w, h)
}

/// Fold decoded bytes into ARGB8888 `u32`s, inferring the channel count from the
/// buffer length so grayscale (1), grayscale+alpha (2), RGB (3), and RGBA (4) all
/// convert correctly. The single normalisation point: every decoder funnels here,
/// so the rest of the app only ever meets straight-alpha ARGB (see the module
/// docs for why straight and not premultiplied).
fn pack(bytes: Vec<u8>, w: u32, h: u32) -> Result<Image> {
    let count = (w as usize) * (h as usize);
    if count == 0 {
        return Err(Error::msg("image has a zero dimension"));
    }
    if bytes.is_empty() || !bytes.len().is_multiple_of(count) {
        return Err(Error::msg("decoded image size does not match dimensions"));
    }
    let channels = bytes.len() / count;
    let mut pixels = Vec::with_capacity(count);
    match channels {
        4 => pixels.extend(
            bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|p| argb(p[3], p[0], p[1], p[2])),
        ),
        3 => pixels.extend(
            bytes
                .as_chunks::<3>()
                .0
                .iter()
                .map(|p| argb(255, p[0], p[1], p[2])),
        ),
        2 => pixels.extend(
            bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|p| argb(p[1], p[0], p[0], p[0])),
        ),
        1 => pixels.extend(bytes.iter().map(|&g| argb(255, g, g, g))),
        _ => return Err(Error::msg("unexpected channel count")),
    }
    Ok(Image {
        pixels,
        width: w,
        height: h,
    })
}

const fn argb(a: u8, r: u8, g: u8, b: u8) -> u32 {
    ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

/// Which decode of a file an image is: the picture itself, or a thumbnail of it
/// fitted to a square of the given edge.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ImageSize {
    Full,
    Thumb(u32),
}

/// Names one image in the [`ImageStore`]: a file, and which decode of it.
///
/// The path is kept as the filesystem gave it, so two files whose names differ
/// only in bytes that are not UTF-8 stay two images. The size is part of the key
/// so one picture can be resident as both the grid's thumbnail and the viewer's
/// full decode, and so a thumbnail decoded for an old scale cannot land under the
/// current one.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ImageKey {
    pub path: PathBuf,
    pub size: ImageSize,
}

impl ImageKey {
    pub fn full(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            size: ImageSize::Full,
        }
    }

    pub fn thumb(path: impl Into<PathBuf>, edge: u32) -> Self {
        Self {
            path: path.into(),
            size: ImageSize::Thumb(edge),
        }
    }
}

struct Resident {
    image: Image,
    /// Which insert put the image here. A key names a file, and the file can be
    /// rewritten: the generation is what tells the renderer that the pixels under
    /// a key it already has a texture for are no longer the same pixels.
    generation: u64,
}

/// The images the renderer can draw, by the key the display list names them with
/// ([`DrawCmd::Image`](crate::render::display::DrawCmd::Image)).
///
/// A pure cache: it decodes nothing itself. Decoding a photograph takes tens to
/// hundreds of milliseconds, which is why the app does it on a worker thread and
/// [`insert`](Self::insert)s the result — a store that decoded on lookup would do
/// it on the frame path, stalling the very loop that is trying to draw.
#[derive(Default)]
pub struct ImageStore {
    images: HashMap<ImageKey, Resident>,
    /// The generation the next insert takes. Never reset, so an image put back
    /// after the store was emptied is still told apart from the one before.
    next_generation: u64,
}

impl ImageStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adopt a decoded image under `key`, replacing any image already there.
    pub fn insert(&mut self, key: ImageKey, image: Image) {
        let generation = self.next_generation;
        self.next_generation += 1;
        self.images.insert(key, Resident { image, generation });
    }

    /// The image under `key`, if one is resident.
    pub fn get(&self, key: &ImageKey) -> Option<&Image> {
        self.images.get(key).map(|resident| &resident.image)
    }

    /// Which insert the image under `key` came from, if one is resident.
    pub fn generation(&self, key: &ImageKey) -> Option<u64> {
        self.images.get(key).map(|resident| resident.generation)
    }

    pub fn contains(&self, key: &ImageKey) -> bool {
        self.images.contains_key(key)
    }

    /// Drop every image whose key `keep` rejects. This is the eviction the caller
    /// needs to bound memory: a folder of 4K photographs is gigabytes decoded, so
    /// only what is on screen (and its immediate neighbours) stays resident.
    pub fn retain(&mut self, keep: impl Fn(&ImageKey) -> bool) {
        self.images.retain(|k, _| keep(k));
    }

    pub fn clear(&mut self) {
        self.images.clear();
    }

    pub fn len(&self) -> usize {
        self.images.len()
    }

    /// Total decoded bytes resident across every image, for the memory high-water in
    /// the pipeline stats. A folder of 4K photos is gigabytes decoded, so this is the
    /// number eviction exists to bound.
    pub fn bytes(&self) -> usize {
        self.images
            .values()
            .map(|resident| resident.image.byte_len())
            .sum()
    }
}

/// The bytes of an image file and the MIME type of its format, for putting the
/// picture on the clipboard. The file as-is (lossless), not a re-encode of the
/// decoded pixels, so no encoder is needed. `None` if it cannot be read (see
/// [`read_encoded`] for what is refused) or is not a format we recognise.
pub fn read_source(path: &Path) -> Option<(Vec<u8>, &'static str)> {
    let bytes = read_encoded(path).ok()?;
    let mime = Format::sniff(&bytes)?.mime();
    Some((bytes, mime))
}

#[cfg(test)]
mod tests {
    use crate::render::image::*;
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    // Real, tiny fixtures generated offline (no external sources): a 2x2 RGBA PNG
    // and a 16x16 solid-red JPEG. See src/render/testdata.
    const TINY_PNG: &[u8] = include_bytes!("testdata/tiny.png");
    const TINY_JPEG: &[u8] = include_bytes!("testdata/tiny.jpg");

    /// Build a valid uncompressed 24-bit BMP from per-row BGR bytes (rows given in
    /// BMP file order: bottom-up when `height` is positive). Pads each row to a
    /// 4-byte boundary, the layout `decode_bmp` parses.
    fn build_bmp24(width: i32, height: i32, rows_bgr: &[&[u8]]) -> Vec<u8> {
        let w = width.unsigned_abs() as usize;
        let stride = (w * 3 + 3) & !3;
        let data_size = stride * rows_bgr.len();
        let mut b = Vec::new();
        b.extend_from_slice(b"BM");
        b.extend_from_slice(&((54 + data_size) as u32).to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes()); // reserved
        b.extend_from_slice(&54u32.to_le_bytes()); // pixel data offset
        b.extend_from_slice(&40u32.to_le_bytes()); // header size
        b.extend_from_slice(&width.to_le_bytes());
        b.extend_from_slice(&height.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes()); // planes
        b.extend_from_slice(&24u16.to_le_bytes()); // bits per pixel
        b.extend_from_slice(&0u32.to_le_bytes()); // compression = BI_RGB
        b.extend_from_slice(&(data_size as u32).to_le_bytes());
        b.extend_from_slice(&2835i32.to_le_bytes()); // x pixels/meter
        b.extend_from_slice(&2835i32.to_le_bytes()); // y pixels/meter
        b.extend_from_slice(&0u32.to_le_bytes()); // colors used
        b.extend_from_slice(&0u32.to_le_bytes()); // important colors
        for row in rows_bgr {
            b.extend_from_slice(row);
            b.resize(b.len() + (stride - row.len()), 0); // row padding
        }
        b
    }

    /// A 4x4-logical-screen GIF whose only frame is a 2x2 opaque-red block placed at
    /// offset (1, 1): the case where the first frame neither fills nor starts at the
    /// origin of the screen.
    fn build_gif_offset_frame() -> Vec<u8> {
        use std::borrow::Cow;
        let mut out = Vec::new();
        let frame = gif::Frame {
            left: 1,
            top: 1,
            width: 2,
            height: 2,
            // A two-entry palette (power-of-two): red at index 0, and a filler.
            palette: Some(vec![255, 0, 0, 0, 0, 0]),
            buffer: Cow::Owned(vec![0u8; 4]), // 2x2, every pixel index 0 (red)
            ..Default::default()
        };
        {
            let mut encoder = gif::Encoder::new(&mut out, 4, 4, &[]).expect("gif encoder");
            encoder.write_frame(&frame).expect("write gif frame");
        }
        out
    }

    #[test]
    fn gif_first_frame_is_placed_on_the_logical_screen() {
        let img = Image::decode(&build_gif_offset_frame(), Limits::default()).expect("decode gif");
        // The image is the logical screen's size, not the 2x2 frame's.
        assert_eq!((img.width, img.height), (4, 4));
        let at = |x: u32, y: u32| img.pixels[(y * 4 + x) as usize];
        // The frame's 2x2 block sits at (1, 1), opaque red.
        for (x, y) in [(1, 1), (2, 1), (1, 2), (2, 2)] {
            assert_eq!(at(x, y), 0xFFFF_0000, "frame pixel ({x},{y})");
        }
        // Everywhere the frame does not cover is transparent (straight alpha, a = 0).
        assert_eq!(at(0, 0) >> 24, 0, "corner outside the frame is transparent");
        assert_eq!(at(3, 3) >> 24, 0, "corner outside the frame is transparent");
    }

    #[test]
    fn probe_reads_dimensions_without_decoding() {
        // Header-only dimensions, for the decode pool's memory reservation. Every
        // format the app decodes must answer, and junk must answer `None` rather than
        // index off the end of a short header.
        assert_eq!(Image::probe(TINY_PNG), Some((2, 2)));
        assert_eq!(Image::probe(TINY_JPEG), Some((16, 16)));
        assert_eq!(Image::probe(&build_bmp24(3, 5, &[])), Some((3, 5)));
        assert_eq!(Image::probe(&build_gif_offset_frame()), Some((4, 4)));
        assert_eq!(Image::probe(b"not an image"), None);
        assert_eq!(Image::probe(&[]), None);
        // A truncated PNG (past the signature but short of IHDR's fields) is refused,
        // not read past its end.
        assert_eq!(Image::probe(&TINY_PNG[..18]), None);
    }

    #[test]
    fn sniffs_every_supported_format_and_rejects_others() {
        assert_eq!(Format::sniff(TINY_PNG), Some(Format::Png));
        assert_eq!(Format::sniff(TINY_JPEG), Some(Format::Jpeg));
        assert_eq!(
            Format::sniff(&build_bmp24(1, 1, &[&[0, 0, 255]])),
            Some(Format::Bmp)
        );
        assert_eq!(Format::sniff(b"GIF89a....."), Some(Format::Gif));
        assert_eq!(Format::sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some(Format::Webp));
        // A RIFF container that is not WEBP (a .wav) is not an image.
        assert_eq!(Format::sniff(b"RIFF\0\0\0\0WAVEfmt "), None);
        assert!(Format::sniff(b"not an image at all").is_none());
        assert!(Format::sniff(&[]).is_none());
    }

    #[test]
    fn the_extension_is_not_trusted() {
        // Content decides: PNG bytes decode as a PNG whatever the file is called.
        let img = Image::decode(TINY_PNG, Limits::default()).expect("decode png");
        assert_eq!((img.width, img.height), (2, 2));
    }

    #[test]
    fn decodes_png_keeping_straight_alpha() {
        let img = Image::decode(TINY_PNG, Limits::default()).expect("decode png");
        assert_eq!(img.pixels.len(), 4);
        // The fixture carries a transparent pixel: its alpha must survive, and its
        // colour must be independent of it (straight, not premultiplied, which is
        // what the pipeline blends).
        assert!(
            img.pixels.iter().any(|p| p >> 24 != 0xFF),
            "fixture should carry transparency"
        );
    }

    #[test]
    fn decodes_jpeg_to_opaque_argb() {
        let img = Image::decode(TINY_JPEG, Limits::default()).expect("decode jpeg");
        assert_eq!((img.width, img.height), (16, 16));
        assert!(img.pixels.iter().all(|p| p >> 24 == 0xFF), "jpeg is opaque");
    }

    #[test]
    fn decodes_bmp_reading_rows_bottom_up() {
        // 1x1 red, stored BGR on disk: comes out opaque red, not blue, so the
        // channel order is right.
        let img = Image::decode(&build_bmp24(1, 1, &[&[0, 0, 255]]), Limits::default())
            .expect("decode bmp");
        assert_eq!(img.pixels, vec![0xFFFF_0000]);

        // Positive height = bottom-up: file row 0 is the image's BOTTOM row, so
        // disk order red-then-blue decodes to blue (top) then red (bottom).
        let bmp = build_bmp24(1, 2, &[&[0, 0, 255], &[255, 0, 0]]);
        let img = Image::decode(&bmp, Limits::default()).expect("decode bmp");
        assert_eq!(img.pixels, vec![0xFF00_00FF, 0xFFFF_0000]);

        // Negative height = top-down: file order is already top-to-bottom.
        let bmp = build_bmp24(1, -2, &[&[0, 0, 255], &[255, 0, 0]]);
        let img = Image::decode(&bmp, Limits::default()).expect("decode bmp");
        assert_eq!(img.pixels, vec![0xFFFF_0000, 0xFF00_00FF]);
    }

    #[test]
    fn rejects_a_bmp_it_cannot_decode_rather_than_guessing() {
        // Bit depth 16: rejected, not mis-decoded.
        let mut bmp = build_bmp24(1, 1, &[&[0, 0, 255]]);
        bmp[28] = 16;
        assert!(Image::decode(&bmp, Limits::default()).is_err());

        // Every prefix short of the full header errors rather than indexing past
        // the end of the buffer.
        let bmp = build_bmp24(1, 1, &[&[0, 0, 255]]);
        for cut in 0..BMP_HEADER {
            assert!(
                Image::decode(&bmp[..cut], Limits::default()).is_err(),
                "{cut}-byte prefix decoded"
            );
        }
    }

    #[test]
    fn rejects_an_image_over_the_limit_before_allocating() {
        // A BMP header claiming 100000 x 100000 is a 40 GB allocation and a pixel
        // count past i32; it is refused on the header.
        let mut bmp = build_bmp24(1, 1, &[&[0, 0, 255]]);
        bmp[18..22].copy_from_slice(&100_000i32.to_le_bytes());
        bmp[22..26].copy_from_slice(&100_000i32.to_le_bytes());
        assert!(Image::decode(&bmp, Limits::default()).is_err());

        // And the limit is honoured for the crate-backed decoders too.
        let tiny = Limits {
            max_width: 1,
            max_height: 1,
        };
        assert!(Image::decode(TINY_PNG, tiny).is_err());
        assert!(Image::decode(TINY_JPEG, tiny).is_err());
    }

    #[test]
    fn rejects_unrecognised_bytes() {
        assert!(Image::decode(b"not an image", Limits::default()).is_err());
        assert!(Image::decode(&[], Limits::default()).is_err());
    }

    #[test]
    fn pack_infers_channels_from_length() {
        // Grayscale, one byte per pixel, expands across the colour channels.
        let img = pack(vec![128, 64], 2, 1).expect("pack gray");
        assert_eq!(img.pixels, vec![0xFF80_8080, 0xFF40_4040]);
        // RGB is opaque; RGBA keeps its alpha, and its colour beside it.
        assert_eq!(
            pack(vec![255, 0, 0], 1, 1).expect("rgb").pixels[0],
            0xFFFF_0000
        );
        assert_eq!(
            pack(vec![10, 20, 30, 40], 1, 1).expect("rgba").pixels[0],
            argb(40, 10, 20, 30)
        );
    }

    #[test]
    fn pack_rejects_a_size_that_does_not_match_the_dimensions() {
        // 5 bytes cannot be a whole number of channels over 2 pixels.
        assert!(pack(vec![1, 2, 3, 4, 5], 2, 1).is_err());
        assert!(pack(vec![], 1, 1).is_err());
        assert!(pack(vec![1, 2, 3], 0, 1).is_err());
    }

    #[test]
    fn the_store_holds_what_the_app_puts_in_it() {
        let mut store = ImageStore::new();
        assert_eq!(store.len(), 0);
        let (full, thumb) = (
            ImageKey::full("photo.png"),
            ImageKey::thumb("photo.png", 96),
        );
        store.insert(full.clone(), pack(vec![1, 2, 3], 1, 1).expect("pack"));
        store.insert(thumb.clone(), pack(vec![9, 9, 9], 1, 1).expect("pack"));
        // The same picture is resident at two sizes under two keys, which is what
        // lets the grid's cell and the viewer's canvas hold it as two textures.
        assert_eq!(store.len(), 2);
        assert!(store.get(&full).is_some());
        assert!(store.contains(&thumb));
        assert!(store.get(&ImageKey::full("absent.png")).is_none());
        // A thumbnail at another scale is another image.
        assert!(!store.contains(&ImageKey::thumb("photo.png", 192)));

        store.retain(|k| k.size == ImageSize::Full);
        assert_eq!(store.len(), 1);
        assert!(store.get(&thumb).is_none());
    }

    #[test]
    fn names_that_differ_only_in_invalid_utf8_are_different_images() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        // Both print the same, with a replacement character for the bad byte.
        // Keyed by that, one would be drawn as the other.
        let one = ImageKey::full(OsStr::from_bytes(b"a\xff.png"));
        let other = ImageKey::full(OsStr::from_bytes(b"a\xfe.png"));
        let mut store = ImageStore::new();
        store.insert(one.clone(), pack(vec![1, 2, 3], 1, 1).expect("pack"));
        assert!(store.contains(&one));
        assert!(!store.contains(&other));
    }

    #[test]
    fn a_replaced_image_has_a_new_generation() {
        // What tells the renderer its texture for this key is stale: a file
        // rewritten at the same size, or decoded again after the store was emptied.
        let key = ImageKey::full("photo.png");
        let mut store = ImageStore::new();
        store.insert(key.clone(), pack(vec![1, 2, 3], 1, 1).expect("pack"));
        let first = store.generation(&key).expect("resident");
        store.insert(key.clone(), pack(vec![4, 5, 6], 1, 1).expect("pack"));
        let second = store.generation(&key).expect("resident");
        assert_ne!(first, second);

        store.clear();
        assert_eq!(store.generation(&key), None);
        store.insert(key.clone(), pack(vec![7, 8, 9], 1, 1).expect("pack"));
        let third = store.generation(&key).expect("resident");
        assert!(third != first && third != second);
    }

    #[test]
    fn store_bytes_sum_the_resident_pixels() {
        // Four bytes (ARGB) per pixel, summed across every resident image: the
        // memory high-water the pipeline stats report and eviction bounds.
        let mut store = ImageStore::new();
        assert_eq!(store.bytes(), 0);
        let (a, b) = (ImageKey::full("a"), ImageKey::full("b"));
        store.insert(a, pack(vec![1, 2, 3, 4, 5, 6], 2, 1).expect("2px"));
        assert_eq!(store.bytes(), 2 * 4);
        store.insert(b.clone(), pack(vec![9, 9, 9], 1, 1).expect("1px"));
        assert_eq!(store.bytes(), 3 * 4);
        store.retain(|k| *k == b);
        assert_eq!(store.bytes(), 4); // one 1x1 image survives: 4 bytes
    }

    /// A unique temp directory for one test, removed by the test that made it.
    fn temp_dir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "bnkfiles_image_{tag}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("make temp dir");
        base
    }

    extern "C" {
        fn mkfifo(path: *const std::ffi::c_char, mode: u32) -> c_int;
    }

    #[test]
    fn a_pipe_with_a_picture_s_name_is_refused_at_once() {
        // `mkfifo x.jpg`: a plain open waits for a writer that never comes, and
        // each one takes a decode worker with it until the pool is gone.
        let dir = temp_dir("fifo");
        let pipe = dir.join("x.jpg");
        let c = std::ffi::CString::new(pipe.as_os_str().as_bytes()).expect("path");
        // SAFETY: `c` is a NUL-terminated path that outlives the call.
        assert_eq!(unsafe { mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo");

        let started = Instant::now();
        assert!(matches!(read_encoded(&pipe), Err(ReadError::NotRegular)));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "it did not wait"
        );
        assert!(Image::load(&pipe)
            .err()
            .is_some_and(|e| e.to_string().contains("not a regular file")));
        assert!(read_source(&pipe).is_none());

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_symlink_to_a_device_is_refused_without_reading_it() {
        // `ln -s /dev/zero x.png` reads zeros until the machine is out of memory.
        let dir = temp_dir("devzero");
        let link = dir.join("x.png");
        std::os::unix::fs::symlink("/dev/zero", &link).expect("symlink");

        assert!(matches!(read_encoded(&link), Err(ReadError::NotRegular)));
        // A directory is not a file of pixels either.
        assert!(matches!(read_encoded(&dir), Err(ReadError::NotRegular)));

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_file_over_the_size_cap_is_refused_and_one_at_it_is_read() {
        // `truncate -s 8G a.png`. Sparse, so making it costs nothing; refused on
        // its length, so reading it never starts.
        let dir = temp_dir("huge");
        let huge = dir.join("a.png");
        let file = std::fs::File::create(&huge).expect("create");
        file.set_len(MAX_ENCODED_BYTES + 1).expect("size");
        drop(file);
        let started = Instant::now();
        assert!(matches!(read_encoded(&huge), Err(ReadError::TooLarge)));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "refused from its size, not after reading a quarter of a gigabyte"
        );

        // An ordinary file still reads whole.
        let small = dir.join("tiny.png");
        std::fs::write(&small, TINY_PNG).expect("file");
        assert_eq!(read_encoded(&small).expect("read"), TINY_PNG);

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xEDB8_8320 & (!(crc & 1)).wrapping_add(1));
            }
        }
        !crc
    }

    fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let crc = crc32(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    }

    /// The signature and IHDR of a PNG: all a header-only estimate reads.
    fn png_header(width: u32, height: u32, depth: u8, colour: u8) -> Vec<u8> {
        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[depth, colour, 0, 0, 0]);
        png_chunk(&mut out, b"IHDR", &ihdr);
        out
    }

    /// A whole 16-bit RGBA PNG of one row, its pixel data in a stored (that is,
    /// uncompressed) deflate block so no compressor is needed to build it.
    fn build_png16_rgba(row: &[[u16; 4]]) -> Vec<u8> {
        let mut out = png_header(row.len() as u32, 1, 16, 6);
        let mut raw = vec![0u8]; // filter type: none
        for sample in row.iter().flatten() {
            raw.extend_from_slice(&sample.to_be_bytes());
        }
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in &raw {
            a = (a + u32::from(byte)) % 65521;
            b = (b + a) % 65521;
        }
        let mut zlib = vec![0x78, 0x01, 0x01];
        zlib.extend_from_slice(&(raw.len() as u16).to_le_bytes());
        zlib.extend_from_slice(&(!(raw.len() as u16)).to_le_bytes());
        zlib.extend_from_slice(&raw);
        zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());
        png_chunk(&mut out, b"IDAT", &zlib);
        png_chunk(&mut out, b"IEND", &[]);
        out
    }

    #[test]
    fn a_16_bit_png_decodes_to_the_high_byte_of_each_sample() {
        // The decoder narrows 16-bit samples itself. Taking the wrong byte of
        // each pair would still produce a picture, only a noisy one.
        let png = build_png16_rgba(&[
            [0x1234, 0x5678, 0x9ABC, 0xFFFF],
            [0xFF00, 0x00FF, 0x8001, 0x80FF],
        ]);
        let img = Image::decode(&png, Limits::default()).expect("decode 16-bit png");
        assert_eq!((img.width, img.height), (2, 1));
        assert_eq!(img.pixels, vec![0xFF12_569A, 0x80FF_0080]);
    }

    #[test]
    fn the_reservation_for_a_png_covers_its_real_peak_not_its_final_size() {
        // The largest image the limits allow, 16 bits a channel. Its finished
        // pixels are a gigabyte; decoding it holds the inflated scanlines and the
        // decoder's output at once, eight bytes a pixel each. A reservation sized
        // to the finished image under-counts that fourfold.
        let (w, h) = (16384usize, 16384usize);
        let deep = png_header(w as u32, h as u32, 16, 6);
        let scanlines = h * (1 + w * 8);
        let output = w * h * 8;
        assert!(
            Image::decode_peak(&deep).expect("an estimate") >= scanlines + output,
            "16-bit RGBA"
        );

        // The same picture at 8 bits: half the transient, still twice the result.
        let shallow = png_header(w as u32, h as u32, 8, 6);
        let estimate = Image::decode_peak(&shallow).expect("an estimate");
        assert!(estimate >= h * (1 + w * 4) + w * h * 4, "8-bit RGBA");
        assert!(estimate < Image::decode_peak(&deep).expect("an estimate"));

        // Grey with alpha added is two channels out, then four packed beside it.
        let grey = png_header(1000, 1000, 8, 0);
        assert!(Image::decode_peak(&grey).expect("an estimate") >= 1000 * 1000 * (2 + 4));
    }

    #[test]
    fn every_format_reserves_at_least_its_output_and_its_packed_pixels() {
        // Each decoder's output and the packed copy made from it exist together,
        // however briefly; BMP alone packs straight from the file.
        let jpeg = Image::decode_peak(TINY_JPEG).expect("jpeg");
        assert!(jpeg >= TINY_JPEG.len() + 16 * 16 * 8);
        let gif_bytes = build_gif_offset_frame();
        let gif = Image::decode_peak(&gif_bytes).expect("gif");
        assert!(
            gif >= gif_bytes.len() + 4 * 4 * 8 + 2 * 2 * 4,
            "screen, packed, frame"
        );
        let bmp_bytes = build_bmp24(3, 5, &[]);
        assert_eq!(
            Image::decode_peak(&bmp_bytes),
            Some(bmp_bytes.len() + 3 * 5 * 4)
        );
        assert_eq!(Image::decode_peak(b"not an image"), None);
    }

    #[test]
    fn an_interlaced_png_reserves_for_its_second_output_buffer() {
        let mut plain = png_header(1000, 1000, 8, 6);
        let flat = Image::decode_peak(&plain).expect("an estimate");
        // The interlace method is the last byte of IHDR's data, before its CRC.
        let at = plain.len() - 5;
        plain[at] = 1;
        assert_eq!(
            Image::decode_peak(&plain).expect("an estimate"),
            flat + 1000 * 1000 * 4
        );
    }

    #[test]
    fn a_jpeg_that_must_hold_its_coefficients_is_told_from_one_that_streams() {
        // Start of image, an APP0 segment to step over, a frame header declaring
        // `frame` components, then the first scan carrying `scan` of them.
        let jpeg = |sof: u8, frame: u8, scan: u8| {
            let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00];
            jpeg.extend_from_slice(&[0xFF, sof, 0x00, 0x08, 8, 0, 16, 0, 16, frame]);
            jpeg.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x03, scan]);
            jpeg
        };
        assert!(!jpeg_holds_coefficients(&jpeg(0xC0, 3, 3)), "one full scan");
        assert!(!jpeg_holds_coefficients(TINY_JPEG), "the fixture streams");
        assert!(jpeg_holds_coefficients(&jpeg(0xC2, 3, 3)), "progressive");
        assert!(
            jpeg_holds_coefficients(&jpeg(0xC0, 3, 1)),
            "baseline, but a component per scan"
        );
        // A stray byte between segments, which the decoder steps over: the
        // progressive frame after it must still be found.
        let mut junk = vec![0xFF, 0xD8, 0x00];
        junk.extend_from_slice(&jpeg(0xC2, 3, 3)[2..]);
        assert!(jpeg_holds_coefficients(&junk));
        // Truncated, or a segment length too small to be real: the cautious
        // answer, not a loop and not a read past the end.
        assert!(jpeg_holds_coefficients(&[0xFF, 0xD8, 0xFF]));
        assert!(jpeg_holds_coefficients(&[
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x00
        ]));
    }
}
