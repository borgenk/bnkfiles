//! What a file is: magic bytes first, the extension as a fallback.
//!
//! Deliberately not a shared-mime-info parser. That database is thousands of glob
//! and magic rules in XML, and parsing it is a project in itself; what an explorer
//! actually needs is to tell a picture from a document from an archive, and to
//! hand a MIME type to the desktop's handler lookup. A couple of dozen types
//! covers that, and the ones that matter most all announce themselves in their
//! first few bytes.
//!
//! Two questions, deliberately kept apart:
//!
//! - [`sniff`] — what is this file, given its first bytes and its name? Reads the
//!   content, so it is only asked when a file is being opened.
//! - [`from_extension`] — what does this file's *name* suggest? Free, so it is
//!   what a directory listing uses, and what [`crate::files::is_image`] is built
//!   on: sniffing every entry of a 100k-entry directory would mean opening every
//!   file in it.
//!
//! When the two disagree, the bytes win: an extension is a claim and a magic
//! number is evidence. How much evidence depends on the signature, though. A zip
//! or an ISO media header says which *container* this is, not what is in it, and
//! two bytes can be the start of anything. Those are narrowed or overruled by an
//! extension that fits them (see [`sniff`]); a signature that pins the format
//! down never is.

/// The image types the in-tree decoders actually handle (see `render/image.rs`).
/// A picture outside this set is still a picture, but the grid cannot draw it, so
/// it gets a file icon rather than a permanently broken thumbnail.
pub const DECODABLE: [&str; 5] = [
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/bmp",
    "image/webp",
];

/// What a file with no other evidence is called. Not `text/plain`: an unknown
/// file is more likely to be binary than to be readable text, and saying so
/// wrongly sends it to a text editor.
pub const UNKNOWN: &str = "application/octet-stream";

/// Whether this is one of the image types the viewer can decode.
pub fn is_decodable_image(mime: &str) -> bool {
    DECODABLE.contains(&mime)
}

/// A magic-number rule: the bytes to match, how far into the file they start, and
/// what a match means.
struct Magic {
    offset: usize,
    bytes: &'static [u8],
    mime: &'static str,
}

/// The signatures that settle the type on their own, most specific first. A
/// container format that needs a second look (RIFF, zip, ISO media) is handled
/// after this table rather than being contorted into one rule.
const MAGIC: &[Magic] = &[
    // Images.
    Magic {
        offset: 0,
        bytes: b"\x89PNG\r\n\x1a\n",
        mime: "image/png",
    },
    Magic {
        offset: 0,
        bytes: b"\xff\xd8\xff",
        mime: "image/jpeg",
    },
    Magic {
        offset: 0,
        bytes: b"GIF87a",
        mime: "image/gif",
    },
    Magic {
        offset: 0,
        bytes: b"GIF89a",
        mime: "image/gif",
    },
    Magic {
        offset: 0,
        bytes: b"\x00\x00\x01\x00",
        mime: "image/vnd.microsoft.icon",
    },
    Magic {
        offset: 0,
        bytes: b"II*\x00",
        mime: "image/tiff",
    },
    Magic {
        offset: 0,
        bytes: b"MM\x00*",
        mime: "image/tiff",
    },
    // Documents and archives.
    Magic {
        offset: 0,
        bytes: b"%PDF-",
        mime: "application/pdf",
    },
    // The third byte is the compression method, and deflate is the only one
    // there has ever been. Matching it keeps this from being a two-byte guess.
    Magic {
        offset: 0,
        bytes: b"\x1f\x8b\x08",
        mime: "application/gzip",
    },
    Magic {
        offset: 0,
        bytes: b"\xfd7zXZ\x00",
        mime: "application/x-xz",
    },
    Magic {
        offset: 0,
        bytes: b"(\xb5/\xfd",
        mime: "application/zstd",
    },
    Magic {
        offset: 0,
        bytes: b"BZh",
        mime: "application/x-bzip2",
    },
    Magic {
        offset: 0,
        bytes: b"7z\xbc\xaf'\x1c",
        mime: "application/x-7z-compressed",
    },
    Magic {
        offset: 0,
        bytes: b"Rar!\x1a\x07",
        mime: "application/vnd.rar",
    },
    // A tar's signature sits in the first header block, not at the start.
    Magic {
        offset: 257,
        bytes: b"ustar",
        mime: "application/x-tar",
    },
    // Executables.
    Magic {
        offset: 0,
        bytes: b"\x7fELF",
        mime: "application/x-executable",
    },
    // Audio and video.
    Magic {
        offset: 0,
        bytes: b"OggS",
        mime: "audio/ogg",
    },
    Magic {
        offset: 0,
        bytes: b"fLaC",
        mime: "audio/flac",
    },
    Magic {
        offset: 0,
        bytes: b"ID3",
        mime: "audio/mpeg",
    },
    Magic {
        offset: 0,
        bytes: b"\x1a\x45\xdf\xa3",
        mime: "video/x-matroska",
    },
    // Fonts.
    Magic {
        offset: 0,
        bytes: b"\x00\x01\x00\x00\x00",
        mime: "font/ttf",
    },
    Magic {
        offset: 0,
        bytes: b"OTTO",
        mime: "font/otf",
    },
    Magic {
        offset: 0,
        bytes: b"wOF2",
        mime: "font/woff2",
    },
];

/// Two-byte signatures. Real ones, but two bytes are also how plenty of ordinary
/// text begins ("BMW", a `#!` line on a Python script), so a match here is a
/// suggestion that a fitting extension overrules.
const WEAK_MAGIC: &[Magic] = &[
    Magic {
        offset: 0,
        bytes: b"BM",
        mime: "image/bmp",
    },
    Magic {
        offset: 0,
        bytes: b"#!",
        mime: "application/x-shellscript",
    },
];

/// What a zip is when nothing says more.
const ZIP: &str = "application/zip";

/// The formats that are a zip archive with a particular layout inside. Telling
/// them apart by content means reading the archive's directory; the extension
/// says it for free, and is believed for exactly these.
const ZIP_BASED: [&str; 9] = [
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    "application/vnd.oasis.opendocument.text",
    "application/vnd.oasis.opendocument.spreadsheet",
    "application/vnd.oasis.opendocument.presentation",
    "application/epub+zip",
    "application/java-archive",
    "application/vnd.android.package-archive",
];

/// What an ISO media file is when neither its brand nor its name says more.
const ISO_MEDIA: &str = "video/mp4";

/// The formats built on the ISO base media container, which all open with the
/// same `ftyp` box.
const ISO_MEDIA_BASED: [&str; 5] = [
    "video/mp4",
    "audio/mp4",
    "video/quicktime",
    "image/heic",
    "image/avif",
];

/// The types RIFF's sub-type tag tells apart.
const RIFF_BASED: [&str; 3] = ["image/webp", "audio/wav", "video/x-msvideo"];

/// Extension → type, for the listing (which has no bytes to look at) and as the
/// fallback when the bytes say nothing. Lower-case, without the dot.
const BY_EXTENSION: &[(&str, &str)] = &[
    // Images.
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("bmp", "image/bmp"),
    ("webp", "image/webp"),
    ("ico", "image/vnd.microsoft.icon"),
    ("tif", "image/tiff"),
    ("tiff", "image/tiff"),
    ("svg", "image/svg+xml"),
    ("avif", "image/avif"),
    ("heic", "image/heic"),
    // Text and code. The explorer does not open these itself, but the type is
    // what decides which application the desktop hands them to.
    ("txt", "text/plain"),
    ("log", "text/plain"),
    ("md", "text/markdown"),
    ("html", "text/html"),
    ("htm", "text/html"),
    ("css", "text/css"),
    ("csv", "text/csv"),
    ("json", "application/json"),
    ("xml", "application/xml"),
    ("toml", "application/toml"),
    ("yaml", "application/yaml"),
    ("yml", "application/yaml"),
    ("rs", "text/rust"),
    ("c", "text/x-csrc"),
    ("h", "text/x-chdr"),
    ("py", "text/x-python"),
    ("sh", "application/x-shellscript"),
    ("js", "text/javascript"),
    ("ts", "text/typescript"),
    // Documents and archives.
    ("pdf", "application/pdf"),
    ("zip", "application/zip"),
    ("gz", "application/gzip"),
    ("xz", "application/x-xz"),
    ("zst", "application/zstd"),
    ("bz2", "application/x-bzip2"),
    ("7z", "application/x-7z-compressed"),
    ("rar", "application/vnd.rar"),
    ("tar", "application/x-tar"),
    (
        "docx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ),
    (
        "xlsx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    ),
    (
        "pptx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    ),
    ("odt", "application/vnd.oasis.opendocument.text"),
    ("ods", "application/vnd.oasis.opendocument.spreadsheet"),
    ("odp", "application/vnd.oasis.opendocument.presentation"),
    ("epub", "application/epub+zip"),
    ("jar", "application/java-archive"),
    ("apk", "application/vnd.android.package-archive"),
    // Audio and video.
    ("mp3", "audio/mpeg"),
    ("flac", "audio/flac"),
    ("ogg", "audio/ogg"),
    ("opus", "audio/opus"),
    ("wav", "audio/wav"),
    ("m4a", "audio/mp4"),
    ("mp4", "video/mp4"),
    ("mkv", "video/x-matroska"),
    ("webm", "video/webm"),
    ("mov", "video/quicktime"),
    ("avi", "video/x-msvideo"),
];

/// What this file's name suggests it is, if anything. Case-insensitive.
pub fn from_extension(extension: &str) -> Option<&'static str> {
    let lower = extension.to_ascii_lowercase();
    BY_EXTENSION
        .iter()
        .find(|(ext, _)| *ext == lower)
        .map(|(_, mime)| *mime)
}

/// What this file is, from its first bytes and (failing those) its extension.
///
/// `head` need only be the first few hundred bytes; 512 covers every rule in the
/// table, tar's offset-257 signature included.
///
/// The evidence is taken strongest first:
///
/// 1. A signature that pins the format down. The extension is not consulted.
/// 2. A container signature (RIFF, zip, ISO media). The container is certain;
///    which format inside it is read from the header where the header says, and
///    otherwise from the extension, if it names a format of that container.
/// 3. A two-byte signature, unless the extension names a format the bytes do
///    not rule out.
/// 4. The extension alone, then a guess at whether it is text.
pub fn sniff(head: &[u8], extension: Option<&str>) -> &'static str {
    let named = extension.and_then(from_extension);
    if let Some(rule) = MAGIC.iter().find(|rule| rule.matches(head)) {
        return rule.mime;
    }
    // RIFF is a container: the type is the four bytes at offset 8, so a WebP and
    // a WAV are told apart there rather than by their shared header.
    if head.starts_with(b"RIFF") {
        if let Some(kind) = head.get(8..12) {
            return match kind {
                b"WEBP" => "image/webp",
                b"WAVE" => "audio/wav",
                b"AVI " => "video/x-msvideo",
                _ => UNKNOWN,
            };
        }
    }
    if head.starts_with(b"PK\x03\x04") {
        // A document is a zip, but opening one in the archive manager is not
        // what anyone asked for. A name that claims something that is not a
        // zip at all is still overruled.
        return named.filter(|mime| ZIP_BASED.contains(mime)).unwrap_or(ZIP);
    }
    if head.get(4..8) == Some(b"ftyp") {
        return iso_media(head.get(8..12), named);
    }
    if let Some(rule) = WEAK_MAGIC.iter().find(|rule| rule.matches(head)) {
        // A named format with a signature of its own would have matched above
        // if the name were true, so there the name is the lie and the two bytes
        // stand. A format with no signature (text, mostly) cannot be checked
        // that way, and its name is better evidence than two bytes are.
        return named
            .filter(|mime| !has_signature(mime))
            .unwrap_or(rule.mime);
    }
    // Nothing recognised the bytes. The extension is the next best evidence, and
    // it is the only evidence for the formats that have no signature at all
    // (plain text, SVG, CSV).
    if let Some(mime) = named {
        return mime;
    }
    // Still nothing: is it readable text? A file with no NUL bytes whose content
    // is valid UTF-8 is text as far as anything here is concerned. Checked last,
    // because it is a guess where the rules above are facts.
    if looks_like_text(head) {
        return "text/plain";
    }
    UNKNOWN
}

impl Magic {
    fn matches(&self, head: &[u8]) -> bool {
        head.get(self.offset..self.offset + self.bytes.len()) == Some(self.bytes)
    }
}

/// Which ISO media format this is, from the major brand in its `ftyp` box and
/// the type its name suggests.
///
/// The brand wins where it is specific. The generic ones (`isom`, `mp42`,
/// `mif1`) are shared by video, audio, and still images alike, so there the
/// extension decides among the formats this container carries.
fn iso_media(brand: Option<&[u8]>, named: Option<&'static str>) -> &'static str {
    match brand {
        Some(b"heic" | b"heix" | b"heim" | b"heis" | b"hevc" | b"hevx") => "image/heic",
        Some(b"avif" | b"avis") => "image/avif",
        Some(b"qt  ") => "video/quicktime",
        Some(b"M4A " | b"M4B ") => "audio/mp4",
        _ => named
            .filter(|mime| ISO_MEDIA_BASED.contains(mime))
            .unwrap_or(ISO_MEDIA),
    }
}

/// Whether files of this type start with bytes [`sniff`] recognises, so that a
/// file which does not start with them is not of this type whatever it is called.
fn has_signature(mime: &str) -> bool {
    mime == ZIP
        || MAGIC.iter().any(|rule| rule.mime == mime)
        || RIFF_BASED.contains(&mime)
        || ZIP_BASED.contains(&mime)
        || ISO_MEDIA_BASED.contains(&mime)
}

/// Whether `head` reads as text: valid UTF-8, no NUL, and no run of control
/// characters that a text file would not contain.
///
/// The UTF-8 check runs on a prefix that may end mid-character, so a trailing
/// partial sequence is trimmed rather than counted against it.
fn looks_like_text(head: &[u8]) -> bool {
    if head.is_empty() || head.contains(&0) {
        return false;
    }
    // Trim up to three bytes of a possibly-truncated final character.
    let Some(text) = (0..=3).find_map(|cut| {
        let whole = head.get(..head.len().checked_sub(cut)?)?;
        std::str::from_utf8(whole).ok().filter(|t| !t.is_empty())
    }) else {
        return false;
    };
    // Tab, newline, and carriage return are the control characters text has.
    !text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
}

#[cfg(test)]
mod tests {
    use crate::mime::*;

    /// A buffer whose first bytes are `head`, padded out so offset rules have
    /// something to read.
    fn with_head(head: &[u8], len: usize) -> Vec<u8> {
        let mut v = head.to_vec();
        v.resize(len.max(head.len()), b'\0');
        v
    }

    #[test]
    fn the_image_formats_are_recognised_by_their_bytes() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n\x00\x00", None), "image/png");
        assert_eq!(sniff(b"\xff\xd8\xff\xe0JFIF", None), "image/jpeg");
        assert_eq!(sniff(b"GIF89a\x01\x00", None), "image/gif");
        assert_eq!(sniff(b"GIF87a\x01\x00", None), "image/gif");
        assert_eq!(sniff(BMP_HEADER, None), "image/bmp");
        // RIFF needs the sub-type at offset 8 to tell a picture from a sound.
        assert_eq!(sniff(b"RIFF\x24\x00\x00\x00WEBPVP8 ", None), "image/webp");
        assert_eq!(sniff(b"RIFF\x24\x00\x00\x00WAVEfmt ", None), "audio/wav");
        assert_eq!(
            sniff(b"RIFF\x24\x00\x00\x00AVI LIST", None),
            "video/x-msvideo"
        );
    }

    #[test]
    fn the_bytes_outrank_a_lying_extension() {
        // A JPEG called .png is a JPEG. This is the whole reason for sniffing.
        assert_eq!(sniff(b"\xff\xd8\xff\xe0JFIF", Some("png")), "image/jpeg");
        assert_eq!(sniff(b"%PDF-1.7", Some("txt")), "application/pdf");
    }

    #[test]
    fn the_extension_answers_when_the_bytes_do_not() {
        // Formats with no signature at all, and files too short to have one.
        assert_eq!(sniff(b"<svg xmlns=", Some("svg")), "image/svg+xml");
        assert_eq!(sniff(b"a,b,c\n1,2,3", Some("csv")), "text/csv");
        assert_eq!(sniff(b"", Some("mp3")), "audio/mpeg");
        // Case does not matter in a file name.
        assert_eq!(sniff(b"", Some("PNG")), "image/png");
        assert_eq!(sniff(b"", Some("JPeG")), "image/jpeg");
    }

    #[test]
    fn archives_documents_and_media_are_recognised() {
        assert_eq!(sniff(b"%PDF-1.4\n%", None), "application/pdf");
        assert_eq!(sniff(b"PK\x03\x04\x14\x00", None), "application/zip");
        assert_eq!(sniff(b"\x1f\x8b\x08\x00", None), "application/gzip");
        assert_eq!(sniff(b"\xfd7zXZ\x00\x00", None), "application/x-xz");
        assert_eq!(sniff(b"(\xb5/\xfd\x00\x00", None), "application/zstd");
        assert_eq!(
            sniff(b"\x7fELF\x02\x01\x01", None),
            "application/x-executable"
        );
        assert_eq!(sniff(b"#!/bin/sh\nexit", None), "application/x-shellscript");
        assert_eq!(sniff(b"OggS\x00\x02", None), "audio/ogg");
        assert_eq!(sniff(b"fLaC\x00\x00", None), "audio/flac");
        assert_eq!(sniff(b"\x1a\x45\xdf\xa3\x01", None), "video/x-matroska");
        // mp4's signature sits at offset 4, after the box length.
        assert_eq!(sniff(b"\x00\x00\x00\x20ftypisom", None), "video/mp4");
    }

    /// The local file header every zip-based document starts with: version 2.0,
    /// deflate, and the first member's name further along.
    const ZIP_HEADER: &[u8] = b"PK\x03\x04\x14\x00\x06\x00\x08\x00\x00\x00\x21\x00";

    /// A 1x1 24-bit BMP's file header and the start of its info header.
    const BMP_HEADER: &[u8] = b"BM\x3a\x00\x00\x00\x00\x00\x00\x00\x36\x00\x00\x00\x28\x00\x00\x00";

    #[test]
    fn a_zip_based_document_is_the_document_its_name_says() {
        // Every one of these is a zip to its first four bytes. Calling them all
        // application/zip opens a letter in the archive manager.
        for (extension, mime) in [
            (
                "docx",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            ),
            (
                "xlsx",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            ),
            (
                "pptx",
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            ),
            ("odt", "application/vnd.oasis.opendocument.text"),
            ("ods", "application/vnd.oasis.opendocument.spreadsheet"),
            ("EPUB", "application/epub+zip"),
            ("jar", "application/java-archive"),
        ] {
            assert_eq!(sniff(ZIP_HEADER, Some(extension)), mime, ".{extension}");
        }
        // An actual archive, by name or by having no name, is an archive.
        assert_eq!(sniff(ZIP_HEADER, Some("zip")), "application/zip");
        assert_eq!(sniff(ZIP_HEADER, None), "application/zip");
        assert_eq!(sniff(ZIP_HEADER, Some("cbz")), "application/zip");
        // The name only chooses among zip formats. One claiming something that
        // is not a zip is still a lie, and the bytes still win.
        assert_eq!(sniff(ZIP_HEADER, Some("png")), "application/zip");
        assert_eq!(sniff(ZIP_HEADER, Some("txt")), "application/zip");
        assert_eq!(sniff(ZIP_HEADER, Some("mp4")), "application/zip");
    }

    #[test]
    fn an_iso_media_file_is_told_apart_by_its_brand() {
        // Real ftyp boxes: length, "ftyp", the major brand, a minor version.
        assert_eq!(
            sniff(b"\x00\x00\x00\x18ftypheic\x00\x00\x00\x00mif1heic", None),
            "image/heic"
        );
        assert_eq!(
            sniff(b"\x00\x00\x00\x1cftypavif\x00\x00\x00\x00avifmif1", None),
            "image/avif"
        );
        assert_eq!(
            sniff(b"\x00\x00\x00\x14ftypqt  \x20\x05\x03\x00qt  ", None),
            "video/quicktime"
        );
        assert_eq!(
            sniff(b"\x00\x00\x00\x20ftypM4A \x00\x00\x00\x00M4A mp42", None),
            "audio/mp4"
        );
        // A specific brand is evidence; a name that disagrees with it is not.
        assert_eq!(
            sniff(b"\x00\x00\x00\x18ftypheic\x00\x00\x00\x00", Some("mp4")),
            "image/heic"
        );
    }

    #[test]
    fn a_generic_iso_media_brand_defers_to_the_name() {
        // isom, mp42 and mif1 are written into video, audio, and stills alike.
        let isom = b"\x00\x00\x00\x20ftypisom\x00\x00\x02\x00isomiso2";
        assert_eq!(sniff(isom, Some("m4a")), "audio/mp4");
        assert_eq!(sniff(isom, Some("mov")), "video/quicktime");
        assert_eq!(sniff(isom, Some("mp4")), "video/mp4");
        let mif1 = b"\x00\x00\x00\x18ftypmif1\x00\x00\x00\x00mif1heic";
        assert_eq!(sniff(mif1, Some("heic")), "image/heic");
        assert_eq!(sniff(mif1, Some("avif")), "image/avif");
        // But only to a name that fits the container.
        assert_eq!(sniff(isom, Some("txt")), "video/mp4");
        assert_eq!(sniff(isom, Some("zip")), "video/mp4");
        // A head cut off before the brand is still ISO media.
        assert_eq!(sniff(b"\x00\x00\x00\x20ftyp", Some("m4a")), "audio/mp4");
    }

    #[test]
    fn two_bytes_do_not_outrank_a_name_they_cannot_contradict() {
        // Notes about a car are not a bitmap, and a Python script with an
        // interpreter line is still Python to the editor that opens it.
        assert_eq!(sniff(b"BMW i3 service notes\n", Some("txt")), "text/plain");
        assert_eq!(sniff(b"BM25 ranking\n", Some("md")), "text/markdown");
        assert_eq!(
            sniff(b"#!/usr/bin/env python3\nimport os\n", Some("py")),
            "text/x-python"
        );
        // With a name that agrees, or no name, the signature is all there is.
        assert_eq!(sniff(BMP_HEADER, Some("bmp")), "image/bmp");
        assert_eq!(
            sniff(b"#!/bin/sh\nexit", Some("sh")),
            "application/x-shellscript"
        );
        // A name whose format has a signature this file does not start with is
        // the lie, so there the two bytes stand.
        assert_eq!(sniff(BMP_HEADER, Some("png")), "image/bmp");
        assert_eq!(sniff(BMP_HEADER, Some("webp")), "image/bmp");
        assert_eq!(sniff(BMP_HEADER, Some("docx")), "image/bmp");
        assert_eq!(
            sniff(b"#!/bin/sh\nexit", Some("mp4")),
            "application/x-shellscript"
        );
    }

    #[test]
    fn a_two_byte_signature_does_not_shadow_a_longer_one() {
        // A tar whose first member's name happens to begin "BM" is a tar.
        let mut tar = with_head(b"BMP-exports/one.bmp", 512);
        tar[257..262].copy_from_slice(b"ustar");
        assert_eq!(sniff(&tar, None), "application/x-tar");
        // gzip is matched on its method byte too, so two bytes of it are not
        // enough to call a file compressed.
        assert_ne!(sniff(b"\x1f\x8b", None), "application/gzip");
    }

    #[test]
    fn a_tar_is_found_by_the_signature_inside_its_first_block() {
        // "ustar" lives at offset 257, well past where the other rules look.
        let mut tar = with_head(b"somefile.txt", 512);
        tar[257..262].copy_from_slice(b"ustar");
        assert_eq!(sniff(&tar, None), "application/x-tar");
        // A short read that does not reach the signature must not claim a match.
        assert_ne!(sniff(&tar[..200], None), "application/x-tar");
    }

    #[test]
    fn readable_text_is_the_last_guess_and_binary_is_not_text() {
        assert_eq!(sniff(b"hello, world\nsecond line\n", None), "text/plain");
        assert_eq!(
            sniff("a UTF-8 line with an em dash \u{2014}".as_bytes(), None),
            "text/plain"
        );
        // A NUL means binary, whatever else is in there.
        assert_eq!(sniff(b"text\x00more text", None), UNKNOWN);
        // So do control characters that text does not contain.
        assert_eq!(sniff(b"text\x07\x08bell", None), UNKNOWN);
        // Tabs and newlines are text.
        assert_eq!(sniff(b"col1\tcol2\r\nv1\tv2\r\n", None), "text/plain");
        // Nothing at all is not text.
        assert_eq!(sniff(b"", None), UNKNOWN);
    }

    #[test]
    fn a_truncated_final_character_does_not_make_a_text_file_binary() {
        // Reading the first N bytes can cut a multi-byte character in half, which
        // must not be mistaken for binary content.
        let text = "a line of prose ending in an em dash \u{2014}";
        for cut in 1..=3 {
            let head = &text.as_bytes()[..text.len() - cut];
            assert_eq!(sniff(head, None), "text/plain", "cut {cut} byte(s) short");
        }
    }

    #[test]
    fn only_the_formats_the_decoders_handle_count_as_decodable() {
        for mime in DECODABLE {
            assert!(is_decodable_image(mime));
        }
        // Real images the in-tree decoders have no answer for. The grid gives
        // these a file icon rather than a thumbnail that can never arrive.
        for mime in ["image/svg+xml", "image/tiff", "image/avif", "image/heic"] {
            assert!(!is_decodable_image(mime), "{mime} is not decodable");
        }
        assert!(!is_decodable_image("application/pdf"));
        assert!(!is_decodable_image(UNKNOWN));
    }

    #[test]
    fn the_extension_table_is_lower_case_and_has_no_duplicates() {
        // A duplicate would mean one of the two rows is unreachable, silently.
        let mut seen: Vec<&str> = Vec::new();
        for (ext, _) in BY_EXTENSION {
            assert_eq!(*ext, ext.to_ascii_lowercase(), "{ext} is not lower-case");
            assert!(!seen.contains(ext), "{ext} appears twice");
            seen.push(ext);
        }
    }
}
