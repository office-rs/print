//! PDF-payload GDI sink (Windows): the system PDF renderer (WinRT
//! `Windows.Data.Pdf`, Win10+ — the Edge PDF engine, no third-party
//! shipping) rasterizes each page at DEVICE DPI, and the existing GDI
//! spool pipeline (DEVMODE override, copies/collate handling, per-page
//! cancel + AbortProc, progress) carries it to the printer. This is the
//! cross-process host route (tauri): a PDF is a few MB where a serialized
//! Scene payload embeds tens of MB of font bytes.

use super::coords;
use super::{abort_proc, gdi_page_sequence, prepare_printer_dc};
use crate::job::{JobState, SharedJob};
use crate::settings::PrintSettings;
use std::path::Path;
use windows::Data::Pdf::{PdfDocument, PdfPageRenderOptions};
use windows::Storage::Streams::{DataReader, InMemoryRandomAccessStream};
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED,
};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Graphics::Gdi as gdi;
use windows_sys::Win32::Graphics::Gdi::HDC;
use windows_sys::Win32::Storage::Xps::{EndDoc, EndPage, SetAbortProc, StartDocW, StartPage, DOCINFOW};

pub(crate) fn run_gdi_pdf_job(
    pdf: &[u8],
    settings: &PrintSettings,
    title: &str,
    output_file: Option<&Path>,
    job: &SharedJob,
) -> Result<(), String> {
    // WinRT needs an apartment on this worker thread; MTA lets the async
    // operations' blocking `.get()` settle.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .map_err(|e| format!("CoInitializeEx: {e}"))?;
        let result = run_inner(pdf, settings, title, output_file, job);
        CoUninitialize();
        result
    }
}

fn run_inner(
    pdf: &[u8],
    settings: &PrintSettings,
    title: &str,
    output_file: Option<&Path>,
    job: &SharedJob,
) -> Result<(), String> {
    let prepared = prepare_printer_dc(settings)?;
    unsafe {
        let hdc: HDC = prepared.hdc;
        // Load the PDF via a temp file: the DataWriter/InMemoryRandomAccess
        // Stream route left LoadFromStreamAsync seeing a zero-length stream
        // (writer stream-section semantics), and a temp file is what the
        // CUPS path already does anyway.
        let doc = {
            let path = std::env::temp_dir().join(format!(
                "office-rs-print-{}.pdf",
                std::process::id()
            ));
            std::fs::write(&path, pdf).map_err(|e| format!("write temp pdf: {e}"))?;
            let loaded = load_pdf_from_path(&path);
            let _ = std::fs::remove_file(&path);
            loaded?
        };
        let page_count = doc
            .PageCount()
            .map_err(|e| format!("page count: {e}"))? as usize;
        job.set_pages_total(page_count);

        let doc_name: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        let output_wide: Option<Vec<u16>> = output_file.map(|p| {
            p.to_string_lossy()
                .into_owned()
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect()
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
            page_count,
            settings.copies,
            settings.collate,
            prepared.driver_copies,
            prepared.driver_collate,
        );
        let mut done = 0usize;
        let mut result = Ok(());
        for page_index in sequence {
            if job.is_cancelled() {
                result = Err("cancelled".into());
                break;
            }
            if StartPage(hdc) <= 0 {
                result = Err("StartPage failed".into());
                break;
            }
            match render_page_to_dc(&doc, page_index as u32, hdc, &prepared) {
                Ok(()) => {}
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
            if EndPage(hdc) <= 0 {
                result = Err("EndPage failed".into());
                break;
            }
            done += 1;
            job.update(None, Some(done), None);
        }
        if result.is_err() {
            windows_sys::Win32::Storage::Xps::AbortDoc(hdc);
        } else {
            EndDoc(hdc);
        }
        gdi::DeleteDC(hdc);
        let _ = HANDLE::default();
        result
    }
}

/// Render PDF page `index` and blit it onto the page DC at device
/// resolution. Destination rect is the full physical page (the raster
/// carries the page box; margins are part of the PDF content).
unsafe fn render_page_to_dc(
    doc: &PdfDocument,
    index: u32,
    hdc: HDC,
    prepared: &super::PreparedPrinter,
) -> Result<(), String> {
    let page = doc
        .GetPage(index)
        .map_err(|e| format!("GetPage({index}): {e}"))?;
    // Preserve the PDF page's aspect within the physical sheet: render at
    // device size scaled by the min axis ratio, centered like the vector
    // path's paper-on-desk mapping.
    let dims = page
        .Dimensions()
        .map_err(|e| format!("dims: {e}"))?;
    // The page box in PDF points (MediaBox; fallback CropBox).
    let media = dims
        .MediaBox()
        .or_else(|_| dims.CropBox())
        .map_err(|e| format!("mediabox: {e}"))?;
    let (pdf_w, pdf_h) = (f64::from(media.Width), f64::from(media.Height));
    if pdf_w <= 0.0 || pdf_h <= 0.0 {
        return Err("pdf page has no extent".into());
    }
    let phys_w = prepared.phys_w as f64;
    let phys_h = prepared.phys_h as f64;
    let scale = (phys_w / pdf_w).min(phys_h / pdf_h);
    let dest_w = (pdf_w * scale).round().max(1.0) as i32;
    let dest_h = (pdf_h * scale).round().max(1.0) as i32;
    let dest_x = ((phys_w - dest_w as f64) / 2.0).round() as i32;
    let dest_y = ((phys_h - dest_h as f64) / 2.0).round() as i32;

    let options = PdfPageRenderOptions::new()
        .map_err(|e| format!("render options: {e}"))?;
    options
        .SetDestinationWidth(dest_w as u32)
        .map_err(|e| format!("dest width: {e}"))?;
    options
        .SetDestinationHeight(dest_h as u32)
        .map_err(|e| format!("dest height: {e}"))?;

    let out = InMemoryRandomAccessStream::new()
        .map_err(|e| format!("out stream: {e}"))?;
    page.RenderWithOptionsToStreamAsync(&out, &options)
        .map_err(|e| format!("render: {e}"))?
        .join()
        .map_err(|e| format!("render wait: {e}"))?;

    let size = out.Size().map_err(|e| format!("size: {e}"))? as u32;
    if size == 0 {
        return Err("render produced 0 bytes".into());
    }
    let input = out
        .GetInputStreamAt(0)
        .map_err(|e| format!("input stream: {e}"))?;
    let reader = DataReader::CreateDataReader(&input)
        .map_err(|e| format!("reader: {e}"))?;
    reader
        .LoadAsync(size)
        .map_err(|e| format!("load: {e}"))?
        .join()
        .map_err(|e| format!("load wait: {e}"))?;
    let mut bmp = vec![0u8; size as usize];
    reader
        .ReadBytes(&mut bmp)
        .map_err(|e| format!("read: {e}"))?;
    // The stream is a PNG image (Windows.Data.Pdf renders pages to an
    // encoded image stream; PNG on current Windows).
    // image has no BGRA conversion — decode to RGBA and swizzle per pixel.
    let decoded = image::load_from_memory(&bmp)
        .map_err(|e| format!("decode rendered page: {e}"))?
        .to_rgba8();
    let (decoded_w, decoded_h) = decoded.dimensions();
    let mut bgra = decoded.into_raw();
    for px in bgra.as_chunks_mut::<4>().0 {
        px.swap(0, 2); // RGBA -> BGRA for StretchDIBits
    }

    gdi::SetStretchBltMode(hdc, gdi::HALFTONE);
    let info = gdi::BITMAPINFO {
        bmiHeader: gdi::BITMAPINFOHEADER {
            biSize: std::mem::size_of::<gdi::BITMAPINFOHEADER>() as u32,
            biWidth: decoded_w as i32,
            biHeight: -(decoded_h as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: gdi::BI_RGB,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        bmiColors: [gdi::RGBQUAD {
            rgbBlue: 0,
            rgbGreen: 0,
            rgbRed: 0,
            rgbReserved: 0,
        }],
    };
    gdi::StretchDIBits(
        hdc,
        dest_x,
        dest_y,
        decoded_w as i32,
        decoded_h as i32,
        0,
        0,
        decoded_w as i32,
        decoded_h as i32,
        bgra.as_ptr() as *const core::ffi::c_void,
        &info,
        gdi::DIB_RGB_COLORS,
        gdi::SRCCOPY,
    );
    let _ = coords::page_transform; // shared module kept for the vector sink
    Ok(())
}

/// Load a PdfDocument from a file path (StorageFile route; runs on this
/// MTA worker thread so the async `.join()` blocks fine).
fn load_pdf_from_path(path: &std::path::Path) -> Result<PdfDocument, String> {
    use windows::Storage::StorageFile;
    let wide = windows::core::HSTRING::from(path.as_os_str());
    let file = StorageFile::GetFileFromPathAsync(&wide)
        .map_err(|e| format!("storage file: {e}"))?
        .join()
        .map_err(|e| format!("storage file wait: {e}"))?;
    PdfDocument::LoadFromFileAsync(&file)
        .map_err(|e| format!("load: {e}"))?
        .join()
        .map_err(|e| format!("load wait: {e}"))
}
