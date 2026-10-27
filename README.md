# office-rs/print

Print spooling for office-rs editors ([rword](https://github.com/office-rs/rword),
rofd, and any `imaging` Scene-IR host): a UI-agnostic settings/dialog state
machine plus platform backends — **Windows GDI vector replay** of Scene IR
and **Linux CUPS** PDF spooling.

## Layout

Single crate at the repo root (flat layout; a `crates/` restructure is
deferred until a second crate actually joins).

- **Type layer** (compiles on `wasm32-unknown-unknown`): `PrintSettings`
  with clamping and PWG/DMPAPER paper mappings, page-spec parsing
  (`"1-3,5"`) and collate planning, the dialog state machine
  (`DialogState` / `PrintServices` / `Labels`), job handles, and the
  serde payload (`PrintPayload` + `SceneWire`, a font-deduplicated JSON
  wire format over `imaging::record::Scene`).
- **Backend layer** (native): `list_printers` / `printer_capabilities` /
  `submit` (+ `submit_to_file` print-to-file channel), the GDI sink
  (Windows: DEVMODE override, private fonts via `AddFontMemResourceEx`,
  glyph-index `ExtTextOutW`, 64×-subpixel world transform, per-page
  cancel + `AbortProc`, driver-unsupported copies/collate emulated by
  replay order) and the CUPS sink (Linux: `cupsPrintFile` with
  media/sides/ColorModel/copies/Collate job options).
- `examples/discover.rs` — a NON-rword example host (reuse acceptance:
  `cargo tree -p office-rs-print` shows zero rword dependencies).

## Job-parameter discipline

Copies, collate, duplex and color are JOB parameters (driver/DEVMODE/CUPS
options, or submit-side replay emulation) — the payload itself is never
multiplied. Page-range/parity planning is host-side
(`page_plan::resolve_page_list`); the payload carries exactly the planned
pages.

## Example

```
cargo run --example discover                  # list printers + capabilities
cargo run --example discover -- print "Microsoft Print to PDF" --out out.pdf
cargo run --example discover -- cancel        # submit + immediate cancel probe
```

The `print` verb drives the full Windows pipeline with two synthetic
pages (fills, stroke, clip); with `--out` the "Microsoft Print to PDF"
driver materializes a PDF for inspection.

## Status

Bootstrap complete (print design plan 2/3). Manual test checklist:
- [x] Windows discovery lists printers with capabilities
- [x] Windows Print-to-PDF: 2/2 pages, fills/strokes/clips at correct
      96-dpi→pt coordinates (content-stream verified)
- [ ] Linux CUPS against `cups-pdf` (CI compiles; runtime manual)
- [ ] rword integration (plan 3/3: wasm bridge, SDK panel, xilem dialog,
      tauri IPC, `PrintSession::plan`)

## License

Apache-2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE).
