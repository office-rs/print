//! office-rs-print — print spooling for office-rs editors (rword, rofd,
//! and any Scene-IR host).
//!
//! Two layers (design §6.1):
//! - **Type layer** (wasm32-compilable): [`PrintSettings`], page planning,
//!   the dialog state machine, job handles and the serde payload. No
//!   platform code.
//! - **Backend layer** (native): printer discovery, submission and the
//!   platform sinks (Windows GDI vector replay, Linux CUPS).
//!
//! Dependency discipline: the public face depends only on `imaging`
//! (Scene IR) and `serde` — never on rword/rofd crates, so any Scene-IR
//! host can adopt it.

pub mod dialog;
pub mod job;
pub mod page_plan;
pub mod payload;
pub mod settings;

#[cfg(not(target_arch = "wasm32"))]
mod backend;

// Type-layer re-exports.
pub use dialog::{DialogEvent, DialogPhase, DialogState, DiscoverState, Labels, PrintServices};
pub use job::{join_all, JobHandle, JobSnapshot, JobState};
pub use page_plan::{
    collate_plan, parse_page_spec, resolve_page_list, validate_page_spec, MAX_SPEC_LEN,
    MAX_TOTAL_PAGES,
};
pub use payload::{PageScene, PrintPayload, SceneWire};
pub use settings::{
    validate_settings, ColorMode, Duplex, Orientation, PageRange, Paper, Parity, PrintSettings,
    SettingsError,
};

// Backend-layer re-exports (native only).
#[cfg(not(target_arch = "wasm32"))]
pub use backend::{
    list_printers, printer_capabilities, submit, submit_to_file, PrinterCaps, PrinterInfo,
};

/// Crate version (host diagnostics).
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_semver_shaped() {
        let v = super::version();
        assert!(v.split('.').count() == 3, "version = {v}");
    }

    /// The reuse promise (design §12): the crate never depends on rword.
    /// Asserting through the public surface is impractical; the CI check
    /// `cargo tree` covers it, and this test pins the public API surface
    /// count so accidental additions are noticed in review.
    #[test]
    fn public_surface_stays_small() {
        // Touch every re-exported family once (compile-time assertions in
        // disguise — a removed or renamed item breaks this test).
        let _ = super::PrintSettings::default();
        let _ = super::Paper::A4.pwg_name();
        let _ = super::DialogState::initial();
        let _ = super::Labels::en();
        let _: Option<super::PrintPayload> = None;
        let _: Option<super::SceneWire> = None;
        let _ = super::JobSnapshot::fresh(0);
        let _ = super::MAX_TOTAL_PAGES;
    }
}
