//! Scene-IR → GDI replay (design §6.1/D2): one `PaintSink` over the page
//! DC. Coordinate strategy: the page world transform (96-dpi scene px →
//! device units) is composed with a 64× subpixel pre-scale, and every
//! emitted coordinate is `(scene_px * 64).round()` — GDI path/text
//! coordinates are integers in logical units, so the pre-scale preserves
//! sub-96-dpi precision (a 600-dpi dot is ~6.4 subpixel units).
//!
//! Mapping: fills → `BeginPath`/`EndPath`/`FillPath` (brush + fill rule);
//! image-brush fills → `StretchDIBits` (BGRA, top-down); strokes →
//! `ExtCreatePen` (PS_GEOMETRIC, dash → PS_USERSTYLE) + `StrokePath`;
//! glyph runs → private font (`AddFontMemResourceEx` once per blob,
//! `CreateFontIndirectW` by parsed family) + per-glyph `ExtTextOutW` with
//! `ETO_GLYPH_INDEX`; clips → `SaveDC` + `SelectClipPath` / `RestoreDC`.
//! GDI has no compositor: group alpha/masks/filters and blurred rects warn
//! and degrade (v1 accepted gaps); solid-color alpha is ignored (warned).

use super::coords::{affine_to_xform, lf_height, rgba_to_bgra};
use super::font_name::family_name;
use imaging::kurbo::{Affine, PathEl, Point};
use imaging::peniko;
use imaging::record::{self, Scene};
use std::collections::HashMap;
use windows_sys::Win32::Foundation::{HANDLE, POINT};
use windows_sys::Win32::Graphics::Gdi::*;

/// Subpixel pre-scale folded into every composed transform: emitted GDI
/// logical coordinates are 1/64 of a scene pixel.
const SUBPIXEL: f64 = 64.0;

/// COLORREF is 0x00BBGGRR.
fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (b as u32) << 16 | (g as u32) << 8 | r as u32
}

fn xform_to_affine(t: &XFORM) -> Affine {
    Affine::new([
        t.eM11 as f64,
        t.eM12 as f64,
        t.eM21 as f64,
        t.eM22 as f64,
        t.eDx as f64,
        t.eDy as f64,
    ])
}

/// Registered private fonts + created logical faces for one job.
pub(crate) struct FontRegistry {
    mem_fonts: HashMap<usize, HANDLE>,
    faces: HashMap<(usize, i32), HFONT>,
}

impl FontRegistry {
    pub(crate) fn new() -> Self {
        Self {
            mem_fonts: HashMap::new(),
            faces: HashMap::new(),
        }
    }

    /// Register the font bytes (once per blob) and select a face of
    /// `size_px`. Returns the DC's previous object for restoration.
    fn select_face(
        &mut self,
        hdc: HDC,
        font: &peniko::FontData,
        size_px: f32,
    ) -> Result<HGDIOBJ, String> {
        let ptr = font.data.as_ref().as_ptr() as usize;
        if !self.mem_fonts.contains_key(&ptr) {
            let mut num_fonts: u32 = 0;
            let handle = unsafe {
                AddFontMemResourceEx(
                    font.data.as_ref().as_ptr() as *const core::ffi::c_void,
                    font.data.as_ref().len() as u32,
                    std::ptr::null(),
                    &mut num_fonts,
                )
            };
            if handle.is_null() {
                return Err("AddFontMemResourceEx failed".to_string());
            }
            self.mem_fonts.insert(ptr, handle);
        }
        let height = lf_height(size_px);
        let face = *self.faces.entry((ptr, height)).or_insert_with(|| {
            let family =
                family_name(font.data.as_ref(), font.index).unwrap_or_default();
            let mut name = [0u16; 32];
            for (slot, unit) in name.iter_mut().zip(family.encode_utf16()) {
                *slot = unit;
            }
            unsafe {
                CreateFontIndirectW(&LOGFONTW {
                    lfHeight: height,
                    lfWidth: 0,
                    lfEscapement: 0,
                    lfOrientation: 0,
                    lfWeight: 400,
                    lfItalic: 0,
                    lfUnderline: 0,
                    lfStrikeOut: 0,
                    lfCharSet: DEFAULT_CHARSET as u8,
                    lfOutPrecision: 0,
                    lfClipPrecision: 0,
                    lfQuality: 0,
                    lfPitchAndFamily: 0,
                    lfFaceName: name,
                })
            }
        });
        if face.is_null() {
            return Err("CreateFontIndirectW failed".to_string());
        }
        Ok(unsafe { SelectObject(hdc, face) })
    }

    /// Drop all private fonts (job end).
    pub(crate) fn release_all(&mut self) {
        for (_, handle) in self.mem_fonts.drain() {
            unsafe {
                RemoveFontMemResourceEx(handle);
            }
        }
        self.faces.clear();
    }
}

/// Replay one page-local scene onto a page DC. `page` is the page world
/// transform (scene px → device); it must already be active when called
/// and is re-established after every per-draw override.
pub(crate) fn replay_page(
    hdc: HDC,
    scene: &Scene,
    page: XFORM,
    warnings: &mut Vec<String>,
    fonts: &mut FontRegistry,
) {
    unsafe {
        SetGraphicsMode(hdc, GM_ADVANCED as i32);
        SetStretchBltMode(hdc, HALFTONE as i32);
    }
    let mut sink = GdiSink {
        hdc,
        warnings,
        fonts,
        page: xform_to_affine(&page),
        clip_depth: 0,
        group_clips: Vec::new(),
        last_pos: (0.0, 0.0),
    };
    record::replay(scene, &mut sink);
    if sink.clip_depth > 0 {
        unsafe {
            RestoreDC(hdc, -sink.clip_depth);
        }
    }
}

struct GdiSink<'a> {
    hdc: HDC,
    warnings: &'a mut Vec<String>,
    fonts: &'a mut FontRegistry,
    /// Page world transform (scene px → device), kept for composition.
    page: Affine,
    /// Net `SaveDC` depth (clips + group clips).
    clip_depth: i32,
    /// Whether each `push_group` pushed a SaveDC (its pop restores it).
    group_clips: Vec<bool>,
    /// Last scene-space path anchor (for quadratic degree elevation).
    last_pos: (f64, f64),
}

impl GdiSink<'_> {
    fn warn_once(&mut self, message: &str) {
        if !self.warnings.iter().any(|w| w == message) {
            self.warnings.push(message.to_string());
        }
    }

    /// Install `page ∘ draw ∘ scale(1/SUBPIXEL)` as the world transform:
    /// emitted logical coordinates are `scene_px * 64` (see the module
    /// docs), so the inverse subpixel scale folds into the transform —
    /// `device = page(draw(logical / 64)) == page(draw(scene_px))`.
    fn set_world(&self, transform: Affine) {
        let full = self.page * transform * Affine::scale(1.0 / SUBPIXEL);
        unsafe {
            SetWorldTransform(self.hdc, &affine_to_xform(&full));
        }
    }

    /// Re-install the plain page transform (per-draw override reset).
    fn reset_world(&self) {
        let full = self.page * Affine::scale(1.0 / SUBPIXEL);
        unsafe {
            SetWorldTransform(self.hdc, &affine_to_xform(&full));
        }
    }

    fn emit(v: f64) -> i32 {
        (v * SUBPIXEL).round() as i32
    }

    /// Path elements in scene px; tracks the anchor for quad elevation.
    fn path_els(&mut self, els: &[PathEl]) {
        unsafe {
            for el in els {
                match *el {
                    PathEl::MoveTo(p) => {
                        MoveToEx(self.hdc, Self::emit(p.x), Self::emit(p.y), std::ptr::null_mut());
                        self.last_pos = (p.x, p.y);
                    }
                    PathEl::LineTo(p) => {
                        LineTo(self.hdc, Self::emit(p.x), Self::emit(p.y));
                        self.last_pos = (p.x, p.y);
                    }
                    PathEl::QuadTo(c, p) => {
                        // Degree-elevate the quadratic to GDI's cubic using
                        // the tracked scene-space anchor.
                        let (x0, y0) = self.last_pos;
                        let c1 = ((x0 + 2.0 * c.x) / 3.0, (y0 + 2.0 * c.y) / 3.0);
                        let c2 = ((p.x + 2.0 * c.x) / 3.0, (p.y + 2.0 * c.y) / 3.0);
                        PolyBezierTo(
                            self.hdc,
                            [
                                POINT { x: Self::emit(c1.0), y: Self::emit(c1.1) },
                                POINT { x: Self::emit(c2.0), y: Self::emit(c2.1) },
                                POINT { x: Self::emit(p.x), y: Self::emit(p.y) },
                            ]
                            .as_ptr(),
                            3,
                        );
                        self.last_pos = (p.x, p.y);
                    }
                    PathEl::CurveTo(c1, c2, p) => {
                        PolyBezierTo(
                            self.hdc,
                            [
                                POINT { x: Self::emit(c1.x), y: Self::emit(c1.y) },
                                POINT { x: Self::emit(c2.x), y: Self::emit(c2.y) },
                                POINT { x: Self::emit(p.x), y: Self::emit(p.y) },
                            ]
                            .as_ptr(),
                            3,
                        );
                        self.last_pos = (p.x, p.y);
                    }
                    PathEl::ClosePath => {
                        CloseFigure(self.hdc);
                    }
                }
            }
        }
    }

    /// Begin a fill clip scope (shared by push_clip and group clips).
    unsafe fn push_fill_clip(
        &mut self,
        transform: Affine,
        shape: imaging::GeometryRef<'_>,
        fill_rule: peniko::Fill,
    ) {
        SaveDC(self.hdc);
        self.clip_depth += 1;
        BeginPath(self.hdc);
        self.set_world(transform);
        let path = shape.to_path(0.1);
        self.path_els(path.elements());
        SetPolyFillMode(
            self.hdc,
            if matches!(fill_rule, peniko::Fill::EvenOdd) {
                ALTERNATE
            } else {
                WINDING
            },
        );
        EndPath(self.hdc);
        SelectClipPath(self.hdc, RGN_COPY);
        self.reset_world();
    }

    /// Image draw: destination rect from the placement transform over the
    /// natural-size rect (the painter's draw_image convention), pixels
    /// swizzled to top-down BGRA.
    fn draw_image(&mut self, placement: Affine, brush: peniko::ImageBrushRef<'_>) {
        let image = brush.image;
        let (w, h) = (image.width, image.height);
        if w == 0 || h == 0 {
            return;
        }
        let corners = [
            placement * Point::new(0.0, 0.0),
            placement * Point::new(f64::from(w), 0.0),
            placement * Point::new(0.0, f64::from(h)),
            placement * Point::new(f64::from(w), f64::from(h)),
        ];
        let (mut x0, mut y0, mut x1, mut y1) = (
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        );
        for p in corners {
            x0 = x0.min(p.x);
            y0 = y0.min(p.y);
            x1 = x1.max(p.x);
            y1 = y1.max(p.y);
        }
        // Device-space destination: map through the page transform.
        let d0 = self.page * Point::new(x0, y0);
        let d1 = self.page * Point::new(x1, y1);
        let bgra = rgba_to_bgra(image.data.as_ref());
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32), // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB as u32,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [RGBQUAD {
                rgbBlue: 0,
                rgbGreen: 0,
                rgbRed: 0,
                rgbReserved: 0,
            }],
        };
        unsafe {
            // Destination is set in DEVICE units (STRETCH destination is
            // transformed by the world transform, so switch to identity
            // first to avoid double transforms).
            SetGraphicsMode(self.hdc, GM_ADVANCED as i32);
            SetWorldTransform(self.hdc, &affine_to_xform(&Affine::IDENTITY));
            StretchDIBits(
                self.hdc,
                d0.x.round() as i32,
                d0.y.round() as i32,
                (d1.x - d0.x).round().max(1.0) as i32,
                (d1.y - d0.y).round().max(1.0) as i32,
                0,
                0,
                w as i32,
                h as i32,
                bgra.as_ptr() as *const core::ffi::c_void,
                &info,
                DIB_RGB_COLORS as u32,
                SRCCOPY as u32,
            );
            let _ = &mut info;
            self.reset_world();
        }
    }
}

impl imaging::PaintSink for GdiSink<'_> {
    fn push_clip(&mut self, clip: imaging::ClipRef<'_>) {
        unsafe {
            match clip {
                imaging::ClipRef::Fill {
                    transform,
                    shape,
                    fill_rule,
                } => {
                    self.push_fill_clip(transform, shape, fill_rule);
                }
                imaging::ClipRef::Stroke {
                    transform,
                    shape,
                    ..
                } => {
                    // GDI clips are fill-only; the stroke outline widens to
                    // a fill approximation (v1 accepted gap).
                    self.warn_once("stroke clips approximated by their fill path in GDI v1");
                    self.push_fill_clip(transform, shape, peniko::Fill::NonZero);
                }
            }
        }
    }

    fn pop_clip(&mut self) {
        unsafe {
            RestoreDC(self.hdc, -1);
        }
        self.clip_depth -= 1;
    }

    fn push_group(&mut self, group: imaging::GroupRef<'_>) {
        if !group.filters.is_empty() {
            self.warn_once("group filters skipped in GDI v1");
        }
        if group.mask.is_some() {
            self.warn_once("group masks skipped in GDI v1");
        }
        if group.composite.alpha < 1.0 {
            self.warn_once("group alpha ignored in GDI v1");
        }
        // Groups render inline (no isolation); an isolated clip becomes a
        // plain clip scope.
        match group.clip {
            Some(imaging::ClipRef::Fill {
                transform,
                shape,
                fill_rule,
            }) => unsafe {
                self.push_fill_clip(transform, shape, fill_rule);
                self.group_clips.push(true);
            },
            Some(_) => {
                self.warn_once("stroke group-clip approximated in GDI v1");
                self.group_clips.push(false);
            }
            None => self.group_clips.push(false),
        }
    }

    fn pop_group(&mut self) {
        if self.group_clips.pop() == Some(true) {
            unsafe {
                RestoreDC(self.hdc, -1);
            }
            self.clip_depth -= 1;
        }
    }

    fn fill(&mut self, draw: imaging::FillRef<'_>) {
        if let peniko::BrushRef::Image(image_brush) = draw.brush {
            let placement =
                draw.transform * draw.brush_transform.unwrap_or(Affine::IDENTITY);
            self.draw_image(placement, image_brush);
            return;
        }
        let peniko::BrushRef::Solid(color) = draw.brush else {
            self.warn_once("unsupported fill brush skipped in GDI v1");
            return;
        };
        let c = color.to_rgba8();
        if c.a != 255 {
            self.warn_once("solid fill alpha ignored in GDI v1");
        }
        let path = draw.shape.to_path(0.1);
        if path.elements().is_empty() {
            return;
        }
        unsafe {
            let brush = CreateSolidBrush(rgb(c.r, c.g, c.b));
            let old = SelectObject(self.hdc, brush as HGDIOBJ);
            SetPolyFillMode(
                self.hdc,
                if matches!(draw.fill_rule, peniko::Fill::EvenOdd) {
                    ALTERNATE
                } else {
                    WINDING
                },
            );
            self.set_world(draw.transform);
            BeginPath(self.hdc);
            self.path_els(path.elements());
            EndPath(self.hdc);
            FillPath(self.hdc);
            self.reset_world();
            SelectObject(self.hdc, old);
            DeleteObject(brush as HGDIOBJ);
        }
    }

    fn stroke(&mut self, draw: imaging::StrokeRef<'_>) {
        let peniko::BrushRef::Solid(color) = draw.brush else {
            self.warn_once("unsupported stroke brush skipped in GDI v1");
            return;
        };
        let c = color.to_rgba8();
        let path = draw.shape.to_path(0.1);
        if path.elements().is_empty() {
            return;
        }
        unsafe {
            // One end cap (GDI limitation): the START cap wins, matching
            // krilla's collapse in the PDF sink.
            let style: i32 = PS_GEOMETRIC
                | PS_JOIN_ROUND
                | PS_ENDCAP_ROUND
                | if draw.stroke.dash_pattern.is_empty() {
                    0
                } else {
                    PS_USERSTYLE
                };
            let dashes: Vec<u32> = if draw.stroke.dash_pattern.is_empty() {
                Vec::new()
            } else {
                draw.stroke
                    .dash_pattern
                    .iter()
                    .map(|d| (d.max(0.5) * SUBPIXEL).round() as u32)
                    .collect()
            };
            let logbrush = LOGBRUSH {
                lbStyle: BS_SOLID,
                lbColor: rgb(c.r, c.g, c.b),
                lbHatch: 0,
            };
            let pen = ExtCreatePen(
                style as u32,
                (draw.stroke.width.max(1.0) * SUBPIXEL).round() as u32,
                &logbrush,
                dashes.len() as u32,
                if dashes.is_empty() {
                    std::ptr::null()
                } else {
                    dashes.as_ptr()
                },
            );
            if pen.is_null() {
                self.warn_once("ExtCreatePen failed; stroke skipped");
                return;
            }
            let old = SelectObject(self.hdc, pen as HGDIOBJ);
            self.set_world(draw.transform);
            BeginPath(self.hdc);
            self.path_els(path.elements());
            EndPath(self.hdc);
            StrokePath(self.hdc);
            self.reset_world();
            SelectObject(self.hdc, old);
            DeleteObject(pen);
        }
    }

    fn glyph_run(
        &mut self,
        draw: imaging::GlyphRunRef<'_>,
        glyphs: &mut dyn Iterator<Item = imaging::record::Glyph>,
    ) {
        let peniko::BrushRef::Solid(color) = draw.brush else {
            self.warn_once("unsupported glyph brush skipped in GDI v1");
            return;
        };
        let c = color.to_rgba8();
        let old_font = match self.fonts.select_face(self.hdc, draw.font, draw.font_size) {
            Ok(old) => old,
            Err(_) => {
                self.warn_once("font selection failed; glyph run skipped");
                return;
            }
        };
        unsafe {
            SetTextColor(self.hdc, rgb(c.r, c.g, c.b));
            SetTextAlign(self.hdc, (TA_LEFT | TA_BASELINE) as u32);
            SetBkMode(self.hdc, TRANSPARENT as i32);
        }
        let run_transform = draw.transform;
        let glyph_transform = draw.glyph_transform;
        for glyph in glyphs {
            let pos =
                run_transform * Point::new(f64::from(glyph.x), f64::from(glyph.y));
            // Per-glyph transforms anchor at each glyph's own origin (the
            // same convention as the PDF sink's anchored push).
            let full = match glyph_transform {
                Some(t) => {
                    let anchor = Affine::translate((pos.x, pos.y));
                    run_transform * anchor * t * anchor.inverse()
                }
                None => run_transform,
            };
            self.set_world(full);
            let id = glyph.id as u16;
            unsafe {
                ExtTextOutW(
                    self.hdc,
                    Self::emit(pos.x),
                    Self::emit(pos.y),
                    ETO_GLYPH_INDEX,
                    std::ptr::null(),
                    &id,
                    1,
                    std::ptr::null(),
                );
            }
        }
        self.reset_world();
        unsafe {
            SelectObject(self.hdc, old_font);
        }
    }

    fn blurred_rounded_rect(&mut self, _draw: imaging::BlurredRoundedRect) {
        self.warn_once("blurred rounded rects skipped in GDI v1");
    }
}
