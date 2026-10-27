//! Print settings model (design §6.1): the single source of truth shared by
//! every host UI (SDK DOM panel, xilem dialog, tauri IPC) and both
//! backends (DEVMODE on Windows, CUPS job options on Linux).
//!
//! Clamping discipline (spec §7): copies 1..=999, spec strings ≤500 chars,
//! total page cap 5000 — clamp or reject up front, never fail after
//! allocating.

use serde::{Deserialize, Serialize};

/// Paper size, v1 set (PWG 5101.1 media names + Windows DMPAPER ids).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Paper {
    A3,
    A4,
    A5,
    B4,
    B5,
    Letter,
    Legal,
}

impl Paper {
    /// PWG 5101.1 self-describing media name (the CUPS `media` option).
    pub fn pwg_name(self) -> &'static str {
        match self {
            Paper::A3 => "iso_a3_297x420mm",
            Paper::A4 => "iso_a4_210x297mm",
            Paper::A5 => "iso_a5_148x210mm",
            Paper::B4 => "iso_b4_250x353mm",
            Paper::B5 => "iso_b5_176x250mm",
            Paper::Letter => "na_letter_8.5x11in",
            Paper::Legal => "na_legal_8.5x14in",
        }
    }

    /// Windows `DEVMODE.dmPaperSize` constant (DMPAPER_*).
    pub fn dmpaper(self) -> u16 {
        match self {
            Paper::Letter => 1,  // DMPAPER_LETTER
            Paper::Legal => 4,   // DMPAPER_LEGAL
            Paper::A3 => 8,      // DMPAPER_A3
            Paper::A4 => 9,      // DMPAPER_A4
            Paper::A5 => 11,     // DMPAPER_A5
            Paper::B4 => 12,     // DMPAPER_B4
            Paper::B5 => 13,     // DMPAPER_B5
        }
    }

    /// Portrait media size in millimeters (width × height) — the paper
    /// authority (spec §7: document sections rule rendering; this feeds
    /// paper-grouped job splitting).
    pub fn size_mm(self) -> (f32, f32) {
        match self {
            Paper::A3 => (297.0, 420.0),
            Paper::A4 => (210.0, 297.0),
            Paper::A5 => (148.0, 210.0),
            Paper::B4 => (250.0, 353.0),
            Paper::B5 => (176.0, 250.0),
            Paper::Letter => (215.9, 279.4),
            Paper::Legal => (215.9, 355.6),
        }
    }

    /// Map a raw DMPAPER value back to [`Paper`] (discovery capability
    /// lists); `None` for values outside the v1 set.
    pub fn from_dmpaper(raw: u16) -> Option<Paper> {
        Some(match raw {
            1 => Paper::Letter,
            4 => Paper::Legal,
            8 => Paper::A3,
            9 => Paper::A4,
            11 => Paper::A5,
            12 => Paper::B4,
            13 => Paper::B5,
            _ => return None,
        })
    }
}

/// Two-sided printing mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Duplex {
    #[default]
    Off,
    /// Flip on the long edge (book).
    LongEdge,
    /// Flip on the short edge (tablet).
    ShortEdge,
}

/// Color mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ColorMode {
    #[default]
    Color,
    Grayscale,
}

/// Sheet orientation. `Auto` resolves per the first page's aspect once the
/// payload is known (never reaches a backend unresolved).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[derive(Default)]
pub enum Orientation {
    #[default]
    Portrait,
    Landscape,
    Auto,
}


/// Page parity selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Parity {
    #[default]
    All,
    Odd,
    Even,
}

/// Page range selection. `Custom` carries a spec string like `"1-3,5"`
/// (parsed by [`crate::page_plan::parse_page_spec`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
#[derive(Default)]
pub enum PageRange {
    #[default]
    All,
    Current,
    Custom { spec: String },
}


/// Print job settings (design §6.1 v1 fields). Copy/parity/duplex/color
/// are JOB parameters only (D10) — they never multiply the payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrintSettings {
    /// `None` = system default printer.
    pub printer: Option<String>,
    /// 1..=999 (clamped by [`PrintSettings::normalized`]).
    pub copies: u32,
    /// Collate copies (1,2,3,1,2,3) vs page-major (1,1,2,2,3,3).
    pub collate: bool,
    pub duplex: Duplex,
    pub color: ColorMode,
    pub paper: Paper,
    pub orientation: Orientation,
    pub range: PageRange,
    pub parity: Parity,
}

impl Default for PrintSettings {
    fn default() -> Self {
        Self {
            printer: None,
            copies: 1,
            collate: true,
            duplex: Duplex::Off,
            color: ColorMode::Color,
            paper: Paper::A4,
            orientation: Orientation::Portrait,
            range: PageRange::All,
            parity: Parity::All,
        }
    }
}

/// Validation problems a settings editor can surface before submit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsError {
    RangeSpecTooLong { len: usize },
    InvalidRangeSpec { reason: String },
}

impl PrintSettings {
    pub const MAX_COPIES: u32 = 999;

    /// Clamp numeric fields into their legal ranges (idempotent).
    pub fn normalized(mut self) -> Self {
        self.copies = self.copies.clamp(1, Self::MAX_COPIES);
        self
    }

    /// Resolve `Orientation::Auto` against the first page's aspect
    /// (landscape = width > height); other orientations pass through.
    pub fn effective_orientation(&self, first_page_landscape: bool) -> Orientation {
        match self.orientation {
            Orientation::Auto => {
                if first_page_landscape {
                    Orientation::Landscape
                } else {
                    Orientation::Portrait
                }
            }
            other => other,
        }
    }
}

/// Validate what can be checked without a payload: the custom range spec
/// (length + syntax). Numeric clamping is [`PrintSettings::normalized`]'s
/// job; page-count bounds are checked at resolve time (page_plan).
pub fn validate_settings(s: &PrintSettings) -> Vec<SettingsError> {
    let mut errors = Vec::new();
    if let PageRange::Custom { spec } = &s.range {
        match crate::page_plan::validate_page_spec(spec) {
            Ok(()) => {}
            Err(reason) => {
                let too_long = spec.chars().count() > crate::page_plan::MAX_SPEC_LEN;
                errors.push(if too_long {
                    SettingsError::RangeSpecTooLong {
                        len: spec.chars().count(),
                    }
                } else {
                    SettingsError::InvalidRangeSpec { reason }
                });
            }
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_clamps_to_1_999() {
        for (input, expect) in [(0, 1), (1, 1), (500, 500), (1000, 999), (u32::MAX, 999)] {
            let s = PrintSettings {
                copies: input,
                ..PrintSettings::default()
            };
            assert_eq!(s.normalized().copies, expect, "copies {input}");
        }
    }

    #[test]
    fn paper_mappings_snapshot() {
        let expect = [
            (Paper::A3, "iso_a3_297x420mm", 8u16, (297.0, 420.0)),
            (Paper::A4, "iso_a4_210x297mm", 9, (210.0, 297.0)),
            (Paper::A5, "iso_a5_148x210mm", 11, (148.0, 210.0)),
            (Paper::B4, "iso_b4_250x353mm", 12, (250.0, 353.0)),
            (Paper::B5, "iso_b5_176x250mm", 13, (176.0, 250.0)),
            (Paper::Letter, "na_letter_8.5x11in", 1, (215.9, 279.4)),
            (Paper::Legal, "na_legal_8.5x14in", 4, (215.9, 355.6)),
        ];
        for (paper, pwg, dm, mm) in expect {
            assert_eq!(paper.pwg_name(), pwg);
            assert_eq!(paper.dmpaper(), dm);
            assert_eq!(paper.size_mm(), mm);
            assert_eq!(Paper::from_dmpaper(dm), Some(paper));
        }
        assert_eq!(Paper::from_dmpaper(0), None);
    }

    #[test]
    fn overlong_custom_spec_reports_too_long() {
        let spec = "1".repeat(crate::page_plan::MAX_SPEC_LEN + 1);
        let s = PrintSettings {
            range: PageRange::Custom { spec },
            ..PrintSettings::default()
        };
        let errors = validate_settings(&s);
        assert!(matches!(
            errors.as_slice(),
            [SettingsError::RangeSpecTooLong { len }] if *len == crate::page_plan::MAX_SPEC_LEN + 1
        ));
    }

    #[test]
    fn invalid_custom_spec_reports_reason() {
        let s = PrintSettings {
            range: PageRange::Custom { spec: "1-3,x".into() },
            ..PrintSettings::default()
        };
        assert!(matches!(
            validate_settings(&s).as_slice(),
            [SettingsError::InvalidRangeSpec { .. }]
        ));
    }

    #[test]
    fn valid_or_absent_spec_yields_no_errors() {
        assert!(validate_settings(&PrintSettings::default()).is_empty());
        let ok = PrintSettings {
            range: PageRange::Custom { spec: "1-3,5".into() },
            ..PrintSettings::default()
        };
        assert!(validate_settings(&ok).is_empty());
    }

    #[test]
    fn auto_orientation_resolves_from_first_page() {
        let s = PrintSettings {
            orientation: Orientation::Auto,
            ..PrintSettings::default()
        };
        assert_eq!(s.effective_orientation(true), Orientation::Landscape);
        assert_eq!(s.effective_orientation(false), Orientation::Portrait);
        let fixed = PrintSettings {
            orientation: Orientation::Landscape,
            ..PrintSettings::default()
        };
        assert_eq!(fixed.effective_orientation(false), Orientation::Landscape);
    }

    #[test]
    fn settings_json_round_trip() {
        let s = PrintSettings {
            printer: Some("Microsoft Print to PDF".into()),
            copies: 3,
            collate: false,
            duplex: Duplex::LongEdge,
            color: ColorMode::Grayscale,
            paper: Paper::Legal,
            orientation: Orientation::Landscape,
            range: PageRange::Custom { spec: "1-3,5".into() },
            parity: Parity::Odd,
        }
        .normalized();
        let json = serde_json::to_string(&s).unwrap();
        let back: PrintSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        // Tagged enum shape (tauri IPC friendliness).
        assert!(json.contains(r#""kind":"Custom""#), "{json}");
    }
}
