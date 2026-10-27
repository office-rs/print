//! GDI vector sink (Windows, design §6.1/D2): Scene-IR replay through
//! GDI — pure CPU, no GPU/Vello. Job pipeline: `OpenPrinterW` →
//! `DocumentPropertiesW` (driver default DEVMODE) → [`devmode::apply_settings`]
//! → `CreateDCW` → `SetAbortProc` → `StartDocW` → collate/copy loop with
//! `StartPage`/replay/`EndPage` (cancel checked per page) → `EndDoc`.
//! Copies/collate ride the DEVMODE when the driver declares the fields;
//! otherwise this side emulates them by replay order (D10: the payload
//! itself is never multiplied).

use super::devmode::apply_settings;
use crate::job::{JobState, SharedJob};
use crate::page_plan::collate_plan;
use crate::payload::PageScene;
use crate::settings::PrintSettings;
use std::path::Path;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Graphics::Gdi::{
    CreateDCW, DMCOLLATE_FALSE, DM_COLLATE, DM_COPIES, DM_IN_BUFFER, DM_OUT_BUFFER, DEVMODEW,
};
use windows_sys::Win32::Graphics::Printing::{
    DocumentPropertiesW, OpenPrinterW, PRINTER_DEFAULTSW,
};
use windows_sys::Win32::Graphics::Gdi as gdi;
use windows_sys::Win32::Storage::Xps::{
    AbortDoc, EndDoc, EndPage, SetAbortProc, StartDocW, StartPage, DOCINFOW,
};

pub(crate) mod coords;
pub(crate) mod font_name;
pub(crate) mod replay;

/// Pure: the page replay order for the copies/collate settings.
/// `driver_copies`/`driver_collate` reflect the DEVMODE's declared support
/// (when both hold, the driver multiplexes and we replay ONE copy).
pub(crate) fn gdi_page_sequence(
    pages: usize,
    copies: u32,
    collate: bool,
    driver_copies: bool,
    driver_collate: bool,
) -> Vec<usize> {
    let all: Vec<usize> = (0..pages).collect();
    if driver_copies && (collate || driver_collate || copies == 1) {
        return all; // driver handles copy multiplexing (incl. non-collate)
    }
    if driver_copies && !collate && !driver_collate {
        // Driver does copies but cannot collate AND we want page-major —
        // degenerate to per-copy emulation for exactness.
        let mut out = Vec::new();
        for page in &all {
            for _ in 0..copies {
                out.push(*page);
            }
        }
        return out;
    }
    // No driver copies: emulate by replay order (collate_plan is
    // 1-based; feed it 1-based and drop back to indices).
    let one_based: Vec<usize> = (1..=pages).collect();
    collate_plan(&one_based, copies, collate)
        .into_iter()
        .flatten()
        .map(|p| p - 1)
        .collect()
}

unsafe extern "system" fn abort_proc(_hdc: gdi::HDC, _code: i32) -> i32 {
    // Returning 0 cancels the job; we drive cancellation through the
    // SharedJob flag instead, so always continue.
    1
}

/// PDF-payload job (Task 2 wires the WinRT renderer; the stub keeps the
/// dispatch shape in place).
pub(crate) fn run_gdi_pdf_job(
    _pdf: &[u8],
    _settings: &PrintSettings,
    _title: &str,
    _output_file: Option<&Path>,
    _job: &SharedJob,
) -> Result<(), String> {
    Err("gdi pdf sink not implemented yet".to_string())
}

pub(crate) fn run_gdi_job(
    pages: &[PageScene],
    settings: &PrintSettings,
    title: &str,
    output_file: Option<&Path>,
    job: &SharedJob,
) -> Result<(), String> {
    // Resolve the target printer (default when None).
    let printer_name = match &settings.printer {
        Some(name) => name.clone(),
        None => super::discover_win::default_printer_name()
            .ok_or_else(|| "no default printer".to_string())?,
    };
    let mut warnings: Vec<String> = Vec::new();
    unsafe {
        // ── OpenPrinterW + driver default DEVMODE ──────────────────────
        let mut handle: HANDLE = std::ptr::null_mut();
        let wide_name: Vec<u16> = printer_name.encode_utf16().chain(std::iter::once(0)).collect();
        let defaults = PRINTER_DEFAULTSW {
            DesiredAccess: 0,
            pDatatype: std::ptr::null_mut(),
            pDevMode: std::ptr::null_mut(),
        };
        if OpenPrinterW(
            wide_name.as_ptr(),
            &mut handle,
            &defaults,
        ) == 0
        {
            return Err(format!("OpenPrinterW failed for {printer_name:?}"));
        }
        // Size probe then fetch the driver default DEVMODE.
        let needed = DocumentPropertiesW(
            std::ptr::null_mut(),
            handle,
            wide_name.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null(),
            0,
        );
        if needed < 0 {
            CloseHandle(handle);
            return Err("DocumentPropertiesW size probe failed".into());
        }
        let mut devmode_buf = vec![0u8; needed as usize];
        let devmode = devmode_buf.as_mut_ptr() as *mut DEVMODEW;
        if DocumentPropertiesW(
            std::ptr::null_mut(),
            handle,
            wide_name.as_ptr(),
            devmode,
            std::ptr::null(),
            DM_OUT_BUFFER,
        ) < 0
        {
            CloseHandle(handle);
            return Err("DocumentPropertiesW fetch failed".into());
        }
        let driver_copies = (*devmode).dmFields & DM_COPIES != 0;
        let driver_collate =
            (*devmode).dmFields & DM_COLLATE != 0 && (*devmode).dmCollate != DMCOLLATE_FALSE;
        // Apply settings in place.
        apply_settings(&mut *devmode, settings);
        // Push the modified DEVMODE back through the driver (validation +
        // dependent fields), then create the DC.
        if DocumentPropertiesW(
            std::ptr::null_mut(),
            handle,
            wide_name.as_ptr(),
            devmode,
            devmode,
            DM_IN_BUFFER | DM_OUT_BUFFER,
        ) < 0
        {
            CloseHandle(handle);
            return Err("DocumentPropertiesW merge failed".into());
        }
        let hdc = CreateDCW(
            std::ptr::null(),
            wide_name.as_ptr(),
            std::ptr::null(),
            devmode,
        );
        CloseHandle(handle);
        if hdc.is_null() {
            return Err("CreateDCW failed".into());
        }

        // ── Page geometry ──────────────────────────────────────────────
        let dpi_x = gdi::GetDeviceCaps(hdc, gdi::LOGPIXELSX as i32);
        let dpi_y = gdi::GetDeviceCaps(hdc, gdi::LOGPIXELSY as i32);
        let offset_x = gdi::GetDeviceCaps(hdc, gdi::PHYSICALOFFSETX as i32);
        let offset_y = gdi::GetDeviceCaps(hdc, gdi::PHYSICALOFFSETY as i32);

        let doc_name: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        let output_wide: Option<Vec<u16>> = output_file.map(|p| {
            let s = p.to_string_lossy().into_owned();
            s.encode_utf16().chain(std::iter::once(0)).collect()
        });
        let docinfo = DOCINFOW {
            cbSize: std::mem::size_of::<DOCINFOW>() as i32,
            lpszDocName: doc_name.as_ptr(),
            lpszOutput: match &output_wide {
                Some(v) => v.as_ptr(),
                None => std::ptr::null(),
            },
            lpszDatatype: std::ptr::null(),
            fwType: 0,
        };

        SetAbortProc(hdc, Some(abort_proc));
        if StartDocW(hdc, &docinfo) <= 0 {
            gdi::DeleteDC(hdc);
            return Err("StartDocW failed".into());
        }

        job.update(Some(JobState::Rendering), None, None);
        let sequence = gdi_page_sequence(
            pages.len(),
            settings.copies,
            settings.collate,
            driver_copies,
            driver_collate,
        );
        let mut fonts = replay::FontRegistry::new();
        let mut result = Ok(());
        let mut done = 0usize;
        let total = sequence.len();
        for page_index in sequence {
            if job.is_cancelled() {
                result = Err("cancelled".into());
                break;
            }
            let page = &pages[page_index];
            // Each page's own px size sets the world transform; geometry
            // differences between pages are normal (paper changes).
            let page_transform = coords::page_transform(
                dpi_x as f32,
                dpi_y as f32,
                offset_x,
                offset_y,
            );
            if StartPage(hdc) <= 0 {
                result = Err("StartPage failed".into());
                break;
            }
            replay::replay_page(
                hdc,
                &page.scene.0,
                page_transform,
                &mut warnings,
                &mut fonts,
            );
            if EndPage(hdc) <= 0 {
                result = Err("EndPage failed".into());
                break;
            }
            done += 1;
            job.update(None, Some(done), None);
            let _ = total;
        }
        fonts.release_all();
        if result.is_err() {
            AbortDoc(hdc);
        } else {
            EndDoc(hdc);
        }
        gdi::DeleteDC(hdc);
        // Surface job warnings (dedup) through the shared state.
        if !warnings.is_empty() {
            let msg = warnings.join("; ");
            job.update(None, None, Some(format!("gdi degraded: {msg}")));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Orientation;

    #[test]
    fn driver_handles_everything_single_replay() {
        assert_eq!(gdi_page_sequence(3, 5, true, true, true), vec![0, 1, 2]);
        assert_eq!(gdi_page_sequence(3, 1, false, true, false), vec![0, 1, 2]);
    }

    #[test]
    fn collate_emulation_replays_copy_major() {
        assert_eq!(
            gdi_page_sequence(3, 2, true, false, false),
            vec![0, 1, 2, 0, 1, 2]
        );
    }

    #[test]
    fn non_collate_emulation_replays_page_major() {
        assert_eq!(
            gdi_page_sequence(3, 2, false, false, false),
            vec![0, 0, 1, 1, 2, 2]
        );
    }

    #[test]
    fn driver_copies_without_collate_support_page_major() {
        // Driver takes copies but can't collate while we ask page-major —
        // exactness requires emulation.
        assert_eq!(
            gdi_page_sequence(2, 2, false, true, false),
            vec![0, 0, 1, 1]
        );
    }

    #[test]
    fn single_copy_never_multiplies() {
        assert_eq!(gdi_page_sequence(4, 1, false, false, false), vec![0, 1, 2, 3]);
    }

    #[test]
    fn orientation_helper_passes_through() {
        // Sanity for the settings the pipeline consumes.
        let s = PrintSettings {
            orientation: Orientation::Landscape,
            ..PrintSettings::default()
        };
        assert_eq!(s.effective_orientation(false), Orientation::Landscape);
    }
}
