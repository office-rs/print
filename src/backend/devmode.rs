//! DEVMODE override (design §6.1, Windows): map [`PrintSettings`] onto the
//! driver's default `DEVMODEW` before `CreateDCW`. Copies/collate/duplex/
//! color/orientation/paper are JOB parameters (D10) — the DEVMODE is the
//! only place they are set; the submission side emulates them by replay
//! loops only when the driver lacks the field.

use crate::settings::{ColorMode, Duplex, Orientation, PrintSettings};
use windows_sys::Win32::Graphics::Gdi::{
    DMCOLLATE_FALSE, DMCOLLATE_TRUE, DMCOLOR_COLOR, DMCOLOR_MONOCHROME, DMDUP_SIMPLEX,
    DMORIENT_LANDSCAPE, DMORIENT_PORTRAIT, DM_COLLATE, DM_COLOR, DM_COPIES, DM_DUPLEX,
    DM_ORIENTATION, DM_PAPERSIZE, DEVMODEW,
};

const DMDUP_VERTICAL: i16 = 2; // long edge
const DMDUP_HORIZONTAL: i16 = 3; // short edge

/// Apply print settings onto the driver's default DEVMODE. Only fields the
/// DEVMODE declares support for (via `dmFields`) get their bit set —
/// e.g. a driver without `DM_DUPLEX` keeps the field untouched and the
/// submit loop emulates duplex by replaying pages (spec §6.1).
pub(crate) fn apply_settings(dm: &mut DEVMODEW, s: &PrintSettings) {
    let s = s.clone().normalized();
    // Orientation must already be resolved (never Auto here — the payload
    // producer resolves it against the first page).
    let orientation = s.effective_orientation(false);
    // Orientation/paper/copies live in the Anonymous1 union; the driver
    // fills the s1 (printer) arm, which is the only arm we touch.
    unsafe {
        if dm.dmFields & DM_ORIENTATION != 0 || dm.Anonymous1.Anonymous1.dmOrientation != 0 {
            dm.dmFields |= DM_ORIENTATION;
            dm.Anonymous1.Anonymous1.dmOrientation = match orientation {
                Orientation::Portrait | Orientation::Auto => DMORIENT_PORTRAIT as i16,
                Orientation::Landscape => DMORIENT_LANDSCAPE as i16,
            };
        }
        if dm.dmFields & DM_PAPERSIZE != 0 || dm.Anonymous1.Anonymous1.dmPaperSize != 0 {
            dm.dmFields |= DM_PAPERSIZE;
            dm.Anonymous1.Anonymous1.dmPaperSize = s.paper.dmpaper() as i16;
        }
        if dm.dmFields & DM_COPIES != 0 || dm.Anonymous1.Anonymous1.dmCopies != 0 {
            dm.dmFields |= DM_COPIES;
            dm.Anonymous1.Anonymous1.dmCopies = s.copies as i16;
        }
    }
    if dm.dmFields & DM_COLOR != 0 || dm.dmColor != 0 {
        dm.dmFields |= DM_COLOR;
        dm.dmColor = match s.color {
            ColorMode::Color => DMCOLOR_COLOR,
            ColorMode::Grayscale => DMCOLOR_MONOCHROME,
        };
    }
    if dm.dmFields & DM_DUPLEX != 0 || dm.dmDuplex != 0 {
        dm.dmFields |= DM_DUPLEX;
        dm.dmDuplex = match s.duplex {
            Duplex::Off => DMDUP_SIMPLEX,
            Duplex::LongEdge => DMDUP_VERTICAL,
            Duplex::ShortEdge => DMDUP_HORIZONTAL,
        };
    }
    if dm.dmFields & DM_COLLATE != 0 || dm.dmCollate != 0 {
        dm.dmFields |= DM_COLLATE;
        dm.dmCollate = if s.collate {
            DMCOLLATE_TRUE
        } else {
            DMCOLLATE_FALSE
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Paper, PrintSettings};

    /// A DEVMODE declaring the given supported fields, with the current
    /// values nonzero so "field present" is distinguishable from the bit.
    fn devmode(fields: u32) -> DEVMODEW {
        let mut dm: DEVMODEW = unsafe { std::mem::zeroed() };
        dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        dm.dmFields = fields;
        // Nonzero placeholders for every field we set, so the test can
        // verify each write happened (zero would be a legal portrait/1-copy
        // value and hide a missing write).
        if fields & DM_ORIENTATION != 0 {
            dm.Anonymous1.Anonymous1.dmOrientation = DMORIENT_LANDSCAPE as i16;
        }
        if fields & DM_PAPERSIZE != 0 {
            dm.Anonymous1.Anonymous1.dmPaperSize = 1; // Letter
        }
        if fields & DM_COPIES != 0 {
            dm.Anonymous1.Anonymous1.dmCopies = 7;
        }
        if fields & DM_COLOR != 0 {
            dm.dmColor = DMCOLOR_MONOCHROME;
        }
        if fields & DM_DUPLEX != 0 {
            dm.dmDuplex = DMDUP_SIMPLEX;
        }
        if fields & DM_COLLATE != 0 {
            dm.dmCollate = DMCOLLATE_FALSE;
        }
        dm
    }

    const ALL_FIELDS: u32 =
        DM_ORIENTATION | DM_PAPERSIZE | DM_COPIES | DM_COLOR | DM_DUPLEX | DM_COLLATE;

    #[test]
    fn applies_every_field() {
        let mut dm = devmode(ALL_FIELDS);
        let s = PrintSettings {
            copies: 2000, // clamps to 999
            collate: true,
            duplex: Duplex::ShortEdge,
            color: ColorMode::Color,
            paper: Paper::A4,
            orientation: Orientation::Portrait,
            ..PrintSettings::default()
        };
        apply_settings(&mut dm, &s);
        assert_eq!(unsafe { dm.Anonymous1.Anonymous1.dmOrientation }, DMORIENT_PORTRAIT as i16);
        assert_eq!(unsafe { dm.Anonymous1.Anonymous1.dmPaperSize }, Paper::A4.dmpaper() as i16);
        assert_eq!(unsafe { dm.Anonymous1.Anonymous1.dmCopies }, 999);
        assert_eq!(dm.dmColor, DMCOLOR_COLOR);
        assert_eq!(dm.dmDuplex, DMDUP_HORIZONTAL);
        assert_eq!(dm.dmCollate, DMCOLLATE_TRUE);
    }

    #[test]
    fn duplex_long_edge_maps_vertical() {
        let mut dm = devmode(DM_DUPLEX);
        let s = PrintSettings {
            duplex: Duplex::LongEdge,
            ..PrintSettings::default()
        };
        apply_settings(&mut dm, &s);
        assert_eq!(dm.dmDuplex, DMDUP_VERTICAL);
    }

    #[test]
    fn unsupported_field_stays_untouched() {
        // No DM_DUPLEX declared → the submit side emulates by replay; the
        // DEVMODE must keep the driver default untouched.
        let mut dm = devmode(ALL_FIELDS & !DM_DUPLEX);
        assert_eq!(dm.dmDuplex, 0);
        let s = PrintSettings {
            duplex: Duplex::LongEdge,
            ..PrintSettings::default()
        };
        apply_settings(&mut dm, &s);
        assert_eq!(dm.dmDuplex, 0, "unsupported field untouched");
        assert_eq!(dm.dmFields & DM_DUPLEX, 0, "field bit not invented");
    }
}
