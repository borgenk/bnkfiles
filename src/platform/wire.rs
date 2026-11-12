//! Wayland wire-format encoding and decoding.
//!
//! A message is: `u32 object_id`, then `u32 (size << 16 | opcode)` where size
//! is the whole message length in bytes including this 8-byte header, then the
//! arguments. Everything is host byte order (little-endian on our targets), and
//! every argument is padded to a 4-byte boundary. File descriptors are not in
//! the byte stream; they travel as SCM_RIGHTS ancillary data (see `ffi`).

use crate::platform::error::{Error, Result};

/// One request argument. The `Bind` variant is the "generic" new-id used only
/// by `wl_registry.bind`, where the interface is not known statically and is
/// therefore sent inline as interface-string + version + id.
pub enum Arg<'a> {
    Int(i32),
    Uint(u32),
    Object(u32),
    NewId(u32),
    Str(&'a str),
    Bind {
        interface: &'a str,
        version: u32,
        new_id: u32,
    },
}

/// A fully received event: its target object, opcode, and the argument bytes
/// after the header. Owned so the connection buffer can advance independently.
pub struct Message {
    pub object: u32,
    pub opcode: u16,
    pub body: Vec<u8>,
}

/// The largest request libwayland's server side accepts; a longer one is a
/// protocol error that closes the connection.
const MAX_REQUEST: usize = 4096;

/// Append an encoded request to `buf`.
///
/// A request larger than [`MAX_REQUEST`] is dropped, not sent. Nothing here builds
/// one (the only variable-length arguments are a window title and MIME types), so
/// this is the backstop that keeps a caller that someday does from being fatal.
/// Dropped rather than truncated, because a cut frame would put the next request's
/// bytes where this one's arguments should be.
pub fn encode(buf: &mut Vec<u8>, object: u32, opcode: u16, args: &[Arg]) {
    let start = buf.len();
    buf.extend_from_slice(&object.to_ne_bytes());
    buf.extend_from_slice(&[0u8; 4]); // size|opcode, patched once length is known
    for arg in args {
        encode_arg(buf, arg);
    }
    let size = match u16::try_from(buf.len() - start) {
        Ok(size) if usize::from(size) <= MAX_REQUEST => size,
        _ => {
            buf.truncate(start);
            return;
        }
    };
    let word = (u32::from(size) << 16) | u32::from(opcode);
    if let Some(header) = buf.get_mut(start + 4..start + 8) {
        header.copy_from_slice(&word.to_ne_bytes());
    }
}

fn encode_arg(buf: &mut Vec<u8>, arg: &Arg) {
    match arg {
        Arg::Int(v) => buf.extend_from_slice(&v.to_ne_bytes()),
        Arg::Uint(v) | Arg::Object(v) | Arg::NewId(v) => buf.extend_from_slice(&v.to_ne_bytes()),
        Arg::Str(s) => encode_str(buf, s),
        Arg::Bind {
            interface,
            version,
            new_id,
        } => {
            encode_str(buf, interface);
            buf.extend_from_slice(&version.to_ne_bytes());
            buf.extend_from_slice(&new_id.to_ne_bytes());
        }
    }
}

/// Encode a string: length (including the trailing NUL) then the bytes, the
/// NUL, and zero padding up to a 4-byte boundary.
fn encode_str(buf: &mut Vec<u8>, s: &str) {
    let len = s.len() as u32 + 1;
    buf.extend_from_slice(&len.to_ne_bytes());
    buf.extend_from_slice(s.as_bytes());
    buf.push(0);
    while !buf.len().is_multiple_of(4) {
        buf.push(0);
    }
}

/// Sequential reader over an event's argument bytes, decoding each argument in
/// native byte order (the wire format is host-endian). Every read is bounds-checked:
/// the bytes are the compositor's.
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// The next `n` bytes, advancing past them, or an error (the reader unmoved)
    /// when fewer remain.
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let truncated = || Error::msg("truncated wayland message");
        let end = self.pos.checked_add(n).ok_or_else(truncated)?;
        let slice = self.data.get(self.pos..end).ok_or_else(truncated)?;
        self.pos = end;
        Ok(slice)
    }

    fn word(&mut self) -> Result<[u8; 4]> {
        let bytes = self.take(4)?;
        bytes
            .try_into()
            .map_err(|_| Error::msg("truncated wayland message"))
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_ne_bytes(self.word()?))
    }

    /// Read a `wl_fixed`: a signed 24.8 fixed-point number, as pixels. Used for
    /// pointer coordinates.
    pub fn fixed(&mut self) -> Result<f32> {
        Ok(i32::from_ne_bytes(self.word()?) as f32 / 256.0)
    }

    /// Read an array argument: a length-prefixed blob of bytes, padded to a
    /// 4-byte boundary. Returned as raw bytes; callers that only need to skip it
    /// (like the `states` array in xdg_toplevel.configure) ignore the result.
    pub fn array(&mut self) -> Result<&'a [u8]> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        let pad = (4 - (len % 4)) % 4;
        let _ = self.take(pad)?;
        Ok(bytes)
    }

    /// Read a string argument, dropping its trailing NUL and its padding.
    pub fn string(&mut self) -> Result<&'a str> {
        let len = self.u32()? as usize;
        if len == 0 {
            return Ok("");
        }
        let bytes = self.take(len)?;
        let pad = (4 - (len % 4)) % 4;
        let _ = self.take(pad)?;
        // `len` counts the trailing NUL and is nonzero here.
        let text = bytes.split_last().map_or(bytes, |(_, text)| text);
        core::str::from_utf8(text).map_err(|_| Error::msg("invalid utf-8 in wayland string"))
    }
}

#[cfg(test)]
mod tests {
    use crate::platform::wire::*;

    #[test]
    fn header_carries_size_and_opcode() {
        let mut buf = Vec::new();
        encode(&mut buf, 5, 2, &[Arg::Uint(0xdead_beef), Arg::Int(-1)]);
        assert_eq!(buf.len(), 8 + 4 + 4);
        let object = u32::from_ne_bytes(buf[0..4].try_into().unwrap());
        let word = u32::from_ne_bytes(buf[4..8].try_into().unwrap());
        assert_eq!(object, 5);
        assert_eq!(word >> 16, 16, "size in bytes");
        assert_eq!(word & 0xffff, 2, "opcode");
    }

    #[test]
    fn string_then_uint_roundtrips_with_padding() {
        let mut buf = Vec::new();
        encode(&mut buf, 1, 0, &[Arg::Str("wl_shm"), Arg::Uint(7)]);
        // String: 4 (len) + "wl_shm\0" (7) padded to 8 = 12 bytes. Plus the
        // uint (4) and the 8-byte header.
        assert_eq!(buf.len(), 8 + 12 + 4);
        let mut r = Reader::new(&buf[8..]);
        assert_eq!(r.string().unwrap(), "wl_shm");
        assert_eq!(r.u32().unwrap(), 7);
    }

    #[test]
    fn bind_encodes_interface_version_and_id() {
        let mut buf = Vec::new();
        encode(
            &mut buf,
            2,
            0,
            &[
                Arg::Uint(3),
                Arg::Bind {
                    interface: "xdg_wm_base",
                    version: 1,
                    new_id: 9,
                },
            ],
        );
        let mut r = Reader::new(&buf[8..]);
        assert_eq!(r.u32().unwrap(), 3, "global name");
        assert_eq!(r.string().unwrap(), "xdg_wm_base");
        assert_eq!(r.u32().unwrap(), 1, "version");
        assert_eq!(r.u32().unwrap(), 9, "new id");
    }

    #[test]
    fn reader_rejects_truncation() {
        let mut r = Reader::new(&[0u8, 0, 0]);
        assert!(r.u32().is_err());
    }

    #[test]
    fn array_reads_length_prefixed_bytes_with_padding() {
        // A 5-byte array: u32 length, 5 bytes, then 3 padding bytes (to 8). A
        // trailing uint proves the reader resumed at the right offset.
        let mut buf = Vec::new();
        buf.extend_from_slice(&5u32.to_ne_bytes());
        buf.extend_from_slice(&[1, 2, 3, 4, 5, 0, 0, 0]);
        buf.extend_from_slice(&99u32.to_ne_bytes());
        let mut r = Reader::new(&buf);
        assert_eq!(r.array().unwrap(), &[1, 2, 3, 4, 5]);
        assert_eq!(r.u32().unwrap(), 99);
    }

    #[test]
    fn fixed_decodes_signed_24_8() {
        // 256 (0x100) is 1.0; -256 is -1.0; 128 is 0.5.
        let mut buf = Vec::new();
        for raw in [256i32, -256, 128] {
            buf.extend_from_slice(&raw.to_ne_bytes());
        }
        let mut r = Reader::new(&buf);
        assert_eq!(r.fixed().unwrap(), 1.0);
        assert_eq!(r.fixed().unwrap(), -1.0);
        assert_eq!(r.fixed().unwrap(), 0.5);
    }

    #[test]
    fn a_request_too_large_for_the_compositor_is_dropped_whole() {
        let mut buf = Vec::new();
        encode(&mut buf, 3, 1, &[Arg::Uint(7)]);
        let before = buf.clone();
        encode(&mut buf, 5, 2, &[Arg::Str(&"x".repeat(MAX_REQUEST))]);
        assert_eq!(buf, before, "the stream holds only the request that fit");
    }
}
