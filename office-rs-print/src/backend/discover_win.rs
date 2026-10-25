//! Printer discovery (Windows, design §6.1): `EnumPrintersW` level 2 over
//! local + connected printers, default detection via `GetDefaultPrinterW`,
//! capability probing via `DeviceCapabilitiesW`.

use super::{PrinterCaps, PrinterInfo};
use crate::settings::Paper;
use windows_sys::Win32::Graphics::Printing::{
    EnumPrintersW, GetDefaultPrinterW, PRINTER_ENUM_CONNECTIONS, PRINTER_ENUM_LOCAL,
    PRINTER_INFO_2W,
};
use windows_sys::Win32::Storage::Xps::{
    DeviceCapabilitiesW, DC_COLORDEVICE, DC_COPIES, DC_DUPLEX, DC_PAPERS,
};

pub(crate) fn list_printers_impl() -> Vec<PrinterInfo> {
    unsafe {
        let default = default_printer_name();
        let flags: u32 = PRINTER_ENUM_LOCAL | PRINTER_ENUM_CONNECTIONS;
        let mut needed: u32 = 0;
        let mut returned: u32 = 0;
        // Size probe (returns 0 + GetLastError == ERROR_INSUFFICIENT_BUFFER).
        EnumPrintersW(
            flags,
            std::ptr::null(),
            2,
            std::ptr::null_mut(),
            0,
            &mut needed,
            &mut returned,
        );
        if needed == 0 {
            return Vec::new();
        }
        let mut buffer = vec![0u8; needed as usize];
        let ok = EnumPrintersW(
            flags,
            std::ptr::null(),
            2,
            buffer.as_mut_ptr(),
            needed,
            &mut needed,
            &mut returned,
        );
        if ok == 0 {
            return Vec::new();
        }
        let infos = std::slice::from_raw_parts(
            buffer.as_ptr() as *const PRINTER_INFO_2W,
            returned as usize,
        );
        infos
            .iter()
            .map(|info| {
                let name = pwstr_to_string(info.pPrinterName);
                let port = if info.pPortName.is_null() {
                    None
                } else {
                    Some(pwstr_to_string(info.pPortName))
                };
                let is_default = default.as_deref() == Some(name.as_str());
                PrinterInfo {
                    name,
                    is_default,
                    port,
                }
            })
            .collect()
    }
}

fn default_printer_name() -> Option<String> {
    unsafe {
        let mut len: u32 = 0;
        GetDefaultPrinterW(std::ptr::null_mut(), &mut len);
        if len == 0 {
            return None;
        }
        let mut buffer = vec![0u16; len as usize];
        if GetDefaultPrinterW(buffer.as_mut_ptr(), &mut len) != 0 {
            Some(String::from_utf16_lossy(&buffer[..len as usize]))
        } else {
            None
        }
    }
}

fn pwstr_to_string(ptr: *const u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe {
        let mut len = 0usize;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
    }
}

pub(crate) fn printer_caps_impl(name: Option<&str>) -> Option<PrinterCaps> {
    let device: Vec<u16> = match name {
        Some(n) => n.encode_utf16().chain(std::iter::once(0)).collect(),
        None => default_printer_name()?.encode_utf16().chain(std::iter::once(0)).collect(),
    };
    Some(caps_from_dc(
        &device,
        &dc_words(&device, DC_PAPERS),
        dc_words(&device, DC_DUPLEX).first().copied().unwrap_or(0),
        dc_words(&device, DC_COLORDEVICE).first().copied().unwrap_or(0),
        dc_words(&device, DC_COPIES)
            .first()
            .copied()
            .unwrap_or(1)
            .max(1) as u32,
    ))
}

fn dc_words(device: &[u16], capability: u16) -> Vec<u16> {
    unsafe {
        let needed = DeviceCapabilitiesW(
            device.as_ptr(),
            std::ptr::null(),
            capability,
            std::ptr::null_mut(),
            std::ptr::null(),
        );
        if needed <= 0 {
            return Vec::new();
        }
        let mut out = vec![0u16; needed as usize];
        let written = DeviceCapabilitiesW(
            device.as_ptr(),
            std::ptr::null(),
            capability,
            out.as_mut_ptr(),
            std::ptr::null(),
        );
        out.truncate(written.max(0) as usize);
        out
    }
}

/// Pure mapping from `DeviceCapabilitiesW` results (kept separate for
/// tests): DMPAPER ids filter through the v1 paper set; a DC_PAPERS count
/// of zero means the query failed and the panel falls back to defaults.
pub(crate) fn caps_from_dc(
    _device: &[u16],
    papers_raw: &[u16],
    duplex_supported: u16,
    color_supported: u16,
    copies_max: u32,
) -> PrinterCaps {
    PrinterCaps {
        duplex: duplex_supported == 1,
        color: color_supported == 1,
        papers: papers_raw
            .iter()
            .filter_map(|raw| Paper::from_dmpaper(*raw))
            .collect(),
        copies_max,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_from_dc_maps_and_filters() {
        let papers = [1u16, 9, 11, 7 /* envelope — filtered */];
        let caps = caps_from_dc(&[], &papers, 1, 1, 99);
        assert!(caps.duplex);
        assert!(caps.color);
        assert_eq!(
            caps.papers,
            vec![Paper::Letter, Paper::A4, Paper::A5]
        );
        assert_eq!(caps.copies_max, 99);
    }

    #[test]
    fn caps_from_dc_unsupported_flags() {
        let caps = caps_from_dc(&[], &[], 0, 0, 1);
        assert!(!caps.duplex);
        assert!(!caps.color);
        assert!(caps.papers.is_empty());
    }

    /// Live smoke on machines with any installed printer (skipped
    /// otherwise) — the reuse-acceptance "拉起发现列表".
    #[test]
    fn discovery_lists_printers_when_present() {
        let printers = list_printers_impl();
        if printers.is_empty() {
            eprintln!("skipping: no printers on this machine");
            return;
        }
        assert!(printers.iter().all(|p| !p.name.is_empty()));
        assert!(
            printers.iter().filter(|p| p.is_default).count() <= 1,
            "at most one default printer"
        );
    }
}
