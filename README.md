# office-rs/print

Print spooling for office-rs editors ([rword](https://github.com/office-rs/rword),
rofd, and any `imaging` Scene-IR host): a UI-agnostic settings/dialog state
machine plus platform backends — **Windows GDI vector replay** of Scene IR
and **Linux CUPS** PDF spooling.

## Layout

- `office-rs-print/` — the crate.
  - **Type layer** (compiles on wasm32): `PrintSettings`, page-spec parsing
    and collate planning, the dialog state machine (`DialogState`,
    `PrintServices`), job handles, and the serde payload (`PrintPayload`
    with a font-deduplicated Scene wire format).
  - **Backend layer** (native only): `list_printers` /
    `printer_capabilities` / `submit`, the GDI sink (Windows) and the CUPS
    sink (Linux).
- `examples/` — a non-rword example host (reuse acceptance).

## Status

Bootstrap (print design plan 2/3). API surface will stabilize as rword and
rofd wire it up.

## License

Apache-2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE).
