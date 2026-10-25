//! CUPS sink (Linux, design §6.1/D3): PDF payload spooled via
//! `cupsPrintFile` with job options from [`options::job_options`]
//! (media/sides/ColorModel/copies/Collate — D10 job parameters; the PDF
//! itself is a single copy). Page planning and paper grouping happen
//! host-side; one submission is one job.

pub(crate) mod ffi;
pub(crate) mod options;

use super::{PrinterCaps, PrinterInfo};
use crate::job::{JobState, SharedJob};
use crate::settings::PrintSettings;
use std::ffi::{CStr, CString};
use std::path::Path;

pub(crate) fn list_printers_impl() -> Vec<PrinterInfo> {
    unsafe {
        let mut num: i32 = 0;
        let dests = ffi::cupsGetDests(&mut num);
        if dests.is_null() || num == 0 {
            if !dests.is_null() {
                ffi::cupsFreeDests(num, dests);
            }
            return Vec::new();
        }
        let slice = std::slice::from_raw_parts(dests, num as usize);
        let printers = slice
            .iter()
            .map(|dest| PrinterInfo {
                name: cstr_to_string(dest.name),
                is_default: dest.is_default != 0,
                port: None,
            })
            .collect();
        ffi::cupsFreeDests(num, dests);
        printers
    }
}

pub(crate) fn printer_caps_impl(name: Option<&str>) -> Option<PrinterCaps> {
    // v1: CUPS capability probing (IPP printer attributes) is not wired;
    // panels fall back to permissive defaults.
    let _ = name;
    None
}

fn cstr_to_string(ptr: *const std::ffi::c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() }
}

/// Grow a CUPS option array (`cupsAddOption` returns the new count; the
/// array pointer grows through `&mut`).
fn assemble_options(opts: &[options::JobOption]) -> (i32, *mut ffi::CupsOption) {
    let mut options: *mut ffi::CupsOption = std::ptr::null_mut();
    let mut num: i32 = 0;
    for opt in opts {
        if let (Ok(name), Ok(value)) = (
            CString::new(opt.name.as_bytes()),
            CString::new(opt.value.as_bytes()),
        ) {
            unsafe {
                num = ffi::cupsAddOption(name.as_ptr(), value.as_ptr(), num, &mut options);
            }
        }
    }
    (num, options)
}

pub(crate) fn run_cups_job(
    pdf: &[u8],
    settings: &PrintSettings,
    title: &str,
    output_file: Option<&Path>,
    job: &SharedJob,
) -> Result<(), String> {
    // Materialize the payload in a file (cupsPrintFile takes a path); the
    // output-file channel writes it and skips submission (test path).
    let path = output_file.map(|p| p.to_path_buf()).unwrap_or_else(|| {
        let mut p = std::env::temp_dir();
        p.push(format!("office-rs-print-{}.pdf", std::process::id()));
        p
    });
    std::fs::write(&path, pdf).map_err(|e| format!("write spool file: {e}"))?;
    if output_file.is_some() {
        job.update(Some(JobState::Done), None, None);
        return Ok(());
    }

    let printer = match &settings.printer {
        Some(name) => name.clone(),
        None => list_printers_impl()
            .into_iter()
            .find(|p| p.is_default)
            .map(|p| p.name)
            .ok_or_else(|| "no default printer".to_string())?,
    };

    let c_printer = CString::new(printer).map_err(|e| e.to_string())?;
    let c_file =
        CString::new(path.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
    let c_job = CString::new(title).map_err(|e| e.to_string())?;
    let (num_options, options) = assemble_options(&options::job_options(settings));

    job.update(Some(JobState::Rendering), None, None);
    if job.is_cancelled() {
        unsafe {
            ffi::cupsFreeOptions(num_options, options);
        }
        let _ = std::fs::remove_file(&path);
        return Err("cancelled".into());
    }
    let job_id = unsafe {
        let id = ffi::cupsPrintFile(
            c_printer.as_ptr(),
            c_file.as_ptr(),
            c_job.as_ptr(),
            num_options,
            options,
        );
        ffi::cupsFreeOptions(num_options, options);
        id
    };
    let _ = std::fs::remove_file(&path);
    if job_id <= 0 {
        let err = unsafe { cstr_to_string(ffi::cupsLastErrorString()) };
        return Err(format!("cupsPrintFile failed: {err}"));
    }
    job.update(Some(JobState::Done), None, None);
    Ok(())
}
