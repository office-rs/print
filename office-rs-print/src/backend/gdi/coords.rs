//! GDI coordinate mapping (design §6.1): scene pixels are 96 dpi, y-down;
//! GDI `MM_TEXT` is also y-down device units — so the page transform is a
//! plain scale (device_dpi/96) plus a translate aligning the SCENE page
//! origin with the physical sheet corner (printable-area offset
//! subtracted). All functions are pure (unit-testable without a DC).

use imaging::kurbo::Affine;
use windows_sys::Win32::Graphics::Gdi::XFORM;

/// The world transform mapping one page's scene px onto the physical sheet:
/// `device = (px * scale) - offset` where `scale = dpi / 96` and the offset
/// is `PHYSICALOFFSETX/Y` (the unprintable margin), so scene (0,0) lands on
/// the sheet corner rather than the printable-area corner — the page's own
/// margins are painted inside the scene.
pub(crate) fn page_transform(dpi_x: f32, dpi_y: f32, offset_x: i32, offset_y: i32) -> XFORM {
    let (sx, sy) = (dpi_x / 96.0, dpi_y / 96.0);
    XFORM {
        eM11: sx,
        eM12: 0.0,
        eM21: 0.0,
        eM22: sy,
        eDx: -offset_x as f32,
        eDy: -offset_y as f32,
    }
}

/// Compose a scene-space affine (a draw's own transform) with the page
/// world transform, returning ONE GDI `XFORM` (`page * a`) so the replay
/// issues at most one `SetWorldTransform` per draw. Coefficient layout is
/// identical between kurbo (`[a, b, c, d, e, f]`, x' = a·x + c·y + e) and
/// GDI (`[eM11, eM12, eM21, eM22, eDx, eDy]`).
pub(crate) fn affine_to_xform(a: &Affine) -> XFORM {
    let c = a.as_coeffs();
    XFORM {
        eM11: c[0] as f32,
        eM12: c[1] as f32,
        eM21: c[2] as f32,
        eM22: c[3] as f32,
        eDx: c[4] as f32,
        eDy: c[5] as f32,
    }
}

/// Logical font height for `CreateFontIndirectW` under the page world
/// transform: the IR's `font_size` is px per em at 96 dpi and the world
/// transform carries the dpi scaling, so `lfHeight` is simply the px size.
/// Negative = character height (em box), matching how the renderer sizes
/// glyphs.
pub(crate) fn lf_height(font_size_px: f32) -> i32 {
    -(font_size_px.round().max(1.0) as i32)
}

/// Swizzle RGBA8 pixels into the BGRA order GDI's 32-bpp BI_RGB
/// `StretchDIBits` expects (the scene's image cache stores RGBA8).
pub(crate) fn rgba_to_bgra(rgba: &[u8]) -> Vec<u8> {
    let mut bgra = rgba.to_vec();
    for px in bgra.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    bgra
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_at_96_dpi_zero_offset() {
        let t = page_transform(96.0, 96.0, 0, 0);
        let XFORM {
            eM11,
            eM12,
            eM21,
            eM22,
            eDx,
            eDy,
        } = t;
        assert_eq!((eM11, eM12, eM21, eM22, eDx, eDy), (1.0, 0.0, 0.0, 1.0, 0.0, 0.0));
    }

    #[test]
    fn scales_and_offsets() {
        // 600 dpi: scale 6.25; a 12-dot unprintable margin shifts origin.
        let t = page_transform(600.0, 300.0, 12, -8);
        assert_eq!(t.eM11, 6.25);
        assert_eq!(t.eM22, 3.125);
        assert_eq!(t.eDx, -12.0);
        assert_eq!(t.eDy, 8.0);
        // Scene (0, 0) maps to device (-12, 8) — the sheet corner sits
        // above/left of the printable origin.
        let x = t.eM11 * 0.0 + t.eDx;
        let y = t.eM22 * 0.0 + t.eDy;
        assert_eq!((x, y), (-12.0, 8.0));
        // A 96-px inch maps to 600 device dots.
        assert_eq!(t.eM11 * 96.0, 600.0);
    }

    #[test]
    fn affine_maps_coefficient_positions() {
        let a = Affine::new([2.0, 0.5, 0.0, 3.0, 10.0, 20.0]);
        let t = affine_to_xform(&a);
        assert_eq!(t.eM11, 2.0);
        assert_eq!(t.eM12, 0.5);
        assert_eq!(t.eM21, 0.0);
        assert_eq!(t.eM22, 3.0);
        assert_eq!(t.eDx, 10.0);
        assert_eq!(t.eDy, 20.0);
    }

    #[test]
    fn lf_height_is_negative_char_height() {
        assert_eq!(lf_height(16.0), -16);
        assert_eq!(lf_height(16.5), -17); // rounded, never zero
        assert_eq!(lf_height(0.2), -1);
    }

    #[test]
    fn rgba_swizzles_to_bgra() {
        assert_eq!(rgba_to_bgra(&[1, 2, 3, 4]), vec![3, 2, 1, 4]);
        assert_eq!(rgba_to_bgra(&[]), Vec::<u8>::new());
        let multi = rgba_to_bgra(&[10, 20, 30, 40, 50, 60, 70, 80]);
        assert_eq!(multi, vec![30, 20, 10, 40, 70, 60, 50, 80]);
    }
}
