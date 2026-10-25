//! CUPS job options (design §6.1/D3): map [`PrintSettings`] onto IPP/CUPS
//! option pairs (`media`, `sides`, `ColorModel`, `copies`, `Collate`).
//! Pure and unit-testable; the sink adds them with `cupsAddOption` and the
//! print system silently ignores unsupported pairs.

use crate::settings::{ColorMode, Duplex, PrintSettings};
use serde::{Deserialize, Serialize};

/// One `cupsAddOption` pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobOption {
    pub name: String,
    pub value: String,
}

/// Build the job options for one spool submission (D10: copies/collate are
/// job parameters; the PDF itself is a single copy).
pub(crate) fn job_options(s: &PrintSettings) -> Vec<JobOption> {
    let s = s.clone().normalized();
    vec![
        JobOption {
            name: "media".into(),
            value: s.paper.pwg_name().into(),
        },
        JobOption {
            name: "sides".into(),
            value: match s.duplex {
                Duplex::Off => "one-sided".to_string(),
                Duplex::LongEdge => "two-sided-long-edge".to_string(),
                Duplex::ShortEdge => "two-sided-short-edge".to_string(),
            },
        },
        JobOption {
            name: "ColorModel".into(),
            value: match s.color {
                ColorMode::Color => "RGB".to_string(),
                ColorMode::Grayscale => "Gray".to_string(),
            },
        },
        JobOption {
            name: "copies".into(),
            value: s.copies.to_string(),
        },
        JobOption {
            name: "Collate".into(),
            value: if s.collate { "True" } else { "False" }.to_string(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Paper, Parity};

    fn find<'a>(opts: &'a [JobOption], name: &str) -> &'a str {
        opts.iter()
            .find(|o| o.name == name)
            .unwrap_or_else(|| panic!("{name} missing"))
            .value
            .as_str()
    }

    #[test]
    fn maps_every_setting() {
        let s = PrintSettings {
            paper: Paper::Letter,
            duplex: Duplex::LongEdge,
            color: ColorMode::Grayscale,
            copies: 3,
            collate: true,
            ..PrintSettings::default()
        };
        let opts = job_options(&s);
        assert_eq!(find(&opts, "media"), "na_letter_8.5x11in");
        assert_eq!(find(&opts, "sides"), "two-sided-long-edge");
        assert_eq!(find(&opts, "ColorModel"), "Gray");
        assert_eq!(find(&opts, "copies"), "3");
        assert_eq!(find(&opts, "Collate"), "True");
    }

    #[test]
    fn simplex_color_defaults() {
        let s = PrintSettings {
            duplex: Duplex::Off,
            color: ColorMode::Color,
            collate: false,
            parity: Parity::Even, // parity never reaches the spool
            ..PrintSettings::default()
        };
        let opts = job_options(&s);
        assert_eq!(find(&opts, "sides"), "one-sided");
        assert_eq!(find(&opts, "ColorModel"), "RGB");
        assert_eq!(find(&opts, "Collate"), "False");
        assert!(!opts.iter().any(|o| o.name == "page-ranges"), "planning is host-side");
    }

    #[test]
    fn copies_clamp_applies() {
        let s = PrintSettings {
            copies: 5000,
            ..PrintSettings::default()
        };
        assert_eq!(find(&job_options(&s), "copies"), "999");
    }
}
