//! Print payload (design §6.1): [`PageScene`] (paper + orientation + a
//! Scene-IR recording) and [`PrintPayload`] crossing host boundaries
//! (tauri IPC, worker threads) as JSON through the [`SceneWire`] newtype —
//! an orphan-rule-compliant serde wrapper over `imaging::record::Scene`.
//!
//! Wire format v1 (JSON):
//! `{ fonts, images, labels, files, contexts, clips, masks, groups, draws,
//! commands }` — opaque scene ids become POSITIONAL table indices. Font
//! and image byte blobs are deduplicated by buffer identity, so a 20 MB
//! CJK font referenced by a thousand glyph runs is stored exactly once.
//! Masks nest their whole mask scene recursively.
//!
//! v1 coverage matches what office-rs print scenes contain: solid and
//! image brushes, paths, glyph runs, clips, groups (incl. masks) and
//! blurred rects. Gradient brushes and non-default blend modes serialize
//! to an explicit error rather than losing data silently.

use crate::settings::{Orientation, Paper};
use imaging::kurbo::{Affine, Cap, Join, PathEl, Rect, RoundedRect, RoundedRectRadii, Stroke, Vec2};
use imaging::peniko;
use imaging::record::{
    self, Clip, Command, Context, ContextId, ContextKind, ContextValue, Draw, Geometry, Glyph,
    GlyphRun, Group, Mask, MaskId, Scene,
};
use imaging::{Composite, MaskMode};
use peniko::{FontData, Style};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// Serde wrapper over a recorded Scene (see the module docs for the wire
/// format). `Default` (empty scene) and `Clone` are cheap pass-throughs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SceneWire(pub Scene);

// ───────────────────────────── wire schema ─────────────────────────────

#[derive(Serialize, Deserialize, Default)]
struct SceneData {
    fonts: Vec<FontEntry>,
    images: Vec<ImageEntry>,
    labels: Vec<String>,
    files: Vec<String>,
    contexts: Vec<ContextEntry>,
    clips: Vec<ClipEntry>,
    masks: Vec<MaskEntry>,
    groups: Vec<GroupEntry>,
    draws: Vec<DrawEntry>,
    commands: Vec<CommandEntry>,
}

#[derive(Serialize, Deserialize)]
struct FontEntry {
    index: u32,
    bytes: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct ImageEntry {
    bytes: Vec<u8>,
    format: String,
    alpha_type: String,
    width: u32,
    height: u32,
}

#[derive(Serialize, Deserialize)]
struct ContextEntry {
    kind: ContextKindEntry,
    value: ContextValueEntry,
    source: Option<SourceEntry>,
}

#[derive(Serialize, Deserialize)]
enum ContextKindEntry {
    Label,
    Widget,
    ChildIndex,
    Slot,
    Named(usize),
}

#[derive(Serialize, Deserialize)]
enum ContextValueEntry {
    Str(usize),
    U64(u64),
    I64(i64),
    Usize(usize),
    Bool(bool),
}

#[derive(Serialize, Deserialize)]
struct SourceEntry {
    file: usize,
    line: u32,
    column: u32,
}

#[derive(Serialize, Deserialize)]
enum ClipEntry {
    Fill {
        transform: Coeffs,
        shape: GeometryEntry,
        fill_rule: String,
    },
    Stroke {
        transform: Coeffs,
        shape: GeometryEntry,
        stroke: StrokeEntry,
    },
}

#[derive(Serialize, Deserialize)]
struct MaskEntry {
    mode: String,
    scene: SceneData,
}

#[derive(Serialize, Deserialize)]
struct GroupEntry {
    clip: Option<ClipEntry>,
    mask: Option<AppliedMaskEntry>,
    filters: Vec<FilterEntry>,
    composite: CompositeEntry,
}

#[derive(Serialize, Deserialize)]
struct AppliedMaskEntry {
    mask: usize,
    transform: Coeffs,
}

#[derive(Serialize, Deserialize)]
enum FilterEntry {
    Flood { color: [u8; 4] },
    Blur { std_deviation_x: f32, std_deviation_y: f32 },
    DropShadow {
        dx: f32,
        dy: f32,
        std_deviation_x: f32,
        std_deviation_y: f32,
        color: [u8; 4],
    },
    Offset {
        dx: f32,
        dy: f32,
    },
}

#[derive(Serialize, Deserialize)]
struct CompositeEntry {
    alpha: f32,
}

#[derive(Serialize, Deserialize)]
enum DrawEntry {
    Fill {
        transform: Coeffs,
        fill_rule: String,
        brush: BrushEntry,
        brush_transform: Option<Coeffs>,
        shape: GeometryEntry,
        composite: CompositeEntry,
    },
    Stroke {
        transform: Coeffs,
        stroke: StrokeEntry,
        brush: BrushEntry,
        brush_transform: Option<Coeffs>,
        shape: GeometryEntry,
        composite: CompositeEntry,
    },
    GlyphRun(Box<GlyphRunEntry>),
    BlurredRoundedRect {
        transform: Coeffs,
        rect: [f64; 4],
        color: [u8; 4],
        radius: f64,
        std_dev: f64,
        composite: CompositeEntry,
    },
}

#[derive(Serialize, Deserialize)]
struct GlyphRunEntry {
    font: usize,
    font_index: u32,
    font_size: f32,
    transform: Coeffs,
    glyph_transform: Option<Coeffs>,
    brush_transform: Option<Coeffs>,
    embolden: [f64; 2],
    hint: bool,
    normalized_coords: Vec<i16>,
    style_fill: Option<String>,
    style_stroke: Option<StrokeEntry>,
    glyphs: Vec<[f32; 3]>,
    brush: BrushEntry,
    composite: CompositeEntry,
}

#[derive(Serialize, Deserialize)]
enum BrushEntry {
    Solid([u8; 4]),
    Image {
        image: usize,
        x_extend: String,
        y_extend: String,
        quality: String,
        alpha: f32,
    },
}

#[derive(Serialize, Deserialize)]
struct StrokeEntry {
    width: f64,
    miter_limit: f64,
    start_cap: String,
    end_cap: String,
    join: String,
    dash_pattern: Vec<f64>,
    dash_offset: f64,
}

#[derive(Serialize, Deserialize)]
enum GeometryEntry {
    Rect([f64; 4]),
    RoundedRect {
        rect: [f64; 4],
        radii: [f64; 4],
    },
    Path(Vec<PathElEntry>),
}

#[derive(Serialize, Deserialize)]
enum PathElEntry {
    M([f64; 2]),
    L([f64; 2]),
    Q([f64; 4]),
    C([f64; 6]),
    Z,
}

type Coeffs = [f64; 6];

#[derive(Serialize, Deserialize)]
enum CommandEntry {
    PushContext(usize),
    PopContext,
    PushClip(usize),
    PopClip,
    PushGroup(usize),
    PopGroup,
    Draw(usize),
}

// ───────────────────────────── serialize ─────────────────────────────

/// Accumulates shared tables (deduplicated by buffer identity) while the
/// command walk converts scene payloads into wire entries.
struct Serializer {
    data: SceneData,
    fonts: HashMap<(usize, u32), usize>,
    images: HashMap<usize, usize>,
    labels: HashMap<imaging::record::LabelId, usize>,
    files: HashMap<imaging::record::FileId, usize>,
    /// Wire-index reservations for masks (defined lazily through groups).
    mask_slots: HashMap<MaskId, usize>,
}

impl Serializer {
    fn new() -> Self {
        Self {
            data: SceneData::default(),
            fonts: HashMap::new(),
            images: HashMap::new(),
            labels: HashMap::new(),
            files: HashMap::new(),
            mask_slots: HashMap::new(),
        }
    }

    fn font(&mut self, font: &FontData) -> Result<usize, String> {
        let key = (font.data.as_ref().as_ptr() as usize, font.index);
        if let Some(idx) = self.fonts.get(&key) {
            return Ok(*idx);
        }
        let idx = self.data.fonts.len();
        self.data.fonts.push(FontEntry {
            index: font.index,
            bytes: font.data.as_ref().to_vec(),
        });
        self.fonts.insert(key, idx);
        Ok(idx)
    }

    fn image(&mut self, image: &peniko::ImageData) -> Result<usize, String> {
        let key = image.data.as_ref().as_ptr() as usize;
        if let Some(idx) = self.images.get(&key) {
            return Ok(*idx);
        }
        let idx = self.data.images.len();
        self.data.images.push(ImageEntry {
            bytes: image.data.as_ref().to_vec(),
            format: image_format_name(image.format)?,
            alpha_type: image_alpha_name(image.alpha_type)?,
            width: image.width,
            height: image.height,
        });
        self.images.insert(key, idx);
        Ok(idx)
    }

    fn label(&mut self, scene: &Scene, id: imaging::record::LabelId) -> Result<usize, String> {
        if let Some(idx) = self.labels.get(&id) {
            return Ok(*idx);
        }
        let idx = self.data.labels.len();
        self.data.labels.push(scene.label(id).to_string());
        self.labels.insert(id, idx);
        Ok(idx)
    }

    fn file(&mut self, scene: &Scene, id: imaging::record::FileId) -> Result<usize, String> {
        if let Some(idx) = self.files.get(&id) {
            return Ok(*idx);
        }
        let idx = self.data.files.len();
        self.data.files.push(scene.file(id).to_string());
        self.files.insert(id, idx);
        Ok(idx)
    }

    fn context(
        &mut self,
        scene: &Scene,
        id: ContextId,
    ) -> Result<usize, String> {
        let ctx: &Context = scene.context(id);
        let kind = match &ctx.kind {
            ContextKind::Label => ContextKindEntry::Label,
            ContextKind::Widget => ContextKindEntry::Widget,
            ContextKind::ChildIndex => ContextKindEntry::ChildIndex,
            ContextKind::Slot => ContextKindEntry::Slot,
            ContextKind::Named(label) => ContextKindEntry::Named(self.label(scene, *label)?),
        };
        let value = match &ctx.value {
            ContextValue::Str(label) => ContextValueEntry::Str(self.label(scene, *label)?),
            ContextValue::U64(v) => ContextValueEntry::U64(*v),
            ContextValue::I64(v) => ContextValueEntry::I64(*v),
            ContextValue::Usize(v) => ContextValueEntry::Usize(*v),
            ContextValue::Bool(v) => ContextValueEntry::Bool(*v),
        };
        let source = match &ctx.source {
            Some(src) => Some(SourceEntry {
                file: self.file(scene, src.file)?,
                line: src.line,
                column: src.column,
            }),
            None => None,
        };
        let idx = self.data.contexts.len();
        self.data.contexts.push(ContextEntry { kind, value, source });
        Ok(idx)
    }

    fn geometry(&mut self, geometry: &Geometry) -> GeometryEntry {
        match geometry {
            Geometry::Rect(r) => GeometryEntry::Rect([r.x0, r.y0, r.x1, r.y1]),
            Geometry::RoundedRect(r) => {
                let rect = r.rect();
                let radii = r.radii();
                GeometryEntry::RoundedRect {
                    rect: [rect.x0, rect.y0, rect.x1, rect.y1],
                    radii: [
                        radii.top_left,
                        radii.top_right,
                        radii.bottom_right,
                        radii.bottom_left,
                    ],
                }
            }
            Geometry::Path(p) => GeometryEntry::Path(
                p.elements()
                    .iter()
                    .map(|el| match *el {
                        PathEl::MoveTo(p) => PathElEntry::M([p.x, p.y]),
                        PathEl::LineTo(p) => PathElEntry::L([p.x, p.y]),
                        PathEl::QuadTo(c, p) => PathElEntry::Q([c.x, c.y, p.x, p.y]),
                        PathEl::CurveTo(c1, c2, p) => {
                            PathElEntry::C([c1.x, c1.y, c2.x, c2.y, p.x, p.y])
                        }
                        PathEl::ClosePath => PathElEntry::Z,
                    })
                    .collect(),
            ),
        }
    }

    fn clip(&mut self, clip: &Clip) -> Result<ClipEntry, String> {
        Ok(match clip {
            Clip::Fill {
                transform,
                shape,
                fill_rule,
            } => ClipEntry::Fill {
                transform: transform.as_coeffs(),
                shape: self.geometry(shape),
                fill_rule: fill_rule_name(*fill_rule).to_string(),
            },
            Clip::Stroke {
                transform,
                shape,
                stroke,
            } => ClipEntry::Stroke {
                transform: transform.as_coeffs(),
                shape: self.geometry(shape),
                stroke: stroke_entry(stroke)?,
            },
        })
    }

    fn mask(&mut self, mask: &Mask) -> Result<MaskEntry, String> {
        Ok(MaskEntry {
            mode: match mask.mode {
                MaskMode::Alpha => "alpha".to_string(),
                MaskMode::Luminance => "luminance".to_string(),
            },
            scene: scene_data(&mask.scene)?,
        })
    }

    fn group(&mut self, scene: &Scene, group: &Group) -> Result<GroupEntry, String> {
        let mask = match &group.mask {
            Some(applied) => {
                let mask_idx = self.mask_table_index(scene, applied.mask)?;
                Some(AppliedMaskEntry {
                    mask: mask_idx,
                    transform: applied.transform.as_coeffs(),
                })
            }
            None => None,
        };
        Ok(GroupEntry {
            clip: match &group.clip {
                Some(c) => Some(self.clip(c)?),
                None => None,
            },
            mask,
            filters: group
                .filters
                .iter()
                .map(filter_entry)
                .collect::<Result<Vec<_>, String>>()?,
            composite: composite_entry(&group.composite)?,
        })
    }

    /// Masks are defined lazily — the command walk only sees them through
    /// groups. Walk the masks this group references (recursively, since a
    /// mask scene can itself contain masked groups) and give each a table
    /// index.
    fn mask_table_index(&mut self, scene: &Scene, id: MaskId) -> Result<usize, String> {
        if let Some(slot) = self.mask_slots.get(&id) {
            return Ok(*slot);
        }
        let idx = self.data.masks.len();
        // Reserve the slot first: the nested walk may reach the same mask
        // again through a cycle-safe depth (scenes are trees, but the
        // reservation also guards self-reference within one entry).
        self.mask_slots.insert(id, idx);
        let entry = self.mask(scene.mask(id))?;
        self.data.masks.push(entry);
        Ok(idx)
    }

    fn brush(&mut self, brush: &peniko::Brush) -> Result<BrushEntry, String> {
        Ok(match brush {
            peniko::Brush::Solid(color) => BrushEntry::Solid(rgba8(color)),
            peniko::Brush::Image(image_brush) => BrushEntry::Image {
                image: self.image(&image_brush.image)?,
                x_extend: extend_name(image_brush.sampler.x_extend)?.to_string(),
                y_extend: extend_name(image_brush.sampler.y_extend)?.to_string(),
                quality: image_quality_name(image_brush.sampler.quality)?.to_string(),
                alpha: image_brush.sampler.alpha,
            },
            peniko::Brush::Gradient(_) => {
                return Err("gradient brushes are unsupported in the wire format v1".to_string())
            }
        })
    }

    fn draw(&mut self, draw: &Draw) -> Result<DrawEntry, String> {
        Ok(match draw {
            Draw::Fill {
                transform,
                fill_rule,
                brush,
                brush_transform,
                shape,
                composite,
            } => DrawEntry::Fill {
                transform: transform.as_coeffs(),
                fill_rule: fill_rule_name(*fill_rule).to_string(),
                brush: self.brush(brush)?,
                brush_transform: brush_transform.map(|t| t.as_coeffs()),
                shape: self.geometry(shape),
                composite: composite_entry(composite)?,
            },
            Draw::Stroke {
                transform,
                stroke,
                brush,
                brush_transform,
                shape,
                composite,
            } => DrawEntry::Stroke {
                transform: transform.as_coeffs(),
                stroke: stroke_entry(stroke)?,
                brush: self.brush(brush)?,
                brush_transform: brush_transform.map(|t| t.as_coeffs()),
                shape: self.geometry(shape),
                composite: composite_entry(composite)?,
            },
            Draw::GlyphRun(run) => DrawEntry::GlyphRun(Box::new(GlyphRunEntry {
                font: self.font(&run.font)?,
                font_index: run.font.index,
                font_size: run.font_size,
                transform: run.transform.as_coeffs(),
                glyph_transform: run.glyph_transform.map(|t| t.as_coeffs()),
                brush_transform: run.brush_transform.map(|t| t.as_coeffs()),
                embolden: [run.font_embolden.x, run.font_embolden.y],
                hint: run.hint,
                normalized_coords: run.normalized_coords.clone(),
                style_fill: match &run.style {
                    Style::Fill(rule) => Some(fill_rule_name(*rule).to_string()),
                    Style::Stroke(_) => None,
                },
                style_stroke: match &run.style {
                    Style::Fill(_) => None,
                    Style::Stroke(s) => Some(stroke_entry(s)?),
                },
                glyphs: run
                    .glyphs
                    .iter()
                    .map(|g| [g.id as f32, g.x, g.y])
                    .collect(),
                brush: self.brush(&run.brush)?,
                composite: composite_entry(&run.composite)?,
            })),
            Draw::BlurredRoundedRect(b) => DrawEntry::BlurredRoundedRect {
                transform: b.transform.as_coeffs(),
                rect: [b.rect.x0, b.rect.y0, b.rect.x1, b.rect.y1],
                color: rgba8(&b.color),
                radius: b.radius,
                std_dev: b.std_dev,
                composite: composite_entry(&b.composite)?,
            },
        })
    }

    /// Wire indices are assigned on FIRST ENCOUNTER while walking the
    /// commands in order (plus masks pulled in through groups).
    fn commands(&mut self, scene: &Scene) -> Result<Vec<CommandEntry>, String> {
        // Table-index bookkeeping alongside the shared maps: contexts are
        // encountered inline, clips/groups/draws too.
        let mut ctx_idx: HashMap<ContextId, usize> = HashMap::new();
        let mut clip_idx: HashMap<imaging::record::ClipId, usize> = HashMap::new();
        let mut group_idx: HashMap<imaging::record::GroupId, usize> = HashMap::new();
        let mut draw_idx: HashMap<imaging::record::DrawId, usize> = HashMap::new();
        let mut out = Vec::with_capacity(scene.commands().len());
        for command in scene.commands() {
            let entry = match command {
                Command::PushContext(id) => {
                    let idx = match ctx_idx.get(id) {
                        Some(i) => *i,
                        None => {
                            let i = self.context(scene, *id)?;
                            ctx_idx.insert(*id, i);
                            i
                        }
                    };
                    CommandEntry::PushContext(idx)
                }
                Command::PopContext => CommandEntry::PopContext,
                Command::PushClip(id) => {
                    let idx = match clip_idx.get(id) {
                        Some(i) => *i,
                        None => {
                            let i = self.data.clips.len();
                            let entry = self.clip(scene.clip(*id))?;
                            self.data.clips.push(entry);
                            clip_idx.insert(*id, i);
                            i
                        }
                    };
                    CommandEntry::PushClip(idx)
                }
                Command::PopClip => CommandEntry::PopClip,
                Command::PushGroup(id) => {
                    let idx = match group_idx.get(id) {
                        Some(i) => *i,
                        None => {
                            let i = self.data.groups.len();
                            let entry = self.group(scene, scene.group(*id))?;
                            self.data.groups.push(entry);
                            group_idx.insert(*id, i);
                            i
                        }
                    };
                    CommandEntry::PushGroup(idx)
                }
                Command::PopGroup => CommandEntry::PopGroup,
                Command::Draw(id) => {
                    let idx = match draw_idx.get(id) {
                        Some(i) => *i,
                        None => {
                            let i = self.data.draws.len();
                            let entry = self.draw(scene.draw_op(*id))?;
                            self.data.draws.push(entry);
                            draw_idx.insert(*id, i);
                            i
                        }
                    };
                    CommandEntry::Draw(idx)
                }
            };
            out.push(entry);
        }
        Ok(out)
    }
}

fn scene_data(scene: &Scene) -> Result<SceneData, String> {
    let mut ser = Serializer::new();
    let commands = ser.commands(scene)?;
    Ok(SceneData {
        fonts: ser.data.fonts,
        images: ser.data.images,
        labels: ser.data.labels,
        files: ser.data.files,
        contexts: ser.data.contexts,
        clips: ser.data.clips,
        masks: ser.data.masks,
        groups: ser.data.groups,
        draws: ser.data.draws,
        commands,
    })
}

// ───────────────────────────── deserialize ─────────────────────────────

struct Deserializer<'a> {
    data: &'a SceneData,
    mask_ids: HashMap<usize, MaskId>,
    /// Font blobs materialized once per table entry; every glyph run
    /// referencing the entry shares the same Arc (Blob equality is
    /// identity-based and the wire format promises a single copy).
    font_blobs: Vec<peniko::Blob<u8>>,
    /// Same sharing for image blobs.
    image_blobs: Vec<peniko::Blob<u8>>,
}

impl<'a> Deserializer<'a> {
    fn coeffs(c: &Coeffs) -> Affine {
        Affine::new(*c)
    }

    fn geometry(entry: &GeometryEntry) -> Geometry {
        match entry {
            GeometryEntry::Rect([x0, y0, x1, y1]) => Geometry::Rect(Rect::new(*x0, *y0, *x1, *y1)),
            GeometryEntry::RoundedRect { rect, radii } => Geometry::RoundedRect(
                RoundedRect::new(
                    rect[0],
                    rect[1],
                    rect[2],
                    rect[3],
                    RoundedRectRadii::new(radii[0], radii[1], radii[2], radii[3]),
                ),
            ),
            GeometryEntry::Path(els) => Geometry::Path(imaging::kurbo::BezPath::from_vec(
                els.iter()
                    .map(|el| match *el {
                        PathElEntry::M([x, y]) => PathEl::MoveTo(imaging::kurbo::Point::new(x, y)),
                        PathElEntry::L([x, y]) => PathEl::LineTo(imaging::kurbo::Point::new(x, y)),
                        PathElEntry::Q([cx, cy, x, y]) => {
                            PathEl::QuadTo(imaging::kurbo::Point::new(cx, cy), imaging::kurbo::Point::new(x, y))
                        }
                        PathElEntry::C([c1x, c1y, c2x, c2y, x, y]) => PathEl::CurveTo(
                            imaging::kurbo::Point::new(c1x, c1y),
                            imaging::kurbo::Point::new(c2x, c2y),
                            imaging::kurbo::Point::new(x, y),
                        ),
                        PathElEntry::Z => PathEl::ClosePath,
                    })
                    .collect(),
            )),
        }
    }

    fn stroke(entry: &StrokeEntry) -> Result<Stroke, String> {
        Ok(Stroke {
            width: entry.width,
            miter_limit: entry.miter_limit,
            start_cap: cap_from_name(&entry.start_cap)?,
            end_cap: cap_from_name(&entry.end_cap)?,
            join: match entry.join.as_str() {
                "bevel" => Join::Bevel,
                "round" => Join::Round,
                "miter" => Join::Miter,
                other => return Err(format!("unknown line join {other:?}")),
            },
            dash_pattern: entry.dash_pattern.iter().copied().collect(),
            dash_offset: entry.dash_offset,
        })
    }

    fn clip(&self, entry: &ClipEntry) -> Result<Clip, String> {
        Ok(match entry {
            ClipEntry::Fill {
                transform,
                shape,
                fill_rule,
            } => Clip::Fill {
                transform: Self::coeffs(transform),
                shape: Self::geometry(shape),
                fill_rule: fill_rule_from_name(fill_rule)?,
            },
            ClipEntry::Stroke {
                transform,
                shape,
                stroke,
            } => Clip::Stroke {
                transform: Self::coeffs(transform),
                shape: Self::geometry(shape),
                stroke: Self::stroke(stroke)?,
            },
        })
    }

    fn mask(&mut self, entry: &MaskEntry) -> Result<Mask, String> {
        Ok(Mask::new(
            match entry.mode.as_str() {
                "alpha" => MaskMode::Alpha,
                "luminance" => MaskMode::Luminance,
                other => return Err(format!("unknown mask mode {other:?}")),
            },
            rebuild_scene(&entry.scene)?,
        ))
    }

    fn ensure_mask(&mut self, idx: usize, target: &mut Scene) -> Result<MaskId, String> {
        if let Some(id) = self.mask_ids.get(&idx) {
            return Ok(*id);
        }
        let entry = self
            .data
            .masks
            .get(idx)
            .ok_or_else(|| format!("mask index {idx} out of bounds"))?;
        let mask = self.mask(entry)?;
        let id = target.define_mask(mask);
        self.mask_ids.insert(idx, id);
        Ok(id)
    }

    fn group(&mut self, entry: &GroupEntry, target: &mut Scene) -> Result<Group, String> {
        let mask = match &entry.mask {
            Some(applied) => Some(record::AppliedMask {
                mask: self.ensure_mask(applied.mask, target)?,
                transform: Self::coeffs(&applied.transform),
            }),
            None => None,
        };
        Ok(Group {
            clip: match &entry.clip {
                Some(c) => Some(self.clip(c)?),
                None => None,
            },
            mask,
            filters: entry
                .filters
                .iter()
                .map(filter_from_entry)
                .collect::<Result<Vec<_>, _>>()?,
            composite: composite_from_entry(&entry.composite),
        })
    }

    fn brush(&self, entry: &BrushEntry) -> Result<peniko::Brush, String> {
        Ok(match entry {
            BrushEntry::Solid(rgba) => peniko::Brush::Solid(peniko::Color::from_rgba8(
                rgba[0], rgba[1], rgba[2], rgba[3],
            )),
            BrushEntry::Image {
                image,
                x_extend,
                y_extend,
                quality,
                alpha,
            } => {
                let entry = self
                    .data
                    .images
                    .get(*image)
                    .ok_or_else(|| format!("image index {image} out of bounds"))?;
                peniko::Brush::Image(peniko::ImageBrush {
                    image: peniko::ImageData {
                        data: self.image_blobs[*image].clone(),
                        format: image_format_from_name(&entry.format)?,
                        alpha_type: image_alpha_from_name(&entry.alpha_type)?,
                        width: entry.width,
                        height: entry.height,
                    },
                    sampler: peniko::ImageSampler {
                        x_extend: extend_from_name(x_extend)?,
                        y_extend: extend_from_name(y_extend)?,
                        quality: image_quality_from_name(quality)?,
                        alpha: *alpha,
                    },
                })
            }
        })
    }

    fn draw(&mut self, entry: &DrawEntry) -> Result<Draw, String> {
        Ok(match entry {
            DrawEntry::Fill {
                transform,
                fill_rule,
                brush,
                brush_transform,
                shape,
                composite,
            } => Draw::Fill {
                transform: Self::coeffs(transform),
                fill_rule: fill_rule_from_name(fill_rule)?,
                brush: self.brush(brush)?,
                brush_transform: brush_transform.as_ref().map(Self::coeffs),
                shape: Self::geometry(shape),
                composite: composite_from_entry(composite),
            },
            DrawEntry::Stroke {
                transform,
                stroke,
                brush,
                brush_transform,
                shape,
                composite,
            } => Draw::Stroke {
                transform: Self::coeffs(transform),
                stroke: Self::stroke(stroke)?,
                brush: self.brush(brush)?,
                brush_transform: brush_transform.as_ref().map(Self::coeffs),
                shape: Self::geometry(shape),
                composite: composite_from_entry(composite),
            },
            DrawEntry::GlyphRun(run) => Draw::GlyphRun(GlyphRun {
                font: {
                    let entry = self
                        .data
                        .fonts
                        .get(run.font)
                        .ok_or_else(|| format!("font index {} out of bounds", run.font))?;
                    FontData {
                        data: self.font_blobs[run.font].clone(),
                        index: entry.index,
                    }
                },
                font_size: run.font_size,
                transform: Self::coeffs(&run.transform),
                glyph_transform: run.glyph_transform.as_ref().map(Self::coeffs),
                brush_transform: run.brush_transform.as_ref().map(Self::coeffs),
                font_embolden: Vec2::new(run.embolden[0], run.embolden[1]),
                hint: run.hint,
                normalized_coords: run.normalized_coords.clone(),
                style: match (&run.style_fill, &run.style_stroke) {
                    (Some(rule), None) => Style::Fill(fill_rule_from_name(rule)?),
                    (None, Some(s)) => Style::Stroke(Self::stroke(s)?),
                    _ => return Err("glyph run style must be fill or stroke".to_string()),
                },
                glyphs: run
                    .glyphs
                    .iter()
                    .map(|[id, x, y]| Glyph {
                        id: *id as u32,
                        x: *x,
                        y: *y,
                    })
                    .collect(),
                brush: self.brush(&run.brush)?,
                composite: composite_from_entry(&run.composite),
            }),
            DrawEntry::BlurredRoundedRect {
                transform,
                rect,
                color,
                radius,
                std_dev,
                composite,
            } => Draw::BlurredRoundedRect(imaging::BlurredRoundedRect {
                transform: Self::coeffs(transform),
                rect: Rect::new(rect[0], rect[1], rect[2], rect[3]),
                color: peniko::Color::from_rgba8(color[0], color[1], color[2], color[3]),
                radius: *radius,
                std_dev: *std_dev,
                composite: composite_from_entry(composite),
            }),
        })
    }
}

fn rebuild_scene(data: &SceneData) -> Result<Scene, String> {
    let mut target = Scene::new();
    let mut de = Deserializer {
        data,
        mask_ids: HashMap::new(),
        font_blobs: data
            .fonts
            .iter()
            .map(|f| peniko::Blob::new(Arc::new(f.bytes.clone())))
            .collect(),
        image_blobs: data
            .images
            .iter()
            .map(|i| peniko::Blob::new(Arc::new(i.bytes.clone())))
            .collect(),
    };
    for command in &data.commands {
        match command {
            CommandEntry::PushContext(idx) => {
                let entry = data
                    .contexts
                    .get(*idx)
                    .ok_or_else(|| format!("context index {idx} out of bounds"))?;
                let kind = match &entry.kind {
                    ContextKindEntry::Label => imaging::ContextKindRef::Label,
                    ContextKindEntry::Widget => imaging::ContextKindRef::Widget,
                    ContextKindEntry::ChildIndex => imaging::ContextKindRef::ChildIndex,
                    ContextKindEntry::Slot => imaging::ContextKindRef::Slot,
                    ContextKindEntry::Named(i) => {
                        let name = data
                            .labels
                            .get(*i)
                            .ok_or_else(|| format!("label index {i} out of bounds"))?;
                        imaging::ContextKindRef::Named(name.as_str())
                    }
                };
                let value = match &entry.value {
                    ContextValueEntry::Str(i) => {
                        let s = data
                            .labels
                            .get(*i)
                            .ok_or_else(|| format!("label index {i} out of bounds"))?;
                        imaging::ContextValueRef::Str(s.as_str())
                    }
                    ContextValueEntry::U64(v) => imaging::ContextValueRef::U64(*v),
                    ContextValueEntry::I64(v) => imaging::ContextValueRef::I64(*v),
                    ContextValueEntry::Usize(v) => imaging::ContextValueRef::Usize(*v),
                    ContextValueEntry::Bool(v) => imaging::ContextValueRef::Bool(*v),
                };
                let source = match &entry.source {
                    Some(src) => {
                        let file = data
                            .files
                            .get(src.file)
                            .ok_or_else(|| format!("file index {} out of bounds", src.file))?;
                        Some(imaging::SourceLocationRef {
                            file: file.as_str(),
                            line: src.line,
                            column: src.column,
                        })
                    }
                    None => None,
                };
                target.push_context_ref(imaging::ContextRef { kind, value, source });
            }
            CommandEntry::PopContext => target.pop_context(),
            CommandEntry::PushClip(idx) => {
                let entry = data
                    .clips
                    .get(*idx)
                    .ok_or_else(|| format!("clip index {idx} out of bounds"))?;
                let clip = de.clip(entry)?;
                target.push_clip(clip);
            }
            CommandEntry::PopClip => target.pop_clip(),
            CommandEntry::PushGroup(idx) => {
                let entry = data
                    .groups
                    .get(*idx)
                    .ok_or_else(|| format!("group index {idx} out of bounds"))?;
                let group = de.group(entry, &mut target)?;
                target.push_group(group);
            }
            CommandEntry::PopGroup => target.pop_group(),
            CommandEntry::Draw(idx) => {
                let entry = data
                    .draws
                    .get(*idx)
                    .ok_or_else(|| format!("draw index {idx} out of bounds"))?;
                let draw = de.draw(entry)?;
                target.draw(draw);
            }
        }
    }
    target
        .validate()
        .map_err(|e| format!("rebuilt scene is unbalanced: {e:?}"))?;
    Ok(target)
}

// ───────────────────────────── serde impls ─────────────────────────────

impl Serialize for SceneWire {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        scene_data(&self.0)
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SceneWire {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let data = SceneData::deserialize(deserializer)?;
        let scene = rebuild_scene(&data).map_err(serde::de::Error::custom)?;
        Ok(SceneWire(scene))
    }
}

// ───────────────────────── name mappings ─────────────────────────────


/// `AlphaColor` → RGBA bytes (the wire color representation).
fn rgba8(c: &peniko::Color) -> [u8; 4] {
    let c = c.to_rgba8();
    [c.r, c.g, c.b, c.a]
}

fn fill_rule_name(rule: peniko::Fill) -> &'static str {
    match rule {
        peniko::Fill::NonZero => "non_zero",
        peniko::Fill::EvenOdd => "even_odd",
    }
}

fn fill_rule_from_name(name: &str) -> Result<peniko::Fill, String> {
    match name {
        "non_zero" => Ok(peniko::Fill::NonZero),
        "even_odd" => Ok(peniko::Fill::EvenOdd),
        other => Err(format!("unknown fill rule {other:?}")),
    }
}

fn cap_name(c: Cap) -> &'static str {
    match c {
        Cap::Butt => "butt",
        Cap::Round => "round",
        Cap::Square => "square",
    }
}

fn cap_from_name(name: &str) -> Result<Cap, String> {
    match name {
        "butt" => Ok(Cap::Butt),
        "round" => Ok(Cap::Round),
        "square" => Ok(Cap::Square),
        other => Err(format!("unknown line cap {other:?}")),
    }
}

fn stroke_entry(s: &Stroke) -> Result<StrokeEntry, String> {
    Ok(StrokeEntry {
        width: s.width,
        miter_limit: s.miter_limit,
        start_cap: cap_name(s.start_cap).to_string(),
        end_cap: cap_name(s.end_cap).to_string(),
        join: match s.join {
            Join::Bevel => "bevel",
            Join::Round => "round",
            Join::Miter => "miter",
        }
        .to_string(),
        dash_pattern: s.dash_pattern.to_vec(),
        dash_offset: s.dash_offset,
    })
}

fn composite_entry(c: &Composite) -> Result<CompositeEntry, String> {
    if c.blend != peniko::BlendMode::default() {
        return Err("non-default blend modes are unsupported in the wire format v1".to_string());
    }
    Ok(CompositeEntry { alpha: c.alpha })
}

fn composite_from_entry(e: &CompositeEntry) -> Composite {
    Composite {
        blend: peniko::BlendMode::default(),
        alpha: e.alpha,
    }
}

fn filter_entry(f: &imaging::Filter) -> Result<FilterEntry, String> {
    Ok(match f {
        imaging::Filter::Flood { color } => FilterEntry::Flood {
            color: rgba8(color),
        },
        imaging::Filter::Blur {
            std_deviation_x,
            std_deviation_y,
        } => FilterEntry::Blur {
            std_deviation_x: *std_deviation_x,
            std_deviation_y: *std_deviation_y,
        },
        imaging::Filter::DropShadow {
            dx,
            dy,
            std_deviation_x,
            std_deviation_y,
            color,
        } => FilterEntry::DropShadow {
            dx: *dx,
            dy: *dy,
            std_deviation_x: *std_deviation_x,
            std_deviation_y: *std_deviation_y,
            color: rgba8(color),
        },
        imaging::Filter::Offset { dx, dy } => FilterEntry::Offset {
            dx: *dx,
            dy: *dy,
        },
    })
}

fn filter_from_entry(e: &FilterEntry) -> Result<imaging::Filter, String> {
    Ok(match e {
        FilterEntry::Flood { color } => imaging::Filter::Flood {
            color: peniko::Color::from_rgba8(color[0], color[1], color[2], color[3]),
        },
        FilterEntry::Blur {
            std_deviation_x,
            std_deviation_y,
        } => imaging::Filter::Blur {
            std_deviation_x: *std_deviation_x,
            std_deviation_y: *std_deviation_y,
        },
        FilterEntry::DropShadow {
            dx,
            dy,
            std_deviation_x,
            std_deviation_y,
            color,
        } => imaging::Filter::DropShadow {
            dx: *dx,
            dy: *dy,
            std_deviation_x: *std_deviation_x,
            std_deviation_y: *std_deviation_y,
            color: peniko::Color::from_rgba8(color[0], color[1], color[2], color[3]),
        },
        FilterEntry::Offset { dx, dy } => imaging::Filter::Offset {
            dx: *dx,
            dy: *dy,
        },
    })
}

fn image_format_name(f: peniko::ImageFormat) -> Result<String, String> {
    Ok(match f {
        peniko::ImageFormat::Rgba8 => "rgba8",
        peniko::ImageFormat::Bgra8 => "bgra8",
        _ => return Err(format!("image format {f:?} unsupported in wire v1")),
    }
    .to_string())
}

fn image_format_from_name(name: &str) -> Result<peniko::ImageFormat, String> {
    match name {
        "rgba8" => Ok(peniko::ImageFormat::Rgba8),
        "bgra8" => Ok(peniko::ImageFormat::Bgra8),
        other => Err(format!("unknown image format {other:?}")),
    }
}

fn image_alpha_name(a: peniko::ImageAlphaType) -> Result<String, String> {
    Ok(match a {
        peniko::ImageAlphaType::Alpha => "alpha",
        peniko::ImageAlphaType::AlphaPremultiplied => "alpha_premultiplied",
    }
    .to_string())
}

fn image_alpha_from_name(name: &str) -> Result<peniko::ImageAlphaType, String> {
    match name {
        "alpha" => Ok(peniko::ImageAlphaType::Alpha),
        "alpha_premultiplied" => Ok(peniko::ImageAlphaType::AlphaPremultiplied),
        other => Err(format!("unknown image alpha type {other:?}")),
    }
}

fn extend_name(e: peniko::Extend) -> Result<&'static str, String> {
    match e {
        peniko::Extend::Pad => Ok("pad"),
        peniko::Extend::Repeat => Ok("repeat"),
        peniko::Extend::Reflect => Ok("reflect"),
    }
}

fn extend_from_name(name: &str) -> Result<peniko::Extend, String> {
    match name {
        "pad" => Ok(peniko::Extend::Pad),
        "repeat" => Ok(peniko::Extend::Repeat),
        "reflect" => Ok(peniko::Extend::Reflect),
        other => Err(format!("unknown extend mode {other:?}")),
    }
}

fn image_quality_name(q: peniko::ImageQuality) -> Result<&'static str, String> {
    match q {
        peniko::ImageQuality::Low => Ok("low"),
        peniko::ImageQuality::Medium => Ok("medium"),
        peniko::ImageQuality::High => Ok("high"),
    }
}

fn image_quality_from_name(name: &str) -> Result<peniko::ImageQuality, String> {
    match name {
        "low" => Ok(peniko::ImageQuality::Low),
        "medium" => Ok(peniko::ImageQuality::Medium),
        "high" => Ok(peniko::ImageQuality::High),
        other => Err(format!("unknown image quality {other:?}")),
    }
}

// ───────────────────────────── public payload ─────────────────────────────

/// One page of a print payload: paper + resolved orientation + the
/// page-local scene (origin at the page's top-left, pixels at 96 dpi).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageScene {
    pub paper: Paper,
    /// Always a concrete orientation (`Auto` is resolved before here).
    pub orientation: Orientation,
    pub width_px: f32,
    pub height_px: f32,
    pub scene: SceneWire,
}

/// What gets submitted to a backend (design §6.1): the Windows GDI vector
/// path consumes [`PrintPayload::Pages`]; the Linux CUPS path consumes
/// [`PrintPayload::Pdf`]. Range/parity planning happens host-side — the
/// payload carries exactly the planned pages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PrintPayload {
    Pages { pages: Vec<PageScene> },
    Pdf { bytes: Vec<u8> },
}

#[cfg(test)]
mod tests {
    use super::*;
    use imaging::peniko::Color;

    /// A scene exercising every wire-covered payload family: fills, a
    /// dashed stroke, a clip, a masked semi-transparent group (with a
    /// filter), two glyph runs SHARING one font Arc (dedup check), and a
    /// blurred rect. The mask is defined before the group references it.
    fn kitchen_sink_scene() -> Scene {
        let font_bytes = Arc::new(vec![0xEFu8, 0x00, 0x01, 0x00, 0x99, 0x77]);
        let font = FontData {
            data: peniko::Blob::new(font_bytes),
            index: 0,
        };
        let mut scene = Scene::new();
        scene.draw(Draw::Fill {
            transform: Affine::IDENTITY,
            fill_rule: peniko::Fill::NonZero,
            brush: peniko::Brush::Solid(Color::from_rgba8(255, 255, 255, 255)),
            brush_transform: None,
            shape: Geometry::Rect(Rect::new(0.0, 0.0, 595.0, 842.0)),
            composite: Composite::default(),
        });
        scene.draw(Draw::Stroke {
            transform: Affine::translate((10.0, 10.0)),
            stroke: Stroke {
                width: 1.5,
                miter_limit: 4.0,
                start_cap: Cap::Round,
                end_cap: Cap::Butt,
                join: Join::Miter,
                dash_pattern: [4.0, 2.0].into_iter().collect(),
                dash_offset: 1.0,
            },
            brush: peniko::Brush::Solid(Color::from_rgba8(0, 0, 0, 255)),
            brush_transform: None,
            shape: Geometry::Path({
                let mut p = imaging::kurbo::BezPath::new();
                p.move_to((0.0, 0.0));
                p.quad_to((10.0, 20.0), (40.0, 8.0));
                p.close_path();
                p
            }),
            composite: Composite::default(),
        });
        scene.push_clip(Clip::Fill {
            transform: Affine::IDENTITY,
            shape: Geometry::RoundedRect(RoundedRect::new(
                0.0,
                0.0,
                100.0,
                50.0,
                RoundedRectRadii::new(4.0, 4.0, 0.0, 0.0),
            )),
            fill_rule: peniko::Fill::NonZero,
        });
        scene.draw(Draw::Fill {
            transform: Affine::scale(2.0),
            fill_rule: peniko::Fill::EvenOdd,
            brush: peniko::Brush::Solid(Color::from_rgba8(200, 30, 30, 128)),
            brush_transform: Some(Affine::translate((1.0, 2.0))),
            shape: Geometry::Rect(Rect::new(1.0, 1.0, 30.0, 20.0)),
            composite: Composite {
                blend: peniko::BlendMode::default(),
                alpha: 0.9,
            },
        });
        scene.pop_clip();
        let glyph_run = |brush: peniko::Brush| Draw::GlyphRun(GlyphRun {
            font: FontData {
                data: font.data.clone(),
                index: font.index,
            },
            transform: Affine::translate((50.0, 500.0)),
            glyph_transform: Some(Affine::skew(0.2, 0.0)),
            brush_transform: None,
            font_size: 16.0,
            font_embolden: Vec2::new(1.0, 0.0),
            hint: true,
            normalized_coords: vec![100, -100],
            style: Style::Fill(peniko::Fill::NonZero),
            glyphs: vec![
                Glyph { id: 12, x: 0.0, y: 0.0 },
                Glyph { id: 3456, x: 16.0, y: 2.5 },
            ],
            brush,
            composite: Composite::default(),
        });
        scene.draw(glyph_run(peniko::Brush::Solid(Color::from_rgba8(0, 0, 0, 255))));
        scene.draw(glyph_run(peniko::Brush::Solid(Color::from_rgba8(60, 60, 60, 255))));
        let mask_id = {
            let mut mask_scene = Scene::new();
            mask_scene.draw(Draw::Fill {
                transform: Affine::IDENTITY,
                fill_rule: peniko::Fill::NonZero,
                brush: peniko::Brush::Solid(Color::from_rgba8(255, 255, 255, 255)),
                brush_transform: None,
                shape: Geometry::Rect(Rect::new(0.0, 0.0, 80.0, 80.0)),
                composite: Composite::default(),
            });
            scene.define_mask(Mask::new(MaskMode::Alpha, mask_scene))
        };
        scene.push_group(Group {
            clip: None,
            mask: Some(record::AppliedMask {
                mask: mask_id,
                transform: Affine::IDENTITY,
            }),
            filters: vec![imaging::Filter::Blur {
                std_deviation_x: 2.0,
                std_deviation_y: 3.0,
            }],
            composite: Composite {
                blend: peniko::BlendMode::default(),
                alpha: 0.5,
            },
        });
        scene.draw(Draw::Fill {
            transform: Affine::IDENTITY,
            fill_rule: peniko::Fill::NonZero,
            brush: peniko::Brush::Solid(Color::from_rgba8(0, 120, 215, 255)),
            brush_transform: None,
            shape: Geometry::Rect(Rect::new(200.0, 200.0, 280.0, 280.0)),
            composite: Composite::default(),
        });
        scene.pop_group();
        scene.draw(Draw::BlurredRoundedRect(imaging::BlurredRoundedRect {
            transform: Affine::IDENTITY,
            rect: Rect::new(10.0, 700.0, 210.0, 760.0),
            color: Color::from_rgba8(0, 0, 0, 60),
            radius: 8.0,
            std_dev: 4.0,
            composite: Composite::default(),
        }));
        scene
    }

    #[test]
    fn scene_round_trips_losslessly() {
        let scene = kitchen_sink_scene();
        let json = serde_json::to_string(&SceneWire(scene.clone())).expect("serialize");
        let back: SceneWire = serde_json::from_str(&json).expect("deserialize");

        // peniko `Blob` identity is a per-instance global counter, so whole-
        // scene `assert_eq!` can never hold across a round trip. The
        // lossless claim decomposes instead into:
        // (a) the wire form re-serializes byte-identically from the rebuilt
        //     scene (every field survived),
        let rebuilt_json = serde_json::to_string(&back).expect("re-serialize");
        assert_eq!(json, rebuilt_json, "wire form must be stable across round-trips");
        // (b) the command stream matches exactly,
        assert_eq!(back.0.commands(), scene.commands());
        // (c) blobs that were shared stay shared (one Arc per table entry).
        let mut font_ptrs: Vec<usize> = Vec::new();
        for command in back.0.commands() {
            if let Command::Draw(id) = command {
                if let Draw::GlyphRun(run) = back.0.draw_op(*id) {
                    font_ptrs.push(run.font.data.as_ref().as_ptr() as usize);
                }
            }
        }
        assert!(font_ptrs.len() >= 2, "fixture has two glyph runs");
        assert!(
            font_ptrs.windows(2).all(|w| w[0] == w[1]),
            "all runs must share one font Arc, got {font_ptrs:?}"
        );
    }

    #[test]
    fn font_bytes_dedup_to_one_entry() {
        let scene = kitchen_sink_scene();
        let json = serde_json::to_string(&SceneWire(scene)).expect("serialize");
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let fonts = value["fonts"].as_array().expect("fonts table");
        assert_eq!(fonts.len(), 1, "shared font Arc must dedup: {fonts:?}");
        assert_eq!(fonts[0]["bytes"].as_array().unwrap().len(), 6);
    }

    #[test]
    fn empty_scene_round_trips() {
        let wire = SceneWire::default();
        let json = serde_json::to_string(&wire).unwrap();
        let back: SceneWire = serde_json::from_str(&json).unwrap();
        assert_eq!(back, wire);
    }

    #[test]
    fn payload_json_round_trip() {
        let payload = PrintPayload::Pages { pages: vec![PageScene {
            paper: Paper::A4,
            orientation: Orientation::Portrait,
            width_px: 794.0,
            height_px: 1123.0,
            scene: SceneWire(kitchen_sink_scene()),
        }] };
        let json = serde_json::to_string(&payload).unwrap();
        let back: PrintPayload = serde_json::from_str(&json).unwrap();
        match back {
            PrintPayload::Pages { pages } => {
                assert_eq!(pages.len(), 1);
                assert_eq!(pages[0].paper, Paper::A4);
                assert_eq!(pages[0].width_px, 794.0);
            }
            _ => panic!("wrong variant"),
        }
        let pdf = PrintPayload::Pdf { bytes: vec![1, 2, 3] };
        let back: PrintPayload = serde_json::from_str(&serde_json::to_string(&pdf).unwrap()).unwrap();
        assert!(matches!(back, PrintPayload::Pdf { .. }));
        assert!(serde_json::to_string(&pdf).unwrap().contains(r#""kind":"pdf""#));
    }
}
