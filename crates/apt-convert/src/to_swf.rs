//! Build a standard, viewable SWF from a parsed APT movie.
//!
//! APT shape geometry is pre-tessellated triangle lists (in the aux `.ru`
//! files); we emit each triangle as a closed, filled SWF path so the result
//! renders in Ruffle. Textured triangles become real SWF bitmap fills when the
//! [`Assets`] supply the images, and fall back to their vertex color as a solid
//! fill otherwise.

use std::collections::HashMap;

use apt::{AptFile, Character, CharacterSlot, Control, Frame, PlaceObject};
use apt_aux::{ShapeGeometry, Style, Texture};

use crate::assets::{Assets, TextureKey};
use swf::{
    BitmapFormat, CharacterId, Color, ColorTransform, Compression, DefineBitsLossless, Depth,
    FillStyle, Fixed16, Fixed8, Header, Matrix, PlaceObjectAction, Point, PointDelta, Rectangle,
    Shape, ShapeFlag, ShapeRecord, ShapeStyles, Sprite, StyleChangeData, SwfStr, Tag, Twips,
};

use crate::bytecode::apt_to_swf_actions;
use crate::{Error, Result};

/// Convert an APT movie to SWF bytes, using `assets` for shape geometry and
/// textures.
///
/// Imports must already be resolved (see [`crate::imports::resolve`]); any
/// character slot still empty here simply has nothing to draw.
pub fn apt_to_swf(file: &AptFile, assets: &dyn Assets) -> Result<Vec<u8>> {
    apt_to_swf_with(file, assets, None)
}

/// As [`apt_to_swf`], optionally replacing the movie's background color.
pub fn apt_to_swf_with(
    file: &AptFile,
    assets: &dyn Assets,
    override_background: Option<u32>,
) -> Result<Vec<u8>> {
    let movie = &file.movie;

    // Import URLs are owned here because the tags borrow them.
    let import_groups = group_imports(&movie.imports);
    let import_urls: Vec<String> = import_groups
        .iter()
        .map(|(name, _)| format!("{name}.swf"))
        .collect();

    // Shape/sprite SWF ids mirror the APT character index. Extra synthetic
    // characters (button hit-test shapes, then bitmaps) get ids past the end
    // of the character table.
    let button_count = movie
        .characters
        .iter()
        .filter(|slot| matches!(slot, CharacterSlot::Character(Character::Button(_))))
        .count();
    let hit_shape_base = movie.characters.len() as CharacterId;
    let mut bitmaps = BitmapTable::new(hit_shape_base + button_count as CharacterId);

    // Characters that appear in some PlaceObject, on the root timeline or any
    // sprite's. Most Bitmap characters are never placed directly — they exist
    // purely as texture providers for Shape units (referenced by
    // `bitmap_character_id` in their `.ru` geometry) — and synthesizing a
    // full-image quad for them is both wasted and, for a large shared atlas,
    // unencodable (a single SWF straight edge caps out around 3277px).
    let placed_ids = placed_character_ids(movie);

    let mut shapes: Vec<Shape> = Vec::new();
    let mut sprites: Vec<(CharacterId, Vec<Frame>)> = Vec::new();
    let mut buttons: Vec<(CharacterId, &apt::Button)> = Vec::new();
    let mut texts: Vec<(CharacterId, &apt::Text)> = Vec::new();
    // Bitmap characters placed directly on a timeline: (id, texture SWF id, w, h).
    let mut placed_bitmaps: Vec<(CharacterId, CharacterId, u32, u32)> = Vec::new();
    for (i, slot) in movie.characters.iter().enumerate() {
        if let CharacterSlot::Character(ch) = slot {
            match ch {
                Character::Shape(s) => {
                    let geom = assets.geometry(i as u32).unwrap_or_default();
                    bitmaps.collect(i as u32, &geom, assets);
                    shapes.push(build_shape(i as CharacterId, &geom, &s.bounds, &bitmaps));
                }
                Character::Sprite(sp) => sprites.push((i as CharacterId, sp.frames.clone())),
                Character::Button(b) => buttons.push((i as CharacterId, b)),
                Character::Text(t) => texts.push((i as CharacterId, t)),
                Character::Bitmap if placed_ids.contains(&(i as u32)) => {
                    // A Bitmap character's texture is keyed by its own index;
                    // it renders as a shape covering the image.
                    if let Some((id, w, h)) = bitmaps.register(i as u32, i as u32, assets) {
                        placed_bitmaps.push((i as CharacterId, id, w, h));
                    }
                }
                _ => {}
            }
        }
    }

    // Phase 1: encode every action stream into owned buffers, in a fixed order
    // (button conditions, then each sprite's frames, then the main timeline)
    // that phase 2 replays exactly.
    let mut button_actions: Vec<Vec<u8>> = Vec::new();
    for (_, b) in &buttons {
        for act in &b.actions {
            button_actions.push(apt_to_swf_actions(&act.actions)?);
        }
    }
    let mut action_data: Vec<Vec<u8>> = Vec::new();
    for (_, frames) in &sprites {
        encode_frame_actions(frames, &mut action_data)?;
    }
    encode_frame_actions(&movie.frames, &mut action_data)?;

    // Phase 2: build the tag graph borrowing the encoded buffers.
    let mut tags: Vec<Tag> = Vec::new();

    // APT pulls characters out of a sibling movie by export name; SWF says the
    // same thing with ImportAssets against that movie's `.swf`, which the
    // player resolves relative to this movie. Convert the whole family so the
    // siblings are actually there (see `convert_movie_with_imports`).
    for (url, (_, imports)) in import_urls.iter().zip(&import_groups) {
        tags.push(Tag::ImportAssets {
            url: SwfStr::from_utf8_str(url),
            imports: imports.clone(),
        });
    }

    for (id, texture) in bitmaps.textures() {
        tags.push(Tag::DefineBitsLossless(DefineBitsLossless {
            version: 2,
            id,
            format: BitmapFormat::Rgb32,
            width: texture.width as u16,
            height: texture.height as u16,
            data: std::borrow::Cow::Owned(encode_bitmap_data(texture)?),
        }));
    }
    for shape in &shapes {
        tags.push(Tag::DefineShape(shape.clone()));
    }
    for &(id, texture_id, w, h) in &placed_bitmaps {
        tags.push(Tag::DefineShape(bitmap_shape(id, texture_id, w, h)));
    }
    for (id, t) in &texts {
        tags.push(Tag::DefineEditText(Box::new(build_edit_text(*id, t))));
    }

    let mut cursor = 0usize;
    for (id, frames) in &sprites {
        let (sprite_tags, used) = build_frame_tags(frames, &action_data, cursor)?;
        cursor += used;
        tags.push(Tag::DefineSprite(Sprite {
            id: *id,
            num_frames: frames.len() as u16,
            tags: sprite_tags,
        }));
    }

    // Buttons after sprites/shapes: their records reference those characters.
    //
    // The APT engine hit-tests buttons against the Button character's own
    // triangle mesh, not the (dummy, usually degenerate) shape in its
    // hit-test record — so emit that mesh as a synthetic shape per button
    // and point the SWF hit-test state at it.
    let mut button_cursor = 0usize;
    for (index, (id, b)) in buttons.iter().enumerate() {
        let hit_id = hit_shape_base + index as CharacterId;
        tags.push(Tag::DefineShape(build_hit_shape(hit_id, b)));
        let n = b.actions.len();
        tags.push(Tag::DefineButton2(Box::new(build_button(
            *id,
            b,
            hit_id,
            &button_actions[button_cursor..button_cursor + n],
        ))));
        button_cursor += n;
    }

    // Exports make our characters importable by the movies that reference us.
    if !movie.exports.is_empty() {
        tags.push(Tag::ExportAssets(
            movie
                .exports
                .iter()
                .map(|e| swf::ExportedAsset {
                    id: e.character_id as CharacterId,
                    name: SwfStr::from_utf8_str(&e.name),
                })
                .collect(),
        ));
    }

    match override_background {
        Some(rgb) => tags.push(Tag::SetBackgroundColor(Color {
            r: (rgb >> 16) as u8,
            g: (rgb >> 8) as u8,
            b: rgb as u8,
            a: 255,
        })),
        None => {
            if let Some(color) = find_background(&movie.frames) {
                tags.push(Tag::SetBackgroundColor(color));
            }
        }
    }

    let (main_tags, _) = build_frame_tags(&movie.frames, &action_data, cursor)?;
    tags.extend(main_tags);
    tags.push(Tag::End);

    let stage = Rectangle {
        x_min: Twips::ZERO,
        x_max: Twips::from_pixels(movie.width.max(1) as f64),
        y_min: Twips::ZERO,
        y_max: Twips::from_pixels(movie.height.max(1) as f64),
    };
    let fps = if movie.ms_per_frame > 0 {
        1000.0 / movie.ms_per_frame as f64
    } else {
        30.0
    };
    let header = Header {
        compression: Compression::Zlib,
        version: file.header.swf_version.max(6),
        stage_size: stage,
        frame_rate: Fixed8::from_f64(fps),
        num_frames: movie.frames.len() as u16,
    };

    let mut out = Vec::new();
    swf::write_swf(&header, &tags, &mut out).map_err(|e| Error::SwfWrite(e.to_string()))?;
    Ok(out)
}

/// Group imports by source movie, preserving first-seen order.
fn group_imports(imports: &[apt::Import]) -> Vec<(&str, Vec<swf::ExportedAsset<'_>>)> {
    let mut order: Vec<&str> = Vec::new();
    let mut by_movie: HashMap<&str, Vec<swf::ExportedAsset>> = HashMap::new();
    for import in imports {
        let entry = by_movie.entry(import.movie.as_str()).or_insert_with(|| {
            order.push(import.movie.as_str());
            Vec::new()
        });
        entry.push(swf::ExportedAsset {
            id: import.character_id as CharacterId,
            name: SwfStr::from_utf8_str(&import.name),
        });
    }
    order
        .into_iter()
        .map(|movie| {
            let assets = by_movie.remove(movie).unwrap_or_default();
            (movie, assets)
        })
        .collect()
}

/// Every character id referenced by a `PlaceObject` anywhere in the movie —
/// its root timeline or any sprite's.
fn placed_character_ids(movie: &apt::Movie) -> std::collections::HashSet<u32> {
    let mut ids = std::collections::HashSet::new();
    let mut scan = |frames: &[Frame]| {
        for f in frames {
            for c in &f.controls {
                if let Control::PlaceObject(p) = c {
                    if let Some(id) = p.character_id {
                        if id >= 0 {
                            ids.insert(id as u32);
                        }
                    }
                }
            }
        }
    };
    scan(&movie.frames);
    for slot in &movie.characters {
        if let CharacterSlot::Character(Character::Sprite(sp)) = slot {
            scan(&sp.frames);
        }
    }
    ids
}

fn find_background(frames: &[Frame]) -> Option<Color> {
    for f in frames {
        for c in &f.controls {
            if let Control::BackgroundColor(rgba) = c {
                return Some(unpack_color(*rgba));
            }
        }
    }
    None
}

/// Encode all DoAction/InitAction streams and PlaceObject clip-event handlers
/// in `frames` into `action_data`, in timeline order (matching what
/// `build_frame_tags` consumes).
fn encode_frame_actions(frames: &[Frame], action_data: &mut Vec<Vec<u8>>) -> Result<()> {
    for f in frames {
        for c in &f.controls {
            match c {
                Control::Action(s) | Control::InitAction { actions: s, .. } => {
                    action_data.push(apt_to_swf_actions(s)?);
                }
                Control::PlaceObject(p) => {
                    for block in p.clip_actions.iter().flatten() {
                        action_data.push(apt_to_swf_actions(&block.actions)?);
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Build SWF tags for `frames`, borrowing pre-encoded action buffers starting
/// at `cursor`. Returns the tags and how many action buffers it consumed.
fn build_frame_tags<'a>(
    frames: &'a [Frame],
    action_data: &'a [Vec<u8>],
    cursor: usize,
) -> Result<(Vec<Tag<'a>>, usize)> {
    let mut tags = Vec::new();
    let mut cursor = cursor;
    let start = cursor;
    for frame in frames {
        for control in &frame.controls {
            match control {
                Control::Action(_) => {
                    tags.push(Tag::DoAction(&action_data[cursor]));
                    cursor += 1;
                }
                Control::InitAction { sprite_id, .. } => {
                    tags.push(Tag::DoInitAction {
                        id: *sprite_id as CharacterId,
                        action_data: &action_data[cursor],
                    });
                    cursor += 1;
                }
                Control::PlaceObject(p) => {
                    let n = p.clip_actions.as_ref().map_or(0, |b| b.len());
                    tags.push(place_object_tag(p, &action_data[cursor..cursor + n]));
                    cursor += n;
                }
                Control::RemoveObject { depth } => {
                    tags.push(Tag::RemoveObject(swf::RemoveObject {
                        depth: *depth as Depth,
                        character_id: None,
                    }))
                }
                Control::FrameLabel(label) => tags.push(Tag::FrameLabel(swf::FrameLabel {
                    label: SwfStr::from_utf8_str(label),
                    is_anchor: false,
                })),
                // Background color is hoisted to a single tag before the
                // timeline; sounds aren't converted yet.
                Control::BackgroundColor(_)
                | Control::StartSound { .. }
                | Control::StartSoundStream { .. } => {}
            }
        }
        tags.push(Tag::ShowFrame);
    }
    Ok((tags, cursor - start))
}

fn place_object_tag<'a>(p: &'a PlaceObject, handler_data: &'a [Vec<u8>]) -> Tag<'a> {
    let action = if p.is_move && p.character_id.is_none() {
        PlaceObjectAction::Modify
    } else if p.is_move {
        PlaceObjectAction::Replace(p.character_id.unwrap_or(0) as CharacterId)
    } else {
        PlaceObjectAction::Place(p.character_id.unwrap_or(0) as CharacterId)
    };
    Tag::PlaceObject(Box::new(swf::PlaceObject {
        version: if p.is_place_object_3() { 3 } else { 2 },
        action,
        depth: p.depth as Depth,
        matrix: p.matrix.map(to_swf_matrix),
        color_transform: p.cxform.map(|c| to_swf_cxform(&c)),
        ratio: p.ratio.map(|r| (r * 65535.0) as u16),
        // The instance name is what ActionScript addresses the clip by, so it
        // has to survive the conversion.
        name: p.name.as_deref().map(SwfStr::from_utf8_str),
        clip_actions: p.clip_actions.as_ref().map(|blocks| {
            blocks
                .iter()
                .zip(handler_data)
                .map(|(block, data)| swf::ClipAction {
                    // APT's trigger mask shares SWF's ClipEventFlag bit layout.
                    events: swf::ClipEventFlag::from_bits_truncate(block.triggers as u32),
                    key_code: (block.key_code != 0).then_some(block.key_code as u8),
                    action_data: data,
                })
                .collect()
        }),
        clip_depth: p.clip_depth.map(|d| d as Depth),
        class_name: None,
        filters: None,
        background_color: None,
        blend_mode: None,
        has_image: false,
        is_bitmap_cached: None,
        is_visible: None,
        amf_data: None,
    }))
}

/// The textures a movie references, each assigned a SWF character id.
///
/// Bitmap characters that share a texture id (a packed atlas) share one entry,
/// so the image data is embedded once.
struct BitmapTable {
    next_id: CharacterId,
    /// texture identity -> SWF character id (shared images embed once).
    by_texture: HashMap<TextureKey, CharacterId>,
    /// (shape index, bitmap character id) -> SWF character id. The bitmap id
    /// alone isn't unique once characters from several movies are inlined.
    by_reference: HashMap<(u32, u32), CharacterId>,
    loaded: Vec<(CharacterId, Texture)>,
}

impl BitmapTable {
    fn new(next_id: CharacterId) -> BitmapTable {
        BitmapTable {
            next_id,
            by_texture: HashMap::new(),
            by_reference: HashMap::new(),
            loaded: Vec::new(),
        }
    }

    /// Load and register every texture `shape_index`'s geometry references.
    fn collect(&mut self, shape_index: u32, geom: &ShapeGeometry, assets: &dyn Assets) {
        for unit in &geom.units {
            let Style::Textured {
                bitmap_character_id,
                ..
            } = &unit.style
            else {
                continue;
            };
            let reference = (shape_index, *bitmap_character_id);
            if self.by_reference.contains_key(&reference) {
                continue;
            }
            let Some((key, texture)) = assets.texture(shape_index, *bitmap_character_id) else {
                continue;
            };
            let id = *self.by_texture.entry(key).or_insert_with(|| {
                let id = self.next_id;
                self.next_id += 1;
                self.loaded.push((id, texture));
                id
            });
            self.by_reference.insert(reference, id);
        }
    }

    fn character(&self, shape_index: u32, bitmap_character_id: u32) -> Option<CharacterId> {
        self.by_reference
            .get(&(shape_index, bitmap_character_id))
            .copied()
    }

    /// Register a single texture reference (used for Bitmap characters placed
    /// directly on a timeline). Returns its SWF id and dimensions.
    fn register(
        &mut self,
        shape_index: u32,
        bitmap_character_id: u32,
        assets: &dyn Assets,
    ) -> Option<(CharacterId, u32, u32)> {
        let (key, texture) = assets.texture(shape_index, bitmap_character_id)?;
        let (w, h) = (texture.width, texture.height);
        let id = *self.by_texture.entry(key).or_insert_with(|| {
            let id = self.next_id;
            self.next_id += 1;
            self.loaded.push((id, texture));
            id
        });
        self.by_reference
            .insert((shape_index, bitmap_character_id), id);
        Some((id, w, h))
    }

    fn textures(&self) -> impl Iterator<Item = (CharacterId, &Texture)> {
        self.loaded.iter().map(|(id, t)| (*id, t))
    }
}

/// Encode a texture as DefineBitsLossless2 pixel data: zlib-compressed ARGB
/// with premultiplied alpha, as the SWF format requires.
fn encode_bitmap_data(texture: &Texture) -> Result<Vec<u8>> {
    use std::io::Write;

    let mut argb = Vec::with_capacity(texture.rgba.len());
    for px in texture.rgba.chunks_exact(4) {
        let (r, g, b, a) = (px[0] as u32, px[1] as u32, px[2] as u32, px[3] as u32);
        let mul = |c: u32| ((c * a + 127) / 255) as u8;
        argb.extend_from_slice(&[a as u8, mul(r), mul(g), mul(b)]);
    }
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(&argb)
        .map_err(|e| Error::SwfWrite(e.to_string()))?;
    encoder.finish().map_err(|e| Error::SwfWrite(e.to_string()))
}

/// The SWF fill matrix for a textured unit.
///
/// The `.ru` matrix maps a vertex position (pixels) to texture pixels
/// (`u = a*x + b*y + tx`); a SWF bitmap fill needs the opposite — bitmap
/// pixels to shape twips — so we invert it and scale by 20 (the twips-per-
/// pixel factor; a bitmap drawn "1:1" in SWF uses a scale-20 fill matrix).
/// Returns `None` for a degenerate (non-invertible) matrix.
fn bitmap_fill_matrix(m: &[f32; 6]) -> Option<Matrix> {
    let (a, b, c, d, tx, ty) = (m[0], m[1], m[2], m[3], m[4], m[5]);
    let det = a * d - b * c;
    if !det.is_finite() || det.abs() < 1e-12 {
        return None;
    }
    // Inverse of the 2x2 linear part, row-major [[i00, i01], [i10, i11]].
    let (i00, i01) = (d / det, -b / det);
    let (i10, i11) = (-c / det, a / det);
    // Translation: -(A^-1 * t), in pixels.
    let trans_x = -(i00 * tx + i01 * ty);
    let trans_y = -(i10 * tx + i11 * ty);

    // A near-singular UV matrix inverts to values outside what SWF's fixed-
    // point matrix can encode; fall back to a solid fill rather than emitting
    // an unwritable tag.
    //
    // The scale/skew fields are `Fixed16` (16.16 fixed point stored as i32),
    // and the writer bit-packs each pair's raw i32 into a *5-bit* `NumBits`
    // header (so `count_sbits(raw) <= 31`). That needs `|raw| < 2^30`, i.e.
    // `|value| < 2^30 / 65536 = 2^14 = 16384` — not the much larger bound a
    // "does it fit in an i32" check would suggest. Translate is plain Twips
    // (not Fixed16) and shares that same 5-bit header, so its bound is far
    // more permissive; 5,000,000px of translation is already absurd for any
    // real shape, so that threshold just catches other garbage.
    let scale = [i00, i10, i01, i11].map(|v| v as f64 * 20.0);
    if scale.iter().any(|v| !v.is_finite() || v.abs() >= 16384.0)
        || !trans_x.is_finite()
        || !trans_y.is_finite()
        || trans_x.abs() >= 5_000_000.0
        || trans_y.abs() >= 5_000_000.0
    {
        return None;
    }
    Some(Matrix {
        a: Fixed16::from_f64(scale[0]),
        b: Fixed16::from_f64(scale[1]),
        c: Fixed16::from_f64(scale[2]),
        d: Fixed16::from_f64(scale[3]),
        tx: Twips::from_pixels(trans_x as f64),
        ty: Twips::from_pixels(trans_y as f64),
    })
}

/// Build a filled SWF shape from a triangle-list geometry.
fn build_shape(
    id: CharacterId,
    geom: &ShapeGeometry,
    bounds: &apt::Rect,
    bitmaps: &BitmapTable,
) -> Shape {
    let shape_index = id as u32;
    let mut fill_styles: Vec<FillStyle> = Vec::new();
    let mut records: Vec<ShapeRecord> = Vec::new();

    for unit in &geom.units {
        // A textured unit becomes a bitmap fill when we have its image and an
        // invertible UV matrix; otherwise its color stands in.
        let bitmap_fill = match &unit.style {
            Style::Textured {
                bitmap_character_id,
                matrix,
                clipped,
                ..
            } => bitmaps
                .character(shape_index, *bitmap_character_id)
                .zip(bitmap_fill_matrix(matrix))
                .map(|(id, matrix)| FillStyle::Bitmap {
                    id,
                    matrix,
                    is_smoothed: true,
                    is_repeating: !clipped,
                }),
            _ => None,
        };
        fill_styles.push(bitmap_fill.unwrap_or_else(|| {
            let color = match &unit.style {
                Style::Solid { color }
                | Style::Line { color, .. }
                | Style::Textured { color, .. } => *color,
            };
            FillStyle::Color(unpack_color(color))
        }));
        let fill_index = fill_styles.len() as u32;

        // Each triangle: move to v0, edges to v1, v2, back to v0.
        let verts = &unit.vertices;
        let tri_count = verts.len() / 3;
        for t in 0..tri_count {
            let v0 = verts[t * 3];
            let v1 = verts[t * 3 + 1];
            let v2 = verts[t * 3 + 2];
            records.push(ShapeRecord::StyleChange(Box::new(StyleChangeData {
                move_to: Some(px_point(v0)),
                fill_style_0: None,
                fill_style_1: Some(fill_index),
                line_style: None,
                new_styles: None,
            })));
            records.extend(edge_chain(v0, v1));
            records.extend(edge_chain(v1, v2));
            records.extend(edge_chain(v2, v0));
        }
    }

    Shape {
        version: 3,
        id,
        shape_bounds: rect_to_swf(bounds),
        edge_bounds: rect_to_swf(bounds),
        flags: ShapeFlag::empty(),
        styles: ShapeStyles {
            fill_styles,
            line_styles: vec![],
        },
        shape: records,
    }
}

/// A rectangle shape displaying a whole bitmap at 1:1 pixel scale — how a
/// Bitmap character placed directly on a timeline renders.
fn bitmap_shape(id: CharacterId, texture_id: CharacterId, w: u32, h: u32) -> Shape {
    let bounds = Rectangle {
        x_min: Twips::ZERO,
        x_max: Twips::from_pixels(w as f64),
        y_min: Twips::ZERO,
        y_max: Twips::from_pixels(h as f64),
    };
    // Bitmap fill space is bitmap pixels x 20; scale by 20 to map 1:1.
    let fill = FillStyle::Bitmap {
        id: texture_id,
        matrix: Matrix::scale(Fixed16::from_f64(20.0), Fixed16::from_f64(20.0)),
        is_smoothed: true,
        is_repeating: false,
    };
    let (w, h) = (w as f32, h as f32);
    let mut shape = vec![ShapeRecord::StyleChange(Box::new(StyleChangeData {
        move_to: Some(px_point((0.0, 0.0))),
        fill_style_0: None,
        fill_style_1: Some(1),
        line_style: None,
        new_styles: None,
    }))];
    shape.extend(edge_chain((0.0, 0.0), (w, 0.0)));
    shape.extend(edge_chain((w, 0.0), (w, h)));
    shape.extend(edge_chain((w, h), (0.0, h)));
    shape.extend(edge_chain((0.0, h), (0.0, 0.0)));
    Shape {
        version: 3,
        id,
        shape_bounds: bounds.clone(),
        edge_bounds: bounds,
        flags: ShapeFlag::empty(),
        styles: ShapeStyles {
            fill_styles: vec![fill],
            line_styles: vec![],
        },
        shape,
    }
}

/// EditText -> DefineEditText. The device font stands in for APT's fonts
/// (glyph outlines aren't converted), which is fine for UI text.
fn build_edit_text(id: CharacterId, t: &apt::Text) -> swf::EditText<'_> {
    let align = match t.alignment {
        apt::TextAlignment::Right => swf::TextAlign::Right,
        apt::TextAlignment::Center => swf::TextAlign::Center,
        apt::TextAlignment::Justify => swf::TextAlign::Justify,
        apt::TextAlignment::Left | apt::TextAlignment::None => swf::TextAlign::Left,
    };
    let mut et = swf::EditText::new()
        .with_id(id)
        .with_bounds(rect_to_swf(&t.bounds))
        .with_default_font()
        .with_color(Some(unpack_color(t.color)))
        .with_layout(Some(swf::TextLayout {
            align,
            left_margin: Twips::ZERO,
            right_margin: Twips::ZERO,
            indent: Twips::ZERO,
            leading: Twips::ZERO,
        }))
        .with_is_read_only(t.read_only)
        .with_is_multiline(t.multiline)
        .with_is_word_wrap(t.word_wrap)
        .with_is_selectable(false)
        .with_use_outlines(false);
    if !t.initial_text.is_empty() {
        et = et.with_initial_text(Some(SwfStr::from_utf8_str(&t.initial_text)));
    }
    if !t.variable.is_empty() {
        et = et.with_variable_name(SwfStr::from_utf8_str(&t.variable));
    }
    et
}

/// The button's hit area as a shape: its triangle mesh when present,
/// otherwise its bounding rect. (It is only ever shown as the invisible
/// hit-test state, so the fill color is irrelevant.)
fn build_hit_shape(id: CharacterId, b: &apt::Button) -> Shape {
    let mut records: Vec<ShapeRecord> = Vec::new();
    if b.hit_test_triangles.is_empty() {
        let r = &b.hit_test_bounds;
        let (l, t, rr, bb) = (r.left, r.top, r.right, r.bottom);
        records.push(ShapeRecord::StyleChange(Box::new(StyleChangeData {
            move_to: Some(px_point((l, t))),
            fill_style_0: None,
            fill_style_1: Some(1),
            line_style: None,
            new_styles: None,
        })));
        records.extend(edge_chain((l, t), (rr, t)));
        records.extend(edge_chain((rr, t), (rr, bb)));
        records.extend(edge_chain((rr, bb), (l, bb)));
        records.extend(edge_chain((l, bb), (l, t)));
    } else {
        for tri in &b.hit_test_triangles {
            let v = |i: i16| {
                b.hit_test_vertices
                    .get(i as usize)
                    .copied()
                    .unwrap_or((0.0, 0.0))
            };
            let (v0, v1, v2) = (v(tri[0]), v(tri[1]), v(tri[2]));
            records.push(ShapeRecord::StyleChange(Box::new(StyleChangeData {
                move_to: Some(px_point(v0)),
                fill_style_0: None,
                fill_style_1: Some(1),
                line_style: None,
                new_styles: None,
            })));
            records.extend(edge_chain(v0, v1));
            records.extend(edge_chain(v1, v2));
            records.extend(edge_chain(v2, v0));
        }
    }
    Shape {
        version: 3,
        id,
        shape_bounds: rect_to_swf(&b.hit_test_bounds),
        edge_bounds: rect_to_swf(&b.hit_test_bounds),
        flags: ShapeFlag::empty(),
        styles: ShapeStyles {
            fill_styles: vec![FillStyle::Color(Color {
                r: 0,
                g: 0,
                b: 0,
                a: 0,
            })],
            line_styles: vec![],
        },
        shape: records,
    }
}

/// Button -> DefineButton2. APT's state mask and condition mask share SWF's
/// bit layout, so they carry over directly — except the hit-test state, which
/// is redirected to the synthetic mesh shape (`hit_id`).
fn build_button<'a>(
    id: CharacterId,
    b: &apt::Button,
    hit_id: CharacterId,
    actions: &'a [Vec<u8>],
) -> swf::Button<'a> {
    let mut records: Vec<swf::ButtonRecord> = b
        .records
        .iter()
        .filter_map(|r| {
            // Strip the hit-test state: the record shape is a dummy there.
            let states =
                swf::ButtonState::from_bits_truncate(r.states as u8) - swf::ButtonState::HIT_TEST;
            if states.is_empty() {
                return None;
            }
            Some(swf::ButtonRecord {
                states,
                id: r.character_id as CharacterId,
                depth: r.layer as Depth,
                matrix: to_swf_matrix(r.matrix),
                color_transform: float_cxform_to_swf(&r.cxform),
                filters: vec![],
                blend_mode: swf::BlendMode::Normal,
            })
        })
        .collect();
    // The engine hit-tests the mesh through the hit-test record's matrix
    // (AptAnimationTarget::GetButton composes it with the instance matrix), and
    // menus depend on it: a record can scale a small mesh into a large "keep
    // open" region layered over a close-area button.
    let hit_matrix = b
        .records
        .iter()
        .find(|r| r.states & 0x8 != 0)
        .map(|r| to_swf_matrix(r.matrix))
        .unwrap_or(Matrix::IDENTITY);
    records.push(swf::ButtonRecord {
        states: swf::ButtonState::HIT_TEST,
        id: hit_id,
        depth: 1,
        matrix: hit_matrix,
        color_transform: ColorTransform::IDENTITY,
        filters: vec![],
        blend_mode: swf::BlendMode::Normal,
    });
    let records = records;
    let actions = b
        .actions
        .iter()
        .zip(actions)
        .map(|(act, data)| swf::ButtonAction {
            conditions: swf::ButtonActionCondition::from_bits_truncate(act.conditions as u16),
            action_data: data,
        })
        .collect();
    swf::Button {
        id,
        is_track_as_menu: b.is_menu,
        records,
        actions,
    }
}

fn float_cxform_to_swf(c: &apt::FloatCxForm) -> ColorTransform {
    // Scale is a 0..1 multiplier per ARGB channel; translate is additive 0..255.
    ColorTransform {
        a_multiply: Fixed8::from_f32(c.scale[0]),
        r_multiply: Fixed8::from_f32(c.scale[1]),
        g_multiply: Fixed8::from_f32(c.scale[2]),
        b_multiply: Fixed8::from_f32(c.scale[3]),
        a_add: c.translate[0] as i16,
        r_add: c.translate[1] as i16,
        g_add: c.translate[2] as i16,
        b_add: c.translate[3] as i16,
    }
}

fn edge(from: (f32, f32), to: (f32, f32)) -> ShapeRecord {
    ShapeRecord::StraightEdge {
        delta: PointDelta {
            dx: Twips::from_pixels((to.0 - from.0) as f64),
            dy: Twips::from_pixels((to.1 - from.1) as f64),
        },
    }
}

/// A straight-edge record's delta is bit-packed with a 4-bit `NumBits-2`
/// field (see the SWF spec / `swf` crate's shape writer), capping any single
/// edge at 65535 twips (~3276.75px) per axis. Large shapes — a placeholder
/// quad the size of a big texture atlas, say — need the line broken into a
/// chain of shorter edges to stay encodable at all.
const MAX_EDGE_PX: f32 = 65535.0 / 20.0;

fn edge_chain(from: (f32, f32), to: (f32, f32)) -> Vec<ShapeRecord> {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let steps = (dx.abs().max(dy.abs()) / MAX_EDGE_PX).ceil().max(1.0) as usize;
    let mut records = Vec::with_capacity(steps);
    let mut prev = from;
    for i in 1..=steps {
        let t = i as f32 / steps as f32;
        let next = (from.0 + dx * t, from.1 + dy * t);
        records.push(edge(prev, next));
        prev = next;
    }
    records
}

fn px_point(v: (f32, f32)) -> Point<Twips> {
    Point::new(
        Twips::from_pixels(v.0 as f64),
        Twips::from_pixels(v.1 as f64),
    )
}

fn rect_to_swf(r: &apt::Rect) -> Rectangle<Twips> {
    Rectangle {
        x_min: Twips::from_pixels(r.left as f64),
        x_max: Twips::from_pixels(r.right as f64),
        y_min: Twips::from_pixels(r.top as f64),
        y_max: Twips::from_pixels(r.bottom as f64),
    }
}

fn to_swf_matrix(m: apt::Matrix) -> Matrix {
    Matrix {
        a: Fixed16::from_f32(m.a),
        b: Fixed16::from_f32(m.b),
        c: Fixed16::from_f32(m.c),
        d: Fixed16::from_f32(m.d),
        tx: Twips::from_pixels(m.tx as f64),
        ty: Twips::from_pixels(m.ty as f64),
    }
}

fn to_swf_cxform(c: &apt::PackedCxForm) -> ColorTransform {
    let [sb, sg, sr, sa] = c.scale.to_le_bytes();
    let [bb, bg, br, ba] = c.bias.to_le_bytes();
    ColorTransform {
        r_multiply: Fixed8::from_f64(sr as f64 / 255.0),
        g_multiply: Fixed8::from_f64(sg as f64 / 255.0),
        b_multiply: Fixed8::from_f64(sb as f64 / 255.0),
        a_multiply: Fixed8::from_f64(sa as f64 / 255.0),
        r_add: br as i16,
        g_add: bg as i16,
        b_add: bb as i16,
        a_add: ba as i16,
    }
}

fn unpack_color(c: u32) -> Color {
    let [b, g, r, a] = c.to_le_bytes();
    Color { r, g, b, a }
}
