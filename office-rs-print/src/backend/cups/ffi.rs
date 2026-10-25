//! Hand-written libcups FFI (design §13: no maintained crate; link-time
//! declaration keeps the dependency surface empty). Signatures follow
//! libcups 2.x stable API.

#![allow(non_snake_case)]

use std::ffi::c_char;

/// Opaque printer destination record (`cups_dest_t`).
#[repr(C)]
pub(crate) struct CupsDest {
    pub name: *mut c_char,
    pub is_default: i32,
    pub num_options: i32,
    pub options: *mut CupsOption,
}

/// One key/value option (`cups_option_t`).
#[repr(C)]
pub(crate) struct CupsOption {
    pub name: *mut c_char,
    pub value: *mut c_char,
}

extern "C" {
    pub(crate) fn cupsGetDests(num_dests: *mut i32) -> *mut CupsDest;
    pub(crate) fn cupsFreeDests(num_dests: i32, dests: *mut CupsDest);
    pub(crate) fn cupsAddOption(
        name: *const c_char,
        value: *const c_char,
        num_options: i32,
        options: *mut *mut CupsOption,
    ) -> i32;
    pub(crate) fn cupsFreeOptions(num_options: i32, options: *mut CupsOption);
    pub(crate) fn cupsPrintFile(
        name: *const c_char,
        filename: *const c_char,
        job_name: *const c_char,
        num_options: i32,
        options: *mut CupsOption,
    ) -> i32;
    pub(crate) fn cupsLastErrorString() -> *const c_char;
}
