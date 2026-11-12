//! Zoom and pan state for the image viewer.
//!
//! Holds the current zoom (fit-to-window or an explicit scale) and a pan
//! offset, and turns them into the `(scale, top-left)` geometry the renderer
//! samples with. Panning is clamped so an image edge never pulls inside the
//! window, and zoom keeps a chosen focus point (the cursor, or the window
//! centre) fixed so it feels anchored.

/// Per-step zoom factor (a notch / a `+` press), and the explicit-scale bounds.
pub const ZOOM_STEP: f32 = 1.25;
const MIN_SCALE: f32 = 0.05;
const MAX_SCALE: f32 = 20.0;

#[derive(Clone, Copy)]
enum Zoom {
    /// Scale to fit the window, never past 1:1, centered.
    Fit,
    /// An explicit scale factor.
    Scale(f32),
}

pub struct Viewer {
    zoom: Zoom,
    /// Desired top-left of the displayed image in window pixels; clamped on use.
    pan: (f32, f32),
}

impl Default for Viewer {
    fn default() -> Self {
        Self {
            zoom: Zoom::Fit,
            pan: (0.0, 0.0),
        }
    }
}

impl Viewer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset to fit-on-screen for a freshly loaded image.
    pub fn reset(&mut self) {
        self.zoom = Zoom::Fit;
        self.pan = (0.0, 0.0);
    }

    fn fit_scale(dw: f32, dh: f32, iw: f32, ih: f32) -> f32 {
        (dw / iw).min(dh / ih).min(1.0)
    }

    fn scale(&self, dw: f32, dh: f32, iw: f32, ih: f32) -> f32 {
        match self.zoom {
            Zoom::Fit => Self::fit_scale(dw, dh, iw, ih),
            Zoom::Scale(s) => s,
        }
    }

    /// `(scale, off_x, off_y)` for the current view with pan clamped; `off` is
    /// the displayed image's top-left in window pixels.
    pub fn geometry(&self, dw: u32, dh: u32, iw: u32, ih: u32) -> (f32, f32, f32) {
        let (dw, dh, iw, ih) = (dw as f32, dh as f32, iw as f32, ih as f32);
        let scale = self.scale(dw, dh, iw, ih);
        let ox = axis_offset(dw, iw * scale, self.pan.0);
        let oy = axis_offset(dh, ih * scale, self.pan.1);
        (scale, ox, oy)
    }

    fn zoom_to(&mut self, new_scale: f32, focus: (f32, f32), dw: u32, dh: u32, iw: u32, ih: u32) {
        // An image large enough that fitting it needs less than MIN_SCALE (a
        // panorama in a small window) must still be allowed to sit at its fit
        // scale, or clamping to MIN_SCALE would turn a zoom-out into a zoom-in.
        let fit = Self::fit_scale(dw as f32, dh as f32, iw as f32, ih as f32);
        let new_scale = new_scale.clamp(MIN_SCALE.min(fit), MAX_SCALE);
        let (scale, ox, oy) = self.geometry(dw, dh, iw, ih);
        // Source point currently under `focus`; keep it there at the new scale.
        let sx = (focus.0 - ox) / scale;
        let sy = (focus.1 - oy) / scale;
        self.pan = (focus.0 - sx * new_scale, focus.1 - sy * new_scale);
        self.zoom = Zoom::Scale(new_scale);
    }

    /// Multiply the current scale by `factor`, anchored at `focus`.
    pub fn zoom_by(&mut self, factor: f32, focus: (f32, f32), dw: u32, dh: u32, iw: u32, ih: u32) {
        let current = self.scale(dw as f32, dh as f32, iw as f32, ih as f32);
        self.zoom_to(current * factor, focus, dw, dh, iw, ih);
    }

    pub fn set_fit(&mut self) {
        self.reset();
    }

    pub fn set_actual(&mut self, focus: (f32, f32), dw: u32, dh: u32, iw: u32, ih: u32) {
        self.zoom_to(1.0, focus, dw, dh, iw, ih);
    }

    /// Toggle between fit and 100%.
    pub fn toggle(&mut self, focus: (f32, f32), dw: u32, dh: u32, iw: u32, ih: u32) {
        match self.zoom {
            Zoom::Fit => self.set_actual(focus, dw, dh, iw, ih),
            Zoom::Scale(_) => self.set_fit(),
        }
    }

    /// Move the image by `(dx, dy)` window pixels, clamping the stored pan so a
    /// drag past an edge builds up no slack to unwind. No-op while fitting (the
    /// image is centered then, so there is nothing to pan).
    pub fn pan_by(&mut self, dx: f32, dy: f32, dw: u32, dh: u32, iw: u32, ih: u32) {
        if let Zoom::Scale(_) = self.zoom {
            let (dw, dh, iw, ih) = (dw as f32, dh as f32, iw as f32, ih as f32);
            let scale = self.scale(dw, dh, iw, ih);
            self.pan = (
                axis_offset(dw, iw * scale, self.pan.0 + dx),
                axis_offset(dh, ih * scale, self.pan.1 + dy),
            );
        }
    }
}

/// Offset for one axis: centre when the image is smaller than the window, else
/// the clamped pan so an image edge never pulls inside the window.
fn axis_offset(window: f32, displayed: f32, pan: f32) -> f32 {
    if displayed <= window {
        (window - displayed) / 2.0
    } else {
        pan.clamp(window - displayed, 0.0)
    }
}

#[cfg(test)]
mod tests {
    use crate::viewer::*;

    #[test]
    fn fit_centers_small_image() {
        let v = Viewer::new();
        // 2x2 image in a 4x4 window: scale clamps to 1.0, centered at (1, 1).
        assert_eq!(v.geometry(4, 4, 2, 2), (1.0, 1.0, 1.0));
    }

    #[test]
    fn pan_is_clamped_to_keep_edges() {
        let mut v = Viewer::new();
        // 4x4 image in a 4x4 window, zoom 2x -> displayed 8x8, pannable.
        v.zoom_by(2.0, (2.0, 2.0), 4, 4, 4, 4);
        v.pan_by(1000.0, 1000.0, 4, 4, 4, 4); // far past the top-left edge
        let (scale, ox, oy) = v.geometry(4, 4, 4, 4);
        assert_eq!(scale, 2.0);
        // Clamp range is [window - displayed, 0] = [-4, 0]; +1000 clamps to 0.
        assert_eq!((ox, oy), (0.0, 0.0));
        v.pan_by(-2000.0, -2000.0, 4, 4, 4, 4); // far past the bottom-right edge
        let (_, ox, oy) = v.geometry(4, 4, 4, 4);
        assert_eq!((ox, oy), (-4.0, -4.0));
    }

    #[test]
    fn zooming_out_from_a_tiny_fit_scale_never_zooms_in() {
        let mut v = Viewer::new();
        // A 40000x100 panorama in a 400x400 window fits at 0.01, well under
        // MIN_SCALE: the zoom floor has to follow the fit scale down, or zooming
        // out would clamp back up to MIN_SCALE and enlarge the image.
        let (dw, dh, iw, ih) = (400, 400, 40000, 100);
        let (fit, _, _) = v.geometry(dw, dh, iw, ih);
        assert!(fit < MIN_SCALE);
        v.zoom_by(1.0 / ZOOM_STEP, (200.0, 200.0), dw, dh, iw, ih);
        let (scale, _, _) = v.geometry(dw, dh, iw, ih);
        assert_eq!(scale, fit, "zoom out past the fit scale must floor at it");
    }

    #[test]
    fn zoom_keeps_focus_point_fixed() {
        let mut v = Viewer::new();
        // Window == image (10x10); focus at the top-left corner (0,0).
        v.zoom_by(2.0, (0.0, 0.0), 10, 10, 10, 10);
        let (scale, ox, oy) = v.geometry(10, 10, 10, 10);
        assert_eq!(scale, 2.0);
        // The source pixel under (0,0) before zoom must still map under (0,0).
        // off should be 0 (top-left anchored), clamped within [-10, 0].
        assert_eq!((ox, oy), (0.0, 0.0));
    }
}
