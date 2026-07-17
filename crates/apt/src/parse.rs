//! Parser for the `.apt` blob (with its companion `.const`).

use crate::actions::ActionStream;
use crate::constfile::ConstFile;
use crate::error::Error;
use crate::io::Cursor;
use crate::types::*;
use crate::{AptFile, Result};

/// Sniff the header tag `"Apt Data:<decoupled>:<swfver>:<ptrsize>"`.
/// Missing fields default like the engine: non-decoupled, SWF 6, 4-byte pointers.
pub fn parse_header(data: &[u8]) -> Result<Header> {
    if data.len() < 16 || &data[0..8] != b"Apt Data" {
        return Err(Error::BadAptTag);
    }
    let mut raw_tag = [0u8; 16];
    raw_tag.copy_from_slice(&data[0..16]);

    let decoupled = data[8] == b':' && data[9] == b'1';
    let swf_version = if data[10] == b':' {
        data[11].wrapping_sub(b'0')
    } else {
        6
    };
    let ptr_size = if data[12] == b':' {
        match data[13] {
            b'4' => PtrSize::Four,
            b'8' => PtrSize::Eight,
            c => return Err(Error::BadPtrSize(c as char)),
        }
    } else {
        PtrSize::Four
    };
    Ok(Header {
        decoupled,
        swf_version,
        ptr_size,
        raw_tag,
    })
}

pub fn parse(apt_data: &[u8], const_data: &[u8]) -> Result<AptFile> {
    let header = parse_header(apt_data)?;
    let const_file = ConstFile::read(const_data, header.ptr_size)?;
    let ctx = Parser {
        cur: Cursor::new(apt_data, header.ptr_size),
        consts: &const_file.constants,
        decoupled: header.decoupled,
    };
    let movie = ctx.parse_root(const_file.main_character_offset as usize)?;
    Ok(AptFile {
        header,
        const_magic: const_file.magic,
        movie,
    })
}

struct Parser<'a> {
    cur: Cursor<'a>,
    consts: &'a [Value],
    decoupled: bool,
}

/// Result of reading an `AptCharacter` prelude: cursor at the payload union,
/// plus the decoupled data word (0 in non-decoupled files).
struct CharHeader<'a> {
    c: Cursor<'a>,
    type_id: i32,
    decoupled_data: u32,
}

impl<'a> Parser<'a> {
    fn char_header(&self, offset: usize) -> Result<CharHeader<'a>> {
        let mut c = self.cur.at(offset);
        let type_id = c.i32()?;
        c.ptr()?; // pParentAnim: magic 0x09876543
        let mut decoupled_data = 0;
        if self.decoupled {
            decoupled_data = c.u32()?;
            c.ptr()?; // m_pAnimFile: dead in file
        }
        Ok(CharHeader {
            c,
            type_id,
            decoupled_data,
        })
    }

    fn parse_root(&self, offset: usize) -> Result<Movie> {
        let h = self.char_header(offset)?;
        if h.type_id != 9 {
            return Err(Error::RootNotAnimation(h.type_id));
        }
        let mut c = h.c;
        // AptCharacterAnimation
        let n_frames = c.i32()?;
        let frames_off = c.ptr()?;
        c.ptr()?; // phLabels: dead in file
        let n_characters = c.i32()?;
        let characters_off = c.ptr()?;
        let width = c.u32()?;
        let height = c.u32()?;
        let ms_per_frame = c.u32()?;
        let n_imports = c.i32()?;
        let imports_off = c.ptr()?;
        let n_exports = c.i32()?;
        let exports_off = c.ptr()?;

        let frames = self.parse_frames(frames_off as usize, n_frames)?;

        let mut characters = Vec::with_capacity(n_characters.max(0) as usize);
        let mut cc = self.cur.at(characters_off as usize);
        for i in 0..n_characters.max(0) as usize {
            let char_off = cc.ptr()?;
            let slot = if char_off == 0 {
                CharacterSlot::Empty
            } else if i == 0 || char_off as usize == offset {
                CharacterSlot::Root
            } else {
                CharacterSlot::Character(self.parse_character(char_off as usize, i as u32)?)
            };
            characters.push(slot);
        }

        let mut imports = Vec::with_capacity(n_imports.max(0) as usize);
        let mut ic = self.cur.at(imports_off as usize);
        for _ in 0..n_imports.max(0) {
            ic.align_ptr();
            let movie = ic.ptr_string()?;
            let name = ic.ptr_string()?;
            let character_id = ic.i32()? as u32;
            ic.ptr()?; // file: dead in file
            imports.push(Import {
                movie,
                name,
                character_id,
            });
        }

        let mut exports = Vec::with_capacity(n_exports.max(0) as usize);
        let mut ec = self.cur.at(exports_off as usize);
        for _ in 0..n_exports.max(0) {
            ec.align_ptr();
            let name = ec.ptr_string()?;
            let character_id = ec.i32()? as u32;
            exports.push(Export { name, character_id });
        }

        Ok(Movie {
            frames,
            characters,
            width,
            height,
            ms_per_frame,
            imports,
            exports,
        })
    }

    fn parse_character(&self, offset: usize, _index: u32) -> Result<Character> {
        let h = self.char_header(offset)?;
        let mut c = h.c;
        Ok(match h.type_id {
            1 => Character::Shape(Shape {
                bounds: rect(&mut c)?,
                bitmap_character_id: if self.decoupled {
                    Some((h.decoupled_data & 0x7FFF) as u16)
                } else {
                    None
                },
            }),
            2 => Character::Text(Text {
                bounds: rect(&mut c)?,
                font_id: c.i32()?,
                alignment: TextAlignment::from_i32(c.i32()?),
                color: c.u32()?,
                font_height: c.f32()?,
                read_only: c.i32()? != 0,
                multiline: c.i32()? != 0,
                word_wrap: c.i32()? != 0,
                initial_text: c.ptr_string()?,
                variable: c.ptr_string()?,
            }),
            3 => {
                let name = c.ptr_string()?;
                let n_glyphs = c.i32()?;
                let glyphs_off = c.ptr()?;
                let mut glyphs = Vec::with_capacity(n_glyphs.max(0) as usize);
                let mut gc = self.cur.at(glyphs_off as usize);
                for _ in 0..n_glyphs.max(0) {
                    glyphs.push(gc.ptr()? as u32);
                }
                Character::Font(Font { name, glyphs })
            }
            4 => self.parse_button(&mut c)?,
            5 => {
                let n_frames = c.i32()?;
                let frames_off = c.ptr()?;
                c.ptr()?; // phLabels
                Character::Sprite(Sprite {
                    frames: self.parse_frames(frames_off as usize, n_frames)?,
                })
            }
            6 => Character::Sound,
            7 => Character::Bitmap,
            8 => Character::Morph(Morph {
                start_character_id: c.ptr()? as u32,
                end_character_id: c.ptr()? as u32,
            }),
            10 => {
                let bounds = rect(&mut c)?;
                let matrix = matrix(&mut c)?;
                let n_records = c.i32()?;
                let records_off = c.ptr()?;
                let mut records = Vec::with_capacity(n_records.max(0) as usize);
                let mut rc = self.cur.at(records_off as usize);
                for _ in 0..n_records.max(0) {
                    rc.align_ptr();
                    let font_id = rc.i32()?;
                    let cxform = float_cxform(&mut rc)?;
                    let x_offset = rc.f32()?;
                    let y_offset = rc.f32()?;
                    let scale = rc.f32()?;
                    let n_glyphs = rc.i32()?;
                    let glyphs_off = rc.ptr()?;
                    let mut glyphs = Vec::with_capacity(n_glyphs.max(0) as usize);
                    let mut gc = self.cur.at(glyphs_off as usize);
                    for _ in 0..n_glyphs.max(0) {
                        glyphs.push(GlyphEntry {
                            index: gc.i16()?,
                            advance: gc.i16()?,
                        });
                    }
                    records.push(StaticTextRecord {
                        font_id,
                        cxform,
                        x_offset,
                        y_offset,
                        scale,
                        glyphs,
                    });
                }
                Character::StaticText(StaticText {
                    bounds,
                    matrix,
                    records,
                })
            }
            11 => Character::None,
            12 => Character::Video,
            t => return Err(Error::InvalidCharacterType(t)),
        })
    }

    fn parse_button(&self, c: &mut Cursor) -> Result<Character> {
        let is_menu = c.i32()? != 0;
        let hit_test_bounds = rect(c)?;
        let n_triangles = c.i32()?;
        let n_vertices = c.i32()?;
        let vertices_off = c.ptr()?;
        let indices_off = c.ptr()?;
        let n_records = c.i32()?;
        let records_off = c.ptr()?;
        let n_conditions = c.i32()?;
        let conditions_off = c.ptr()?;
        let sound_off = c.ptr()?;

        let mut hit_test_vertices = Vec::with_capacity(n_vertices.max(0) as usize);
        let mut vc = self.cur.at(vertices_off as usize);
        for _ in 0..n_vertices.max(0) {
            hit_test_vertices.push((vc.f32()?, vc.f32()?));
        }
        let mut hit_test_triangles = Vec::with_capacity(n_triangles.max(0) as usize);
        let mut tc = self.cur.at(indices_off as usize);
        for _ in 0..n_triangles.max(0) {
            hit_test_triangles.push([tc.i16()?, tc.i16()?, tc.i16()?]);
        }

        let mut records = Vec::with_capacity(n_records.max(0) as usize);
        let mut rc = self.cur.at(records_off as usize);
        for _ in 0..n_records.max(0) {
            rc.align_ptr();
            let states = rc.i32()?;
            let character_id = rc.ptr()? as u32;
            let layer = rc.i32()?;
            let mat = matrix(&mut rc)?;
            let cxform = float_cxform(&mut rc)?;
            records.push(ButtonRecord {
                states,
                character_id,
                layer,
                matrix: mat,
                cxform,
            });
        }

        let mut actions = Vec::with_capacity(n_conditions.max(0) as usize);
        let mut ac = self.cur.at(conditions_off as usize);
        for _ in 0..n_conditions.max(0) {
            ac.align_ptr();
            let conditions = ac.i32()?;
            let stream_off = ac.ptr()?;
            actions.push(ButtonAction {
                conditions,
                actions: self.stream(stream_off)?,
            });
        }

        let sounds = if sound_off != 0 {
            let mut sc = self.cur.at(sound_off as usize);
            Some(ButtonSounds {
                over_up_to_idle: sc.ptr()? as u32,
                idle_to_over_up: sc.ptr()? as u32,
                over_up_to_over_down: sc.ptr()? as u32,
                over_down_to_over_up: sc.ptr()? as u32,
            })
        } else {
            None
        };

        Ok(Character::Button(Button {
            is_menu,
            hit_test_bounds,
            hit_test_vertices,
            hit_test_triangles,
            records,
            actions,
            sounds,
        }))
    }

    fn parse_frames(&self, offset: usize, count: i32) -> Result<Vec<Frame>> {
        let mut frames = Vec::with_capacity(count.max(0) as usize);
        let mut fc = self.cur.at(offset);
        for _ in 0..count.max(0) {
            fc.align_ptr();
            let n_controls = fc.i32()?;
            let controls_off = fc.ptr()?;
            let mut controls = Vec::with_capacity(n_controls.max(0) as usize);
            let mut cc = self.cur.at(controls_off as usize);
            for _ in 0..n_controls.max(0) {
                let control_off = cc.ptr()?;
                controls.push(self.parse_control(control_off as usize)?);
            }
            frames.push(Frame { controls });
        }
        Ok(frames)
    }

    fn parse_control(&self, offset: usize) -> Result<Control> {
        let mut c = self.cur.at(offset);
        let type_id = c.i32()?;
        c.align_ptr();
        Ok(match type_id {
            1 => Control::Action(self.stream(c.ptr()?)?),
            2 => Control::FrameLabel(c.ptr_string()?),
            3 | 9 => {
                let flags = c.i32()?;
                let depth = c.i32()?;
                let character_id = c.i32()?;
                let mat = matrix(&mut c)?;
                let cx_scale = c.u32()?;
                let cx_bias = c.u32()?;
                let ratio = c.f32()?;
                let name_off = c.ptr()?;
                let clip_depth = c.i32()?;
                let actions_off = c.ptr()?;

                let clip_actions = if flags & 0x80 != 0 && actions_off != 0 {
                    let mut sc = self.cur.at(actions_off as usize);
                    let n = sc.i32()?;
                    let blocks_off = sc.ptr()?;
                    let mut blocks = Vec::with_capacity(n.max(0) as usize);
                    let mut bc = self.cur.at(blocks_off as usize);
                    for _ in 0..n.max(0) {
                        bc.align_ptr();
                        let triggers = bc.i32()?;
                        let key_code = bc.i32()?;
                        let stream_off = bc.ptr()?;
                        blocks.push(EventAction {
                            triggers,
                            key_code,
                            actions: self.stream(stream_off)?,
                        });
                    }
                    Some(blocks)
                } else {
                    None
                };

                let (blend_mode, filters) = if type_id == 9 {
                    let blend_mode = c.i32()?;
                    let n_filters = c.u32()?;
                    let filters_off = c.ptr()?;
                    let mut filters = Vec::with_capacity(n_filters as usize);
                    let mut flc = self.cur.at(filters_off as usize);
                    for _ in 0..n_filters {
                        let filter_off = flc.ptr()?;
                        filters.push(self.parse_filter(filter_off as usize)?);
                    }
                    (
                        if blend_mode == -1 {
                            None
                        } else {
                            Some(blend_mode)
                        },
                        filters,
                    )
                } else {
                    (None, Vec::new())
                };

                Control::PlaceObject(PlaceObject {
                    is_move: flags & 0x01 != 0,
                    depth,
                    character_id: if flags & 0x02 != 0 {
                        Some(character_id)
                    } else {
                        None
                    },
                    matrix: if flags & 0x04 != 0 { Some(mat) } else { None },
                    cxform: if flags & 0x08 != 0 {
                        Some(PackedCxForm {
                            scale: cx_scale,
                            bias: cx_bias,
                        })
                    } else {
                        None
                    },
                    ratio: if flags & 0x10 != 0 { Some(ratio) } else { None },
                    name: if flags & 0x20 != 0 && name_off != 0 {
                        Some(self.cur.string_at(name_off as usize)?)
                    } else {
                        None
                    },
                    clip_depth: if flags & 0x40 != 0 {
                        Some(clip_depth)
                    } else {
                        None
                    },
                    clip_actions,
                    blend_mode,
                    filters,
                })
            }
            4 => Control::RemoveObject { depth: c.i32()? },
            5 => Control::BackgroundColor(c.u32()?),
            6 => Control::StartSound { sound_id: c.i32()? },
            7 => Control::StartSoundStream { sound_id: c.i32()? },
            8 => {
                let sprite_id = c.i32()?;
                Control::InitAction {
                    sprite_id,
                    actions: self.stream(c.ptr()?)?,
                }
            }
            t => return Err(Error::InvalidControlType(t)),
        })
    }

    fn parse_filter(&self, offset: usize) -> Result<Filter> {
        let mut c = self.cur.at(offset);
        let id = c.u32()?;
        Ok(match id {
            0 => Filter::DropShadow {
                color: c.u32()?,
                blur_x: c.u32()?,
                blur_y: c.u32()?,
                angle: c.u32()?,
                distance: c.u32()?,
                strength: c.u16()?,
                flags: c.u16()?,
            },
            1 => Filter::Blur {
                blur_x: c.u32()?,
                blur_y: c.u32()?,
                flags: c.u16()?,
            },
            2 => Filter::Glow {
                color: c.u32()?,
                blur_x: c.u32()?,
                blur_y: c.u32()?,
                strength: c.u16()?,
                flags: c.u16()?,
            },
            3 => Filter::Bevel {
                highlight_color: c.u32()?,
                shadow_color: c.u32()?,
                blur_x: c.u32()?,
                blur_y: c.u32()?,
                angle: c.u32()?,
                distance: c.u32()?,
                strength: c.u16()?,
                flags: c.u16()?,
            },
            4 | 7 => {
                let n = c.u32()?;
                let colors_off = c.ptr()?;
                let ratios_off = c.ptr()?;
                let blur_x = c.u32()?;
                let blur_y = c.u32()?;
                let angle = c.u32()?;
                let distance = c.u32()?;
                let strength = c.u16()?;
                let flags = c.u16()?;
                let mut colors = Vec::with_capacity(n as usize);
                let mut colc = self.cur.at(colors_off as usize);
                for _ in 0..n {
                    colors.push(colc.u32()?);
                }
                let mut ratios = Vec::with_capacity(n as usize);
                let mut ratc = self.cur.at(ratios_off as usize);
                for _ in 0..n {
                    ratios.push(ratc.u8()?);
                }
                Filter::GradientGlow {
                    is_bevel: id == 7,
                    colors,
                    ratios,
                    blur_x,
                    blur_y,
                    angle,
                    distance,
                    strength,
                    flags,
                }
            }
            6 => {
                let mut values = [0f32; 20];
                for v in &mut values {
                    *v = c.f32()?;
                }
                Filter::ColorMatrix { values }
            }
            id => return Err(Error::InvalidFilterId(id)),
        })
    }

    fn stream(&self, offset: u64) -> Result<ActionStream> {
        if offset == 0 {
            return Ok(ActionStream::default());
        }
        crate::actions::decode_stream(&self.cur, offset as usize, self.consts)
    }
}

fn rect(c: &mut Cursor) -> Result<Rect> {
    Ok(Rect {
        left: c.f32()?,
        top: c.f32()?,
        right: c.f32()?,
        bottom: c.f32()?,
    })
}

fn matrix(c: &mut Cursor) -> Result<Matrix> {
    Ok(Matrix {
        a: c.f32()?,
        b: c.f32()?,
        c: c.f32()?,
        d: c.f32()?,
        tx: c.f32()?,
        ty: c.f32()?,
    })
}

fn float_cxform(c: &mut Cursor) -> Result<FloatCxForm> {
    let mut scale = [0f32; 4];
    let mut translate = [0f32; 4];
    for v in &mut scale {
        *v = c.f32()?;
    }
    for v in &mut translate {
        *v = c.f32()?;
    }
    Ok(FloatCxForm { scale, translate })
}
