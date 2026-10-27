//! Backend layer (design §6.1): printer discovery, submission and the
//! platform sinks — everything under `#[cfg(not(target_arch = "wasm32"))]`.
//! Payload routing: Windows consumes `Pages` through the GDI vector sink,
//! Linux consumes `Pdf` through CUPS; mismatches fail the job (never
//! panic). Page-range/parity planning is host-side — the payload carries
//! exactly the planned pages; copies/collate remain JOB parameters (D10).

use crate::job::{JobHandle, JobState, SharedJob};
use crate::payload::PrintPayload;
use crate::settings::PrintSettings;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

#[cfg(windows)]
pub(crate) mod devmode;
#[cfg(windows)]
pub(crate) mod discover_win;
#[cfg(windows)]
pub(crate) mod gdi;
#[cfg(target_os = "linux")]
pub(crate) mod cups;

/// One discovered printer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrinterInfo {
    pub name: String,
    pub is_default: bool,
    pub port: Option<String>,
}

/// What a printer/driver reports supporting (feeds panel control states).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrinterCaps {
    pub duplex: bool,
    pub color: bool,
    pub papers: Vec<crate::settings::Paper>,
    pub copies_max: u32,
}

/// Enumerate installed printers (native only; the web profile has no
/// discovery — the SDK's services implementation never calls this).
pub fn list_printers() -> Vec<PrinterInfo> {
    #[cfg(windows)]
    {
        discover_win::list_printers_impl()
    }
    #[cfg(target_os = "linux")]
    {
        crate::backend::cups::list_printers_impl()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        Vec::new()
    }
}

/// Capabilities of one printer (`None` = unknown printer).
pub fn printer_capabilities(name: Option<&str>) -> Option<PrinterCaps> {
    #[cfg(windows)]
    {
        discover_win::printer_caps_impl(name)
    }
    #[cfg(not(windows))]
    {
        let _ = name;
        None
    }
}

/// Print a Pages (vector Scene-IR) payload: the payload renders/spools on a
/// worker thread; the returned handle reports progress and carries
/// cancellation.
pub fn print_pages(payload: PrintPayload, settings: &PrintSettings, title: &str) -> JobHandle {
    print_with_output(payload, settings, title, None)
}

/// Print a PDF payload: on Linux CUPS spools it natively; on Windows the
/// system PDF renderer (WinRT `Windows.Data.Pdf`) rasterizes each page at
/// device DPI through the GDI pipeline — the cross-process host route
/// (tauri): a PDF is a few MB vs a serialized Scene payload's tens of MB
/// of embedded font bytes.
pub fn print_pdf(bytes: &[u8], settings: &PrintSettings, title: &str) -> JobHandle {
    print_with_output(PrintPayload::Pdf { bytes: bytes.to_vec() }, settings, title, None)
}

/// Test/diagnostic variant routing the job to a file instead of the port.
pub fn print_pdf_to_file(
    bytes: &[u8],
    settings: &PrintSettings,
    title: &str,
    output_file: Option<std::path::PathBuf>,
) -> JobHandle {
    print_with_output(PrintPayload::Pdf { bytes: bytes.to_vec() }, settings, title, output_file)
}

/// Test/diagnostic variant routing the job to a file instead of the port:
/// GDI sets `DOCINFOW.lpszOutput` (e.g. "Microsoft Print to PDF"
/// materializes a real PDF — the manual-test channel); CUPS writes the
/// spool file and skips submission. `None` degrades to [`print_pages`].
pub fn print_pages_to_file(
    payload: PrintPayload,
    settings: &PrintSettings,
    title: &str,
    output_file: Option<std::path::PathBuf>,
) -> JobHandle {
    print_with_output(payload, settings, title, output_file)
}

fn print_with_output(
    payload: PrintPayload,
    settings: &PrintSettings,
    title: &str,
    output_file: Option<std::path::PathBuf>,
) -> JobHandle {
    let pages_total = match &payload {
        PrintPayload::Pages { pages } => pages.len(),
        PrintPayload::Pdf { .. } => 0,
    };
    let shared = Arc::new(SharedJob::new(pages_total));
    let worker = Arc::clone(&shared);
    let settings = settings.clone();
    let title = title.to_string();
    let thread = std::thread::spawn(move || {
        let result = dispatch(&payload, &settings, &title, output_file.as_deref(), &worker);
        match result {
            Ok(()) => {
                if worker.is_cancelled() {
                    worker.update(Some(JobState::Aborted), None, None);
                } else {
                    worker.update(Some(JobState::Done), None, None);
                }
            }
            Err(message) => {
                let state = if worker.is_cancelled() {
                    JobState::Aborted
                } else {
                    JobState::Failed
                };
                worker.update(Some(state), None, Some(message));
            }
        }
    });
    let handle = JobHandle::new(shared, thread);
    crate::job::register_worker(handle.clone());
    handle
}

/// Route the payload to the platform sink. Pure-routing failures (wrong
/// payload for the OS) produce `Err` jobs.
fn dispatch(
    payload: &PrintPayload,
    settings: &PrintSettings,
    title: &str,
    output_file: Option<&Path>,
    job: &SharedJob,
) -> Result<(), String> {
    match payload {
        PrintPayload::Pages { pages } => {
            if pages.is_empty() {
                return Err("nothing to print: empty page list".to_string());
            }
            #[cfg(windows)]
            {
                crate::backend::gdi::run_gdi_job(pages, settings, title, output_file, job)
            }
            #[cfg(not(windows))]
            {
                let _ = (settings, title, output_file, job);
                Err("the Pages payload is only supported on Windows (GDI)".to_string())
            }
        }
        PrintPayload::Pdf { bytes } => {
            if bytes.is_empty() {
                return Err("nothing to print: empty PDF".to_string());
            }
            #[cfg(target_os = "linux")]
            {
                crate::backend::cups::run_cups_job(bytes, settings, title, output_file, job)
            }
            #[cfg(windows)]
            {
                crate::backend::gdi::run_gdi_pdf_job(bytes, settings, title, output_file, job)
            }
            #[cfg(not(any(windows, target_os = "linux")))]
            {
                let _ = (settings, title, output_file, job);
                Err("PDF printing is supported on Windows and Linux only".to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_pages_payload_fails_the_job() {
        let handle = print_pages(
            PrintPayload::Pages { pages: vec![] },
            &PrintSettings::default(),
            "t",
        );
        let snap = handle.wait();
        assert_eq!(snap.state, JobState::Failed);
        assert!(snap.error.unwrap().contains("empty page list"));
    }

    #[test]
    fn empty_pdf_payload_fails_the_job() {
        let handle = print_pages(
            PrintPayload::Pdf { bytes: vec![] },
            &PrintSettings::default(),
            "t",
        );
        let snap = handle.wait();
        assert_eq!(snap.state, JobState::Failed);
        assert!(snap.error.unwrap().contains("empty PDF"));
    }

    #[cfg(not(windows))]
    #[test]
    fn pages_payload_on_non_windows_is_routing_error() {
        let page = crate::payload::PageScene {
            paper: crate::settings::Paper::A4,
            orientation: crate::settings::Orientation::Portrait,
            width_px: 794.0,
            height_px: 1123.0,
            scene: Default::default(),
        };
        let handle = print_pages(
            PrintPayload::Pages { pages: vec![page] },
            &PrintSettings::default(),
            "t",
        );
        let snap = handle.wait();
        assert_eq!(snap.state, JobState::Failed);
        assert!(snap.error.unwrap().contains("Windows"));
    }
}
