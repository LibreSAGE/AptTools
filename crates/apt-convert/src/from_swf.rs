//! Parse a standard SWF into the APT model.
//!
//! This recovers the timeline structure (frames, PlaceObject/RemoveObject,
//! DoAction/InitAction), sprites, and shape bounds. Shape *geometry* is emitted
//! separately as aux data (see [`extract_geometry`]) since APT keeps geometry
//! outside the `.apt` blob.

use std::collections::BTreeMap;

use apt::{
    AptFile, Character, CharacterSlot, Control, Export, Frame, Header, Morph, Movie, PackedCxForm,
    PlaceObject, PtrSize, Rect, Shape as AptShape, Sprite as AptSprite,
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
            Tag::ExportAssets(assets) => {
                for a in assets {
                    exports.push(Export {
                        name: a.name.to_string_lossy(encoding),
                        character_id: a.id as u32,
                    });
                }
            }
            Tag::SetBackgroundColor(color) => {
                pending.controls.push(Control::BackgroundColor(pack_color(
                    color.r, color.g, color.b, color.a,
                )));
            }
            Tag::PlaceObject(p) => pending
                .controls
                .push(Control::PlaceObject(convert_place(p, version))),
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
        imports: vec![],
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

fn convert_sprite(sprite: &swf::Sprite, version: u8) -> Result<AptSprite> {
    let mut frames: Vec<Frame> = Vec::new();
    let mut pending = Frame::default();
    for tag in &sprite.tags {
        match tag {
            Tag::ShowFrame => frames.push(std::mem::take(&mut pending)),
            Tag::PlaceObject(p) => pending
                .controls
                .push(Control::PlaceObject(convert_place(p, version))),
            Tag::RemoveObject(r) => pending.controls.push(Control::RemoveObject {
                depth: r.depth as i32,
            }),
            Tag::DoAction(data) => pending
                .controls
                .push(Control::Action(swf_to_apt_actions(data)?)),
            _ => {}
        }
    }
    if !pending.controls.is_empty() {
        frames.push(pending);
    }
    Ok(AptSprite { frames })
}

fn convert_place(p: &swf::PlaceObject, _version: u8) -> PlaceObject {
    use swf::PlaceObjectAction;
    let (is_move, character_id) = match p.action {
        PlaceObjectAction::Place(id) => (false, Some(id as i32)),
        PlaceObjectAction::Modify => (true, None),
        PlaceObjectAction::Replace(id) => (true, Some(id as i32)),
    };
    PlaceObject {
        is_move,
        depth: p.depth as i32,
        character_id,
        matrix: p.matrix.map(matrix_from_swf),
        cxform: p.color_transform.as_ref().map(cxform_from_swf),
        ratio: p.ratio.map(|r| r as f32 / 65535.0),
        name: p
            .name
            .map(|s| s.to_string_lossy(swf::SwfStr::encoding_for_version(_version))),
        clip_depth: p.clip_depth.map(|d| d as i32),
        clip_actions: None,
        blend_mode: None,
        filters: vec![],
    }
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
    /// Every bitmap fill referenced by the shapes above, packed into one
    /// shared atlas (`None` if the SWF has no bitmap fills).
    pub atlas: Option<apt_aux::Texture>,
    /// Bitmap character ID -> texture ID for the atlas above. Every entry
    /// maps to the same one shared atlas texture ID ([`ATLAS_TEXTURE_ID`]).
    pub texture_map: apt_aux::dat::TextureMap,
}

/// The texture ID every bitmap character remaps to in
/// [`ExtractedAssets::texture_map`], since every bitmap fill is packed into
/// the single shared atlas.
pub const ATLAS_TEXTURE_ID: u32 = 1;

/// Maximum atlas row width before wrapping to a new shelf; matches the
/// largest packed atlases seen in the original corpus (see
/// `docs/apt-testfiles.md` §6).
const ATLAS_MAX_WIDTH: u32 = 2048;

/// Extract shape geometry and bitmap-fill textures from a SWF for the aux
/// `.ru`/atlas/`.dat` files (APT keeps all of these outside the `.apt` blob).
pub fn extract_geometry(swf_data: &[u8]) -> Result<ExtractedAssets> {
    let buf = swf::decompress_swf(swf_data).map_err(|e| Error::SwfRead(e.to_string()))?;
    let swf = swf::parse_swf(&buf).map_err(|e| Error::SwfRead(e.to_string()))?;

    let mut geometry: BTreeMap<u32, apt_aux::ShapeGeometry> = BTreeMap::new();
    let mut bitmaps: BTreeMap<u32, apt_aux::Texture> = BTreeMap::new();
    collect_shapes_and_bitmaps(&swf.tags, &mut geometry, &mut bitmaps);

    let mut texture_map = apt_aux::dat::TextureMap::default();
    let atlas = if bitmaps.is_empty() {
        None
    } else {
        let ids: Vec<u32> = bitmaps.keys().copied().collect();
        let textures: Vec<apt_aux::Texture> = ids.iter().map(|id| bitmaps[id].clone()).collect();
        let (atlas, rects) = apt_aux::pack_textures(&textures, ATLAS_MAX_WIDTH);
        let rect_by_id: BTreeMap<u32, apt_aux::PackedRect> = ids.into_iter().zip(rects).collect();

        for geom in geometry.values_mut() {
            for unit in &mut geom.units {
                if let apt_aux::Style::Textured { bitmap_character_id, matrix, .. } = &mut unit.style {
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
        Some(atlas)
    };

    Ok(ExtractedAssets { geometry, atlas, texture_map })
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
                geometry.insert(shape.id as u32, crate::geometry::extract_shape_geometry(shape));
            }
            Tag::DefineSprite(sprite) => collect_shapes_and_bitmaps(&sprite.tags, geometry, bitmaps),
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
