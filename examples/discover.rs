//! Example host (design §12 reuse acceptance): a NON-rword binary that
//! lists printers and prints a synthetic two-page Scene-IR payload. It
//! proves the crate's public face carries zero rword leakage and doubles
//! as the GDI/CUPS manual-test vehicle.
//!
//! Usage:
//!   discover                      — list printers (+ capabilities)
//!   print [name]                  — print 2 synthetic pages to `name`
//!   printpdf <file.pdf> [printer] — print an existing PDF via print_pdf
//!                                    (or the default printer)
//!   print [name] --out <file>     — print-to-file channel (e.g. with
//!                                    "Microsoft Print to PDF" this
//!                                    materializes a PDF at <file>)
//!   cancel                        — submit, then cancel immediately

use imaging::kurbo::{Affine, Rect, Stroke, Vec2};
use imaging::peniko;
use imaging::record::{Clip, Draw, Geometry, Glyph, GlyphRun, Scene};
use imaging::Composite;
use peniko::{FontData, Style};
use office_rs_print::{PageScene, PrintPayload, PrintSettings};
use std::sync::Arc;

fn white_fill(w: f64, h: f64) -> Draw {
    Draw::Fill {
        transform: Affine::IDENTITY,
        fill_rule: peniko::Fill::NonZero,
        brush: peniko::Brush::Solid(peniko::Color::from_rgba8(255, 255, 255, 255)),
        brush_transform: None,
        shape: Geometry::Rect(Rect::new(0.0, 0.0, w, h)),
        composite: Composite::default(),
    }
}

fn color_fill(rgba: [u8; 4], x0: f64, y0: f64, x1: f64, y1: f64) -> Draw {
    Draw::Fill {
        transform: Affine::IDENTITY,
        fill_rule: peniko::Fill::NonZero,
        brush: peniko::Brush::Solid(peniko::Color::from_rgba8(
            rgba[0], rgba[1], rgba[2], rgba[3],
        )),
        brush_transform: None,
        shape: Geometry::Rect(Rect::new(x0, y0, x1, y1)),
        composite: Composite::default(),
    }
}

/// One synthetic A4 page: white ground, blue header band, red/green
/// content blocks, a black rule; page two adds a clip-scoped overlay.
fn synthetic_page(index: usize) -> PageScene {
    // A4 portrait at 96 dpi.
    let (w, h) = (794.0, 1123.0);
    let mut scene = Scene::new();
    scene.draw(white_fill(w, h));
    scene.draw(color_fill([0, 120, 215, 255], 0.0, 0.0, w, 120.0));
    scene.draw(color_fill([200, 30, 30, 255], 60.0, 200.0, 320.0, 460.0));
    scene.draw(color_fill([30, 160, 60, 255], 60.0, 500.0, 320.0, 760.0));
    scene.draw(Draw::Stroke {
        transform: Affine::IDENTITY,
        stroke: Stroke::new(3.0),
        brush: peniko::Brush::Solid(peniko::Color::from_rgba8(0, 0, 0, 255)),
        brush_transform: None,
        shape: Geometry::Rect(Rect::new(60.0, 820.0, w - 60.0, 1020.0)),
        composite: Composite::default(),
    });
    // Glyph-size verification line (manual test): real CJK font bytes,
    // sequential glyph ids — visible boxes/hanzi prove the GDI font
    // height scaling matches the 64x subpixel coordinate convention.
    if let Ok(font_bytes) = std::fs::read("C:/Windows/Fonts/simsun.ttc") {
        let glyphs: Vec<_> = (100u32..112)
            .enumerate()
            .map(|(i, id)| Glyph { id, x: (i as f32) * 48.0, y: 0.0 })
            .collect();
        scene.draw(Draw::GlyphRun(GlyphRun {
            font: FontData {
                data: peniko::Blob::new(Arc::new(font_bytes)),
                index: 0,
            },
            transform: Affine::translate((60.0, 130.0)),
            glyph_transform: None,
            brush_transform: None,
            font_size: 48.0,
            font_embolden: Vec2::ZERO,
            hint: false,
            normalized_coords: vec![],
            style: Style::Fill(peniko::Fill::NonZero),
            glyphs,
            brush: peniko::Brush::Solid(peniko::Color::from_rgba8(0, 0, 0, 255)),
            composite: Composite::default(),
        }));
    }
    if index == 1 {
        scene.push_clip(Clip::Fill {
            transform: Affine::IDENTITY,
            shape: Geometry::Rect(Rect::new(400.0, 200.0, 700.0, 500.0)),
            fill_rule: peniko::Fill::NonZero,
        });
        scene.draw(color_fill([255, 200, 0, 255], 350.0, 150.0, 750.0, 550.0));
        scene.pop_clip();
        scene.draw(color_fill([90, 90, 200, 90], 400.0, 600.0, 700.0, 900.0));
    }
    PageScene {
        paper: office_rs_print::Paper::A4,
        orientation: office_rs_print::Orientation::Portrait,
        width_px: w as f32,
        height_px: h as f32,
        scene: office_rs_print::SceneWire(scene),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("discover") => {
            println!("office-rs-print {} — printers:", office_rs_print::version());
            let printers = office_rs_print::list_printers();
            if printers.is_empty() {
                println!("  (none found)");
            }
            for p in &printers {
                let marker = if p.is_default { " [default]" } else { "" };
                println!("  {}{}", p.name, marker);
                if let Some(caps) = office_rs_print::printer_capabilities(Some(&p.name)) {
                    println!(
                        "    duplex={} color={} copies<={} papers={:?}",
                        caps.duplex,
                        caps.color,
                        caps.copies_max,
                        caps.papers
                    );
                }
            }
        }
        Some("print") => {
            let printer = args.get(1).filter(|a| !a.starts_with("--")).cloned();
            let out = args
                .iter()
                .position(|a| a == "--out")
                .and_then(|i| args.get(i + 1))
                .cloned();
            let settings = PrintSettings {
                printer,
                copies: 1,
                ..PrintSettings::default()
            };
            let payload = PrintPayload::Pages {
                pages: vec![synthetic_page(0), synthetic_page(1)],
            };
            let handle = office_rs_print::print_pages_to_file(
                payload,
                &settings,
                "office-rs-print example",
                out.map(std::path::PathBuf::from),
            );
            let snap = handle.wait();
            println!(
                "job finished: {:?} pages={}/{} error={:?}",
                snap.state, snap.pages_done, snap.pages_total, snap.error
            );
            office_rs_print::join_all();
            if snap.state != office_rs_print::JobState::Done {
                std::process::exit(2);
            }
        }
        Some("printpdf") => {
            // Manual-test vehicle for print_pdf: any PDF file straight to a
            // printer (default when omitted). `--out` = print-to-file.
            let file = args.get(1).cloned();
            let printer = args.get(2).filter(|a| !a.starts_with("--")).cloned();
            let out = args
                .iter()
                .position(|a| a == "--out")
                .and_then(|i| args.get(i + 1))
                .cloned();
            let Some(file) = file else {
                eprintln!("usage: printpdf <file.pdf> [printer] [--out <file>]");
                std::process::exit(2);
            };
            let bytes = std::fs::read(&file).unwrap_or_else(|e| {
                eprintln!("read {file}: {e}");
                std::process::exit(2);
            });
            let settings = PrintSettings {
                printer,
                copies: 1,
                ..PrintSettings::default()
            };
            let handle = office_rs_print::print_pdf_to_file(
                &bytes,
                &settings,
                "office-rs-print pdf",
                out.map(std::path::PathBuf::from),
            );
            let snap = handle.wait();
            println!(
                "pdf job: {:?} pages={}/{} error={:?}",
                snap.state, snap.pages_done, snap.pages_total, snap.error
            );
            office_rs_print::join_all();
            if snap.state != office_rs_print::JobState::Done {
                std::process::exit(2);
            }
        }
        Some("cancel") => {
            let payload = PrintPayload::Pages {
                pages: vec![synthetic_page(0), synthetic_page(1)],
            };
            let handle = office_rs_print::print_pages(payload, &PrintSettings::default(), "cancel-probe");
            handle.cancel();
            let snap = handle.wait();
            println!("cancel probe: {:?}", snap.state);
            office_rs_print::join_all();
        }
        Some(other) => {
            eprintln!(
                "usage: discover | print [printer] [--out <file>] | cancel (got {other:?})"
            );
            std::process::exit(2);
        }
    }
    // The Arc import is exercised by payload blobs in real use; keep the
    // dependency shape explicit for the zero-rword proof.
    let _: Option<Arc<u8>> = None;
}
