//! Parse a standard SWF into the APT model.
//!
//! This recovers the timeline structure (frames, PlaceObject/RemoveObject,
//! DoAction/InitAction, frame labels), per-instance clip event handlers, filters
//! and blend modes, sprites, shape bounds, bitmap characters, and cross-movie
//! imports. Shape *geometry* is emitted separately as aux data (see
//! [`extract_geometry`]) since APT keeps geometry outside the `.apt` blob.

use std::collections::BTreeMap;

use apt::{
    AptFile, Character, CharacterSlot, Control, EventAction, Export, Filter, Frame, Header, Import,
    Morph, Movie, PackedCxForm, PlaceObject, PtrSize, Rect, Shape as AptShape, Sprite as AptSprite,
};
use swf::Tag;

use crate::bytecode::swf_to_apt_actions;
use crate::{Error, Result};

/// Options controlling the generated APT.
#[derive(Debug, Clone, Copy)]
pub struct SwfToAptOptions {
    pub ptr_size: PtrSize,
    pub decoupled: bool,
}

impl Default for SwfToAptOptions {
    fn default() -> Self {
        SwfToAptOptions {
            ptr_size: PtrSize::Four,
            decoupled: false,
        }
    }
}

/// Parse SWF bytes into an APT movie.
pub fn swf_to_apt(swf_data: &[u8], options: SwfToAptOptions) -> Result<AptFile> {
    let buf = swf::decompress_swf(swf_data).map_err(|e| Error::SwfRead(e.to_string()))?;
    let swf = swf::parse_swf(&buf).map_err(|e| Error::SwfRead(e.to_string()))?;
    let header = &swf.header;
    let version = header.version();
    let encoding = swf::SwfStr::encoding_for_version(version);

    // Collect definitions by character ID.
    let mut characters: BTreeMap<u16, Character> = BTreeMap::new();
    let mut exports: Vec<Export> = Vec::new();
    let mut imports: Vec<Import> = Vec::new();
    let mut import_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut main_frames: Vec<Frame> = Vec::new();
    let mut max_id = 0u16;
    let mut pending = Frame::default();

    for tag in &swf.tags {
        match tag {
            Tag::ShowFrame => main_frames.push(std::mem::take(&mut pending)),
            Tag::DefineSprite(sprite) => {
                max_id = max_id.max(sprite.id);
                characters.insert(
                    sprite.id,
                    Character::Sprite(convert_sprite(sprite, version)?),
                );
            }
            Tag::DefineShape(shape) => {
                max_id = max_id.max(shape.id);
                characters.insert(
                    shape.id,
                    Character::Shape(AptShape {
                        bounds: rect_from_swf(&shape.shape_bounds),
                        bitmap_character_id: None,
                    }),
                );
            }
            Tag::DefineMorphShape(m) => {
                max_id = max_id.max(m.id);
                characters.insert(
                    m.id,
                    Character::Morph(Morph {
                        start_character_id: 0,
                        end_character_id: 0,
                    }),
                );
            }
            // Every bitmap fill needs a Bitmap character in the table: the
            // engine walks the characters and, for each Bitmap, calls
            // pfnLoadTexture(id) — that is the ONLY thing that loads a texture.
            // Without it the atlas is never bound and textured shapes render
            // blank (the pixels live in the aux atlas, not the `.apt`).
            Tag::DefineBitsLossless(l) => {
                max_id = max_id.max(l.id);
                characters.insert(l.id, Character::Bitmap);
            }
            Tag::DefineBitsJpeg2 { id, .. } => {
                max_id = max_id.max(*id);
                characters.insert(*id, Character::Bitmap);
            }
            Tag::DefineBitsJpeg3(j) => {
                max_id = max_id.max(j.id);
                characters.insert(j.id, Character::Bitmap);
            }
            Tag::DefineBits { id, .. } => {
                max_id = max_id.max(*id);
                characters.insert(*id, Character::Bitmap);
            }
            Tag::ExportAssets(assets) => {
                for a in assets {
                    exports.push(Export {
                        name: a.name.to_string_lossy(encoding),
                        character_id: a.id as u32,
                    });
                }
            }
            // Cross-movie imports: each occupies a (left-empty) character slot
            // the engine resolves at link time from the named sibling movie.
            Tag::ImportAssets {
                url,
                imports: assets,
            } => {
                let url = url.to_string_lossy(encoding);
                let movie = url
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or(&url)
                    .trim_end_matches(".swf")
                    .to_string();
                for a in assets {
                    max_id = max_id.max(a.id);
                    import_ids.insert(a.id as u32);
                    imports.push(Import {
                        movie: movie.clone(),
                        name: a.name.to_string_lossy(encoding),
                        character_id: a.id as u32,
                    });
                }
            }
            Tag::FrameLabel(fl) => pending
                .controls
                .push(Control::FrameLabel(fl.label.to_string_lossy(encoding))),
            Tag::SetBackgroundColor(color) => {
                pending.controls.push(Control::BackgroundColor(pack_color(
                    color.r, color.g, color.b, color.a,
                )));
            }
            Tag::PlaceObject(p) => pending
                .controls
                .push(Control::PlaceObject(convert_place(p, version)?)),
            Tag::RemoveObject(r) => pending.controls.push(Control::RemoveObject {
                depth: r.depth as i32,
            }),
            Tag::DoAction(data) => pending
                .controls
                .push(Control::Action(swf_to_apt_actions(data)?)),
            Tag::DoInitAction { id, action_data } => pending.controls.push(Control::InitAction {
                sprite_id: *id as i32,
                actions: swf_to_apt_actions(action_data)?,
            }),
            Tag::End => break,
            _ => {}
        }
    }
    if !pending.controls.is_empty() {
        main_frames.push(pending);
    }

    // Build the character slot table [0..=max_id]; slot 0 is the root.
    let n = max_id as usize + 1;
    let mut slots = Vec::with_capacity(n);
    for i in 0..n {
        if i == 0 {
            slots.push(CharacterSlot::Root);
        } else if let Some(c) = characters.remove(&(i as u16)) {
            slots.push(CharacterSlot::Character(c));
        } else {
            slots.push(CharacterSlot::Empty);
        }
    }

    // Any PlaceObject that targets an empty slot (a character type we don't yet
    // convert — DefineEditText/Button/StaticText/Font, or a dropped import)
    // would dangle: the engine indexes `apCharacters[id]`, gets NULL, and
    // segfaults dereferencing it. Fill those referenced-but-empty slots with a
    // harmless empty sprite so the instance still exists (preserving its name
    // and depth for ActionScript) but renders nothing. Unreferenced empties
    // (bitmaps, fonts) are left alone — nothing places them.
    let mut placed_empty: Vec<u32> = Vec::new();
    collect_placed_ids(&main_frames, &slots, &mut placed_empty);
    for slot in &slots {
        if let CharacterSlot::Character(Character::Sprite(sprite)) = slot {
            collect_placed_ids(&sprite.frames, &slots, &mut placed_empty);
        }
    }
    placed_empty.sort_unstable();
    placed_empty.dedup();
    // Imported characters are meant to stay empty (the engine fills them at
    // link time); never overwrite them with a placeholder.
    placed_empty.retain(|id| !import_ids.contains(id));
    for &id in &placed_empty {
        // One empty frame, not zero: the engine ticks a placed movie to a frame
        // and indexes `aFrames[nFrame]` without guarding `nFrames == 0`, so a
        // frameless sprite crashes in AptMovie::doFrameControls. A single
        // control-less frame is the smallest structure it can safely play.
        slots[id as usize] = CharacterSlot::Character(Character::Sprite(AptSprite {
            frames: vec![Frame::default()],
        }));
    }
    if !placed_empty.is_empty() {
        log::warn!(
            "substituted empty-sprite placeholders for {} placed but unconverted character(s): {:?} \
             (text/button/font characters are not yet converted; instances render blank)",
            placed_empty.len(),
            placed_empty
        );
    }

    let (width, height) = stage_pixels(header);
    let ms_per_frame = {
        let fps = header.frame_rate().to_f64();
        if fps > 0.0 {
            (1000.0 / fps).round() as u32
        } else {
            33
        }
    };

    let movie = Movie {
        frames: main_frames,
        characters: slots,
        width,
        height,
        ms_per_frame,
        imports,
        exports,
    };

    let apt_header = Header {
        decoupled: options.decoupled,
        swf_version: header.version(),
        ptr_size: options.ptr_size,
        raw_tag: synth_tag(options, header.version()),
    };

    Ok(AptFile {
        header: apt_header,
        const_magic: *apt::constfile::CONST_MAGIC,
        movie,
    })
}

/// Record every character ID a frame's PlaceObjects reference that lands on an
/// [`CharacterSlot::Empty`] (an unconverted or missing definition), so those
/// dangling references can be back-filled with a placeholder.
fn collect_placed_ids(frames: &[Frame], slots: &[CharacterSlot], out: &mut Vec<u32>) {
    for frame in frames {
        for control in &frame.controls {
            if let Control::PlaceObject(p) = control {
                if let Some(id) = p.character_id {
                    if id >= 0 && matches!(slots.get(id as usize), Some(CharacterSlot::Empty)) {
                        out.push(id as u32);
                    }
                }
            }
        }
    }
}

fn convert_sprite(sprite: &swf::Sprite, version: u8) -> Result<AptSprite> {
    let mut frames: Vec<Frame> = Vec::new();
    let mut pending = Frame::default();
    for tag in &sprite.tags {
        match tag {
            Tag::ShowFrame => frames.push(std::mem::take(&mut pending)),
            Tag::PlaceObject(p) => pending
                .controls
                .push(Control::PlaceObject(convert_place(p, version)?)),
            Tag::RemoveObject(r) => pending.controls.push(Control::RemoveObject {
                depth: r.depth as i32,
            }),
            Tag::DoAction(data) => pending
                .controls
                .push(Control::Action(swf_to_apt_actions(data)?)),
            Tag::DoInitAction { id, action_data } => pending.controls.push(Control::InitAction {
                sprite_id: *id as i32,
                actions: swf_to_apt_actions(action_data)?,
            }),
            Tag::FrameLabel(fl) => pending.controls.push(Control::FrameLabel(
                fl.label
                    .to_string_lossy(swf::SwfStr::encoding_for_version(version)),
            )),
            _ => {}
        }
    }
    if !pending.controls.is_empty() {
        frames.push(pending);
    }
    Ok(AptSprite { frames })
}

fn convert_place(p: &swf::PlaceObject, version: u8) -> Result<PlaceObject> {
    use swf::PlaceObjectAction;
    let (is_move, character_id) = match p.action {
        PlaceObjectAction::Place(id) => (false, Some(id as i32)),
        PlaceObjectAction::Modify => (true, None),
        PlaceObjectAction::Replace(id) => (true, Some(id as i32)),
    };
    // `onClipEvent`/`on` handlers attached to this instance: the APT trigger
    // mask shares SWF's ClipEventFlag bit layout, so it copies straight across.
    let clip_actions = match &p.clip_actions {
        Some(acts) => Some(
            acts.iter()
                .map(|a| {
                    Ok(EventAction {
                        triggers: a.events.bits() as i32,
                        key_code: a.key_code.map(|k| k as i32).unwrap_or(0),
                        actions: swf_to_apt_actions(a.action_data)?,
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        ),
        None => None,
    };
    Ok(PlaceObject {
        is_move,
        depth: p.depth as i32,
        character_id,
        matrix: p.matrix.map(matrix_from_swf),
        cxform: p.color_transform.as_ref().map(cxform_from_swf),
        ratio: p.ratio.map(|r| r as f32 / 65535.0),
        name: p
            .name
            .map(|s| s.to_string_lossy(swf::SwfStr::encoding_for_version(version))),
        clip_depth: p.clip_depth.map(|d| d as i32),
        clip_actions,
        blend_mode: p.blend_mode.map(|b| b as u8 as i32),
        filters: p
            .filters
            .as_ref()
            .map(|fs| fs.iter().filter_map(swf_filter_to_apt).collect())
            .unwrap_or_default(),
    })
}

/// Convert a SWF filter to APT's equivalent (inverse of `to_swf`'s mapping):
/// APT stores the raw Flash filter fields, so `Fixed16`/`Fixed8` values map back
/// by their bit pattern. Returns `None` for the convolution filter, which APT
/// has no representation for.
fn swf_filter_to_apt(f: &swf::Filter) -> Option<Filter> {
    use swf::Filter as S;
    let col = |c: &swf::Color| pack_color(c.r, c.g, c.b, c.a);
    let grad = |g: &swf::GradientFilter, is_bevel: bool| Filter::GradientGlow {
        is_bevel,
        colors: g.colors.iter().map(|r| col(&r.color)).collect(),
        ratios: g.colors.iter().map(|r| r.ratio).collect(),
        blur_x: g.blur_x.get() as u32,
        blur_y: g.blur_y.get() as u32,
        angle: g.angle.get() as u32,
        distance: g.distance.get() as u32,
        strength: g.strength.get() as u16,
        flags: g.flags.bits() as u16,
    };
    Some(match f {
        S::DropShadowFilter(d) => Filter::DropShadow {
            color: col(&d.color),
            blur_x: d.blur_x.get() as u32,
            blur_y: d.blur_y.get() as u32,
            angle: d.angle.get() as u32,
            distance: d.distance.get() as u32,
            strength: d.strength.get() as u16,
            flags: d.flags.bits() as u16,
        },
        S::BlurFilter(b) => Filter::Blur {
            blur_x: b.blur_x.get() as u32,
            blur_y: b.blur_y.get() as u32,
            flags: b.flags.bits() as u16,
        },
        S::GlowFilter(g) => Filter::Glow {
            color: col(&g.color),
            blur_x: g.blur_x.get() as u32,
            blur_y: g.blur_y.get() as u32,
            strength: g.strength.get() as u16,
            flags: g.flags.bits() as u16,
        },
        S::BevelFilter(b) => Filter::Bevel {
            highlight_color: col(&b.highlight_color),
            shadow_color: col(&b.shadow_color),
            blur_x: b.blur_x.get() as u32,
            blur_y: b.blur_y.get() as u32,
            angle: b.angle.get() as u32,
            distance: b.distance.get() as u32,
            strength: b.strength.get() as u16,
            flags: b.flags.bits() as u16,
        },
        S::GradientGlowFilter(g) => grad(g, false),
        S::GradientBevelFilter(g) => grad(g, true),
        S::ColorMatrixFilter(c) => Filter::ColorMatrix { values: c.matrix },
        S::ConvolutionFilter(_) => {
            log::warn!("dropping unsupported ConvolutionFilter (no APT equivalent)");
            return None;
        }
    })
}

fn matrix_from_swf(m: swf::Matrix) -> apt::Matrix {
    apt::Matrix {
        a: m.a.to_f32(),
        b: m.b.to_f32(),
        c: m.c.to_f32(),
        d: m.d.to_f32(),
        tx: m.tx.to_pixels() as f32,
        ty: m.ty.to_pixels() as f32,
    }
}

fn cxform_from_swf(c: &swf::ColorTransform) -> PackedCxForm {
    let scale = u32::from_le_bytes([
        (c.b_multiply.to_f64() * 255.0) as u8,
        (c.g_multiply.to_f64() * 255.0) as u8,
        (c.r_multiply.to_f64() * 255.0) as u8,
        (c.a_multiply.to_f64() * 255.0) as u8,
    ]);
    let bias = u32::from_le_bytes([c.b_add as u8, c.g_add as u8, c.r_add as u8, c.a_add as u8]);
    PackedCxForm { scale, bias }
}

fn rect_from_swf(r: &swf::Rectangle<swf::Twips>) -> Rect {
    Rect {
        left: r.x_min.to_pixels() as f32,
        top: r.y_min.to_pixels() as f32,
        right: r.x_max.to_pixels() as f32,
        bottom: r.y_max.to_pixels() as f32,
    }
}

fn stage_pixels(h: &swf::HeaderExt) -> (u32, u32) {
    let s = h.stage_size();
    (
        (s.x_max - s.x_min).to_pixels().round() as u32,
        (s.y_max - s.y_min).to_pixels().round() as u32,
    )
}

fn pack_color(r: u8, g: u8, b: u8, a: u8) -> u32 {
    u32::from_le_bytes([b, g, r, a])
}

fn synth_tag(options: SwfToAptOptions, version: u8) -> [u8; 16] {
    let mut tag = format!(
        "Apt Data:{}:{}:{}",
        if options.decoupled { '1' } else { '0' },
        version % 10,
        options.ptr_size.digit()
    )
    .into_bytes();
    tag.resize(16, 0);
    tag.try_into().unwrap()
}

/// Geometry and texture assets recovered from a SWF, for writing alongside
/// the generated `.apt`/`.const` via [`apt_aux::ru::RuFormat`] and
/// [`apt_aux::dat::TextureMap`].
pub struct ExtractedAssets {
    /// Shape character ID -> triangulated geometry.
    pub geometry: BTreeMap<u32, apt_aux::ShapeGeometry>,
    /// Texture ID -> image, ready to write as `apt_<movie>_<id>.<ext>`. When
    /// packed, this is a single entry keyed by [`ATLAS_TEXTURE_ID`]; unpacked,
    /// one entry per bitmap character keyed by its own character ID.
    pub textures: BTreeMap<u32, apt_aux::Texture>,
    /// Bitmap character ID -> texture ID (`.dat`).
    pub texture_map: apt_aux::dat::TextureMap,
}

/// The single texture ID every bitmap character maps to when packing into one
/// shared atlas.
pub const ATLAS_TEXTURE_ID: u32 = 1;

/// Maximum atlas side (square, power-of-two) before it's allowed to grow past;
/// matches the largest packed atlases seen in the original corpus (see
/// `docs/apt-testfiles.md` §6).
const ATLAS_MAX_SIDE: u32 = 2048;

/// Extract shape geometry and bitmap-fill textures from a SWF for the aux
/// `.ru`/texture/`.dat` files (APT keeps all of these outside the `.apt` blob).
///
/// When `pack` is set, every bitmap fill is packed into one shared square atlas
/// (texture ID [`ATLAS_TEXTURE_ID`]) and shape UV matrices are offset to the
/// atlas; otherwise each bitmap becomes its own standalone texture (keyed by
/// its character ID) and matrices are left addressing the original image.
pub fn extract_geometry(swf_data: &[u8], pack: bool) -> Result<ExtractedAssets> {
    let buf = swf::decompress_swf(swf_data).map_err(|e| Error::SwfRead(e.to_string()))?;
    let swf = swf::parse_swf(&buf).map_err(|e| Error::SwfRead(e.to_string()))?;

    let mut geometry: BTreeMap<u32, apt_aux::ShapeGeometry> = BTreeMap::new();
    let mut bitmaps: BTreeMap<u32, apt_aux::Texture> = BTreeMap::new();
    collect_shapes_and_bitmaps(&swf.tags, &mut geometry, &mut bitmaps);

    let mut texture_map = apt_aux::dat::TextureMap::default();
    let mut textures: BTreeMap<u32, apt_aux::Texture> = BTreeMap::new();

    // Try to pack into one square power-of-two atlas; if the fills don't fit a
    // `ATLAS_MAX_SIDE`-square page, fall back to standalone per-bitmap textures.
    let ids: Vec<u32> = bitmaps.keys().copied().collect();
    let imgs: Vec<apt_aux::Texture> = ids.iter().map(|id| bitmaps[id].clone()).collect();
    let packed = if pack {
        apt_aux::pack_textures(&imgs, ATLAS_MAX_SIDE)
    } else {
        None
    };

    match packed {
        Some((atlas, rects)) => {
            let rect_by_id: BTreeMap<u32, apt_aux::PackedRect> =
                ids.iter().copied().zip(rects).collect();
            for geom in geometry.values_mut() {
                for unit in &mut geom.units {
                    if let apt_aux::Style::Textured {
                        bitmap_character_id,
                        matrix,
                        ..
                    } = &mut unit.style
                    {
                        if let Some(rect) = rect_by_id.get(bitmap_character_id) {
                            matrix[4] += rect.x as f32;
                            matrix[5] += rect.y as f32;
                        }
                    }
                }
            }
            for id in rect_by_id.keys() {
                texture_map.entries.insert(*id, ATLAS_TEXTURE_ID);
            }
            textures.insert(ATLAS_TEXTURE_ID, atlas);
        }
        None => {
            if pack && !bitmaps.is_empty() {
                log::warn!(
                    "bitmap fills don't fit a {ATLAS_MAX_SIDE}x{ATLAS_MAX_SIDE} atlas; \
                     exporting {} texture(s) unpacked",
                    bitmaps.len()
                );
            }
            // One texture per bitmap; the `.dat` maps each character to itself.
            for (id, tex) in bitmaps {
                texture_map.entries.insert(id, id);
                textures.insert(id, tex);
            }
        }
    }

    Ok(ExtractedAssets {
        geometry,
        textures,
        texture_map,
    })
}

/// Walk every tag (recursing into sprites, since some encoders emit
/// definition tags nested inside a sprite rather than at the top level)
/// collecting shape geometry and decoding bitmap character definitions.
fn collect_shapes_and_bitmaps(
    tags: &[Tag],
    geometry: &mut BTreeMap<u32, apt_aux::ShapeGeometry>,
    bitmaps: &mut BTreeMap<u32, apt_aux::Texture>,
) {
    for tag in tags {
        match tag {
            Tag::DefineShape(shape) => {
                geometry.insert(
                    shape.id as u32,
                    crate::geometry::extract_shape_geometry(shape),
                );
            }
            Tag::DefineSprite(sprite) => {
                collect_shapes_and_bitmaps(&sprite.tags, geometry, bitmaps)
            }
            Tag::DefineBitsJpeg2 { id, jpeg_data } => {
                if let Some(t) = crate::bitmaps::decode_jpeg(jpeg_data) {
                    bitmaps.insert(*id as u32, t);
                } else {
                    log::warn!("failed to decode DefineBitsJpeg2 character {id}");
                }
            }
            Tag::DefineBitsJpeg3(j) => {
                if let Some(t) = crate::bitmaps::decode_jpeg3(j) {
                    bitmaps.insert(j.id as u32, t);
                } else {
                    log::warn!("failed to decode DefineBitsJpeg3 character {}", j.id);
                }
            }
            Tag::DefineBitsLossless(l) => {
                if let Some(t) = crate::bitmaps::decode_lossless(l) {
                    bitmaps.insert(l.id as u32, t);
                } else {
                    log::warn!("failed to decode DefineBitsLossless character {}", l.id);
                }
            }
            Tag::DefineBits { id, .. } => {
                log::warn!("skipping legacy DefineBits character {id}: needs a shared JpegTables tag, unsupported");
            }
            _ => {}
        }
    }
}
