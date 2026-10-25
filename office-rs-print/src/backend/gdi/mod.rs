//! GDI vector sink (Windows, design §6.1/D2): Scene-IR replay through
//! GDI — pure CPU, no GPU/Vello. Doc comments on the submodules; the job
//! pipeline lands with the replay implementation.

use crate::job::SharedJob;
use crate::payload::PageScene;
use crate::settings::PrintSettings;
use std::path::Path;

pub(crate) mod coords;
pub(crate) mod font_name;

/// Spool one job: DEVMODE override → CreateDCW → StartDocW → per-page
/// replay (cancel-checked) → EndDoc. Implemented with the replay module;
/// this signature is the backend dispatch seam.
pub(crate) fn run_gdi_job(
    pages: &[PageScene],
    settings: &PrintSettings,
    title: &str,
    output_file: Option<&Path>,
    job: &SharedJob,
) -> Result<(), String> {
    let _ = (pages, settings, title, output_file, job);
    Err("gdi sink not implemented yet".to_string())
}
