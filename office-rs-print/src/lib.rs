//! office-rs-print — print spooling for office-rs editors (rword, rofd,
//! and any Scene-IR host).
//!
//! Two layers (design §6.1):
//! - **Type layer** (wasm32-compilable): [`PrintSettings`], page planning,
//!   the dialog state machine, [`crate::job`] handles and the serde
//!   payload. No platform code.
//! - **Backend layer** (`#[cfg(not(target_arch = "wasm32"))]`): printer
//!   discovery, submission and the platform sinks (Windows GDI vector
//!   replay, Linux CUPS).
//!
//! Dependency discipline: the public face depends only on `imaging`
//! (Scene IR) and `serde` — never on rword/rofd crates, so any Scene-IR
//! host can adopt it.

/// Crate version (host diagnostics).
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub mod page_plan;
pub mod settings;

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_semver_shaped() {
        let v = super::version();
        assert!(v.split('.').count() == 3, "version = {v}");
    }
}
