//! CUPS sink (Linux, design §6.1/D3): PDF payload + `cupsPrintFile` with
//! job options (media/sides/ColorModel/copies/Collate). Implementation
//! lands with the FFI module; these are the backend dispatch seams.

use super::{PrinterCaps, PrinterInfo};
use crate::job::SharedJob;
use crate::settings::PrintSettings;
use std::path::Path;

pub(crate) mod options;

pub(crate) fn list_printers_impl() -> Vec<PrinterInfo> {
    Vec::new()
}

pub(crate) fn printer_caps_impl(_name: Option<&str>) -> Option<PrinterCaps> {
    None
}

pub(crate) fn run_cups_job(
    pdf: &[u8],
    settings: &PrintSettings,
    title: &str,
    output_file: Option<&Path>,
    job: &SharedJob,
) -> Result<(), String> {
    let _ = (pdf, settings, title, output_file, job);
    Err("cups sink not implemented yet".to_string())
}
