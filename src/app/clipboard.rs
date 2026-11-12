//! Putting a path or a picture on the clipboard.
//!
//! Copy only, no paste, so this is the `wl_data_device` dance trimmed to owning a
//! selection. Two things can be copied, each its own kind of data under its own
//! MIME types:
//!
//! - a **file path**, as text, to paste into a terminal or a text field;
//! - an image's **bytes**, under its real image MIME (`image/png`, `image/jpeg`,
//!   …), so it pastes into another app as the picture itself.
//!
//! ```text
//!   copy: bytes ─▶ own a wl_data_source (offer mimes) ─▶ set_selection
//!   serve: source.send(mime, fd) ─▶ a thread writes our bytes ─▶ close
//! ```
//!
//! A source we own serves the same bytes for every MIME it advertises, so path
//! text and image bytes are two separate copies, never one source offering both.

use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::app::{State, ViewMode};
use crate::platform::conn::Connection;
use crate::platform::error::{Error, Result};
use crate::platform::protocol::{
    wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source,
};
use crate::platform::wire::{Arg, Reader};
use crate::render::image;

/// MIME types a copied path is advertised under, the charset-tagged form first.
const PATH_MIMES: &[&str] = &["text/plain;charset=utf-8", "text/plain"];

/// The selection we own, if any: the live `wl_data_source` and the bytes it serves.
/// `source` is `0` when we own nothing (the compositor cancelled us, or we never
/// copied).
pub(super) struct Clipboard {
    pub(super) source: u32,
    /// Shared with the threads serving it, so a paste in flight outlives the next
    /// copy without a copy of its own.
    data: Arc<[u8]>,
    /// Sources we have destroyed that the compositor may not have heard about yet.
    /// A `send` already on its way to one still arrives, with its fd.
    retired: Vec<u32>,
    /// Pastes being written right now.
    serving: Arc<AtomicUsize>,
}

/// How many destroyed sources are remembered. A `send` can only race the destroy
/// of a source that was just replaced, so the last few are all that matter.
const RETIRED_SOURCES: usize = 8;

/// How many pastes are written at once. Each is a thread parked on a pipe until
/// its reader drains it; a reader that never does must not be able to pile them
/// up without bound. Past this a paste gets an empty read and can ask again.
const MAX_SERVING: usize = 8;

impl Clipboard {
    pub(super) fn new() -> Self {
        Self {
            source: 0,
            data: Arc::from([]),
            retired: Vec::new(),
            serving: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Whether `object` is a source of ours, live or just destroyed.
    pub(super) fn owns(&self, object: u32) -> bool {
        (self.source != 0 && self.source == object) || self.retired.contains(&object)
    }

    /// Destroy the live source, remembering it for the events still in flight.
    fn retire(&mut self, conn: &mut Connection) {
        if self.source == 0 {
            return;
        }
        conn.request(self.source, wl_data_source::DESTROY, &[]);
        if self.retired.len() >= RETIRED_SOURCES {
            self.retired.remove(0);
        }
        self.retired.push(self.source);
        self.source = 0;
    }

    /// Write the selection to a paster's pipe, off the event loop: a pipe holds
    /// 64 KiB and a picture is megabytes, so the write lasts as long as the reader
    /// takes, which is the reader's to decide and not something to freeze on.
    fn serve(&self, fd: OwnedFd) {
        if self.serving.load(Ordering::Relaxed) >= MAX_SERVING {
            return;
        }
        self.serving.fetch_add(1, Ordering::Relaxed);
        let (data, serving) = (Arc::clone(&self.data), Arc::clone(&self.serving));
        let spawned = std::thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || {
                write_to_pipe(fd, &data);
                serving.fetch_sub(1, Ordering::Relaxed);
            });
        if spawned.is_err() {
            self.serving.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// Write all of `data` to `fd` and close it. A reader that goes away early ends
/// the write (EPIPE), which is its right and not worth reporting.
fn write_to_pipe(fd: OwnedFd, data: &[u8]) {
    let mut pipe = std::fs::File::from(fd);
    let mut rest = data;
    while !rest.is_empty() {
        match pipe.write(rest) {
            Ok(0) => return,
            Ok(n) => rest = rest.get(n..).unwrap_or_default(),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            // The compositor handed over a non-blocking pipe and it is full.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return,
        }
    }
}

impl State {
    /// Copy the path of whatever is current, as text. Any entry has a path, so this
    /// works over a directory or a document, not only over a picture.
    pub(super) fn copy_path(&mut self) {
        let Some(path) = self.current_path() else {
            return;
        };
        // The path's own bytes: a name that is not UTF-8 pastes as the name it is,
        // not as a lookalike with replacement characters that names no file.
        let text = path.as_os_str().as_bytes().to_vec();
        self.set_selection(text, PATH_MIMES);
    }

    /// Copy the current image's file bytes, under its real image MIME, so it pastes
    /// elsewhere as the picture. The file as-is (lossless, no re-encode); a no-op if
    /// it can no longer be read or is not a format we recognise.
    pub(super) fn copy_image(&mut self) {
        let Some(path) = self.current_image_path() else {
            return;
        };
        let Some((bytes, mime)) = image::read_source(&path) else {
            return;
        };
        self.set_selection(bytes, &[mime]);
    }

    /// The file the copy acts on: the picture on screen in the viewer, whatever the
    /// cursor is on in the explorer. A directory or a file that is not a picture has
    /// a path to copy but no image, which is what the two callers differ on.
    fn current_path(&self) -> Option<PathBuf> {
        match self.mode {
            ViewMode::Viewer => self.images.get(self.shown?).cloned(),
            _ => self.lead_entry().map(|e| e.path.clone()),
        }
    }

    /// The file a picture-copy acts on: as above, but only when it is a picture.
    fn current_image_path(&self) -> Option<PathBuf> {
        let path = self.current_path()?;
        crate::files::is_image(&path).then_some(path)
    }

    /// A `wl_data_source` event on a source of ours: serve our bytes to a paster,
    /// or tear the source down when another selection replaces ours.
    pub(super) fn on_data_source(
        &mut self,
        object: u32,
        opcode: u16,
        r: &mut Reader,
    ) -> Result<()> {
        match opcode {
            wl_data_source::EV_SEND => {
                // Every MIME we advertise serves the same bytes.
                let _mime = r.string()?;
                // The fd is taken whichever source this is for. Left queued, the
                // next event that carries one (a keymap, another paste) would be
                // handed this pipe instead of its own.
                let fd = self
                    .conn
                    .take_fd()
                    .ok_or_else(|| Error::msg("data_source.send arrived without its fd"))?;
                // A source we already destroyed serves nothing: dropping the fd
                // closes it, and the paster reads an empty selection.
                if object == self.clipboard.source {
                    self.clipboard.serve(fd);
                }
            }
            wl_data_source::EV_CANCELLED if object == self.clipboard.source => {
                // Another selection replaced ours: the source is spent.
                self.clipboard.retire(&mut self.conn);
                self.clipboard.data = Arc::from([]);
            }
            _ => {}
        }
        Ok(())
    }

    /// A `wl_data_device` event. The only one acted on is the introduction of an
    /// offer (someone else's selection, or a drag over the window): nothing here
    /// pastes or takes drops, so each is destroyed as it arrives rather than left
    /// to accumulate in the compositor for the life of the window.
    pub(super) fn on_data_device(&mut self, opcode: u16, r: &mut Reader) -> Result<()> {
        if opcode == wl_data_device::EV_DATA_OFFER {
            let offer = r.u32()?;
            self.conn.request(offer, wl_data_offer::DESTROY, &[]);
        }
        Ok(())
    }

    /// Become the clipboard owner serving `data` under `mimes`, replacing any source
    /// we already held. Needs the manager and device: a compositor without a
    /// `wl_data_device_manager` simply gets no clipboard, and copying is a silent
    /// no-op rather than an error.
    fn set_selection(&mut self, data: Vec<u8>, mimes: &[&str]) {
        let Some(manager) = self.data_device_manager else {
            return;
        };
        if self.data_device == 0 {
            return;
        }
        self.clipboard.retire(&mut self.conn);
        let source = self.alloc_id();
        self.conn.request(
            manager,
            wl_data_device_manager::CREATE_DATA_SOURCE,
            &[Arg::NewId(source)],
        );
        for mime in mimes {
            self.conn
                .request(source, wl_data_source::OFFER, &[Arg::Str(mime)]);
        }
        self.conn.request(
            self.data_device,
            wl_data_device::SET_SELECTION,
            &[Arg::Object(source), Arg::Uint(self.last_serial)],
        );
        self.clipboard.source = source;
        self.clipboard.data = Arc::from(data);
    }
}
