//! Serializer for `.apt` + `.const` pairs.
//!
//! The writer walks the movie in the engine's canonical fixup order
//! (exports, then characters in index order — the root movie first — then
//! imports) so that Push-item constant indices come out globally sequential,
//! which the engine's `_parseStream` asserts.

use crate::actions::{encode_stream, ActionStream};
use crate::constfile::ConstFile;
use crate::error::Error;
use crate::io::{Arena, Deferred, Patch};
use crate::types::*;
use crate::{AptFile, Result};

#[derive(Debug, Clone, Copy)]
pub struct WriteOptions {
    pub ptr_size: PtrSize,
    pub decoupled: bool,
    pub swf_version: u8,
    /// When set, emit this exact 16-byte header tag instead of reconstructing
    /// one (used for byte-exact round-trips of files carrying the classic
    /// short tag `"Apt Data:6\x1a"`).
    pub raw_tag: Option<[u8; 16]>,
}

impl WriteOptions {
    /// Faithful re-emit: reuse the source file's exact header tag.
    pub fn from_header(h: &Header) -> WriteOptions {
        WriteOptions {
            ptr_size: h.ptr_size,
            decoupled: h.decoupled,
            swf_version: h.swf_version,
            raw_tag: Some(h.raw_tag),
        }
    }

    /// Fresh output for the given layout, synthesizing a long-form tag.
    pub fn new(ptr_size: PtrSize, decoupled: bool, swf_version: u8) -> WriteOptions {
        WriteOptions {
            ptr_size,
            decoupled,
            swf_version,
            raw_tag: None,
        }
    }
}

pub fn write(file: &AptFile, options: &WriteOptions) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut w = Writer {
        arena: Arena::new(options.ptr_size),
        consts: Vec::new(),
        deferred: Deferred::default(),
        decoupled: options.decoupled,
    };

    // 16-byte header tag; data never starts at offset 0, so 0 = NULL works.
    let tag = match options.raw_tag {
        Some(raw) => raw,
        None => {
            let mut tag = format!(
                "Apt Data:{}:{}:{}",
                if options.decoupled { '1' } else { '0' },
                options.swf_version % 10,
                options.ptr_size.digit()
            )
            .into_bytes();
            tag.resize(16, 0);
            tag.try_into().unwrap()
        }
    };
    w.arena.bytes(&tag);

    let root_offset = w.write_root(&file.movie)?;
    w.deferred.flush(&mut w.arena)?;

    let const_file = ConstFile {
        magic: file.const_magic,
        main_character_offset: root_offset,
        constants: w.consts,
    };
    let const_data = const_file.write(options.ptr_size)?;
    Ok((w.arena.buf, const_data))
}

struct Writer {
    arena: Arena,
    consts: Vec<Value>,
    deferred: Deferred,
    decoupled: bool,
}

impl Writer {
    /// Character record prelude; `decoupled_data` is the 4-byte data union
    /// (only a Shape's backing bitmap ID is file-meaningful).
    fn char_prelude(&mut self, type_id: i32, decoupled_data: u32) -> u64 {
        self.arena.align_ptr();
        let offset = self.arena.len() as u64;
        self.arena.i32(type_id);
        self.arena.ptr_value(PARENT_ANIM_MAGIC as u64);
        if self.decoupled {
            self.arena.u32(decoupled_data);
            self.arena.ptr_value(0); // m_pAnimFile: dead in file
        }
        offset
    }

    fn write_root(&mut self, movie: &Movie) -> Result<u64> {
        let root_offset = self.char_prelude(9, 0);
        let frames_patch;
        let characters_patch;
        let imports_patch;
        let exports_patch;
        {
            let a = &mut self.arena;
            a.i32(movie.frames.len() as i32);
            frames_patch = a.ptr_patch();
            a.ptr_value(0); // phLabels: dead in file
            a.i32(movie.characters.len() as i32);
            characters_patch = a.ptr_patch();
            a.u32(movie.width);
            a.u32(movie.height);
            a.u32(movie.ms_per_frame);
            a.i32(movie.imports.len() as i32);
            imports_patch = a.ptr_patch();
            a.i32(movie.exports.len() as i32);
            exports_patch = a.ptr_patch();
            a.ptr_value(0); // nCurrentConstantIndex: runtime state
        }

        // Fixup order: exports first (no action streams, but keep the shape),
        // then characters in index order — the root (index 0) movie first —
        // then imports.
        let exports_off = self.write_exports(&movie.exports)?;
        self.arena.patch_ptr(exports_patch, exports_off);

        let frames_off = self.write_frames(&movie.frames)?;
        self.arena.patch_ptr(frames_patch, frames_off);

        // Character slot array, patched as each character is written.
        self.arena.align_ptr();
        let characters_off = self.arena.len() as u64;
        let mut slot_patches: Vec<Patch> = Vec::with_capacity(movie.characters.len());
        for _ in &movie.characters {
            slot_patches.push(self.arena.ptr_patch());
        }
        self.arena.patch_ptr(characters_patch, characters_off);

        for (i, slot) in movie.characters.iter().enumerate() {
            let value = match slot {
                CharacterSlot::Root => root_offset,
                CharacterSlot::Empty => 0,
                CharacterSlot::Character(ch) => self.write_character(ch, i as u64)?,
            };
            self.arena.patch_ptr(slot_patches[i], value);
        }

        let imports_off = self.write_imports(&movie.imports)?;
        self.arena.patch_ptr(imports_patch, imports_off);

        Ok(root_offset)
    }

    fn write_exports(&mut self, exports: &[Export]) -> Result<u64> {
        self.arena.align_ptr();
        let off = self.arena.len() as u64;
        for e in exports {
            self.arena.align_ptr();
            self.deferred.string(&mut self.arena, &e.name);
            self.arena.i32(e.character_id as i32);
        }
        Ok(off)
    }

    fn write_imports(&mut self, imports: &[Import]) -> Result<u64> {
        self.arena.align_ptr();
        let off = self.arena.len() as u64;
        for i in imports {
            self.arena.align_ptr();
            self.deferred.string(&mut self.arena, &i.movie);
            self.deferred.string(&mut self.arena, &i.name);
            self.arena.i32(i.character_id as i32);
            self.arena.ptr_value(0); // file: dead in file
        }
        Ok(off)
    }

    fn write_character(&mut self, ch: &Character, index: u64) -> Result<u64> {
        let decoupled_data = match ch {
            Character::Shape(s) => (s.bitmap_character_id.unwrap_or(0) as u32) & 0x7FFF,
            _ => 0,
        };
        let offset = self.char_prelude(ch.type_id(), decoupled_data);
        match ch {
            Character::Shape(s) => {
                self.rect(&s.bounds);
                self.arena.ptr_value(index); // pRenderUnit: own character index
            }
            Character::Text(t) => {
                self.rect(&t.bounds);
                let a = &mut self.arena;
                a.i32(t.font_id);
                a.i32(t.alignment as i32);
                a.u32(t.color);
                a.f32(t.font_height);
                a.i32(t.read_only as i32);
                a.i32(t.multiline as i32);
                a.i32(t.word_wrap as i32);
                self.deferred.string(&mut self.arena, &t.initial_text);
                self.deferred.string(&mut self.arena, &t.variable);
            }
            Character::Font(f) => {
                self.deferred.string(&mut self.arena, &f.name);
                self.arena.i32(f.glyphs.len() as i32);
                let glyphs: Vec<u64> = f.glyphs.iter().map(|&g| g as u64).collect();
                if glyphs.is_empty() {
                    self.arena.ptr_value(0);
                } else {
                    self.deferred.ptr_array(&mut self.arena, glyphs);
                }
            }
            Character::Button(b) => self.write_button(b)?,
            Character::Sprite(s) => {
                let n = s.frames.len() as i32;
                self.arena.i32(n);
                let frames_patch = self.arena.ptr_patch();
                self.arena.ptr_value(0); // phLabels
                let frames_off = self.write_frames(&s.frames)?;
                self.arena.patch_ptr(frames_patch, frames_off);
            }
            Character::Sound | Character::Bitmap => {
                self.arena.ptr_value(index); // zID: own character index
            }
            Character::Morph(m) => {
                self.arena.ptr_value(m.start_character_id as u64);
                self.arena.ptr_value(m.end_character_id as u64);
            }
            Character::StaticText(st) => {
                self.rect(&st.bounds);
                self.matrix(&st.matrix);
                self.arena.i32(st.records.len() as i32);
                let records_patch = self.arena.ptr_patch();

                self.arena.align_ptr();
                let records_off = self.arena.len() as u64;
                let mut glyph_patches = Vec::new();
                for r in &st.records {
                    self.arena.align_ptr();
                    self.arena.i32(r.font_id);
                    self.float_cxform(&r.cxform);
                    let a = &mut self.arena;
                    a.f32(r.x_offset);
                    a.f32(r.y_offset);
                    a.f32(r.scale);
                    a.i32(r.glyphs.len() as i32);
                    glyph_patches.push(a.ptr_patch());
                }
                self.arena.patch_ptr(records_patch, records_off);
                for (patch, r) in glyph_patches.into_iter().zip(&st.records) {
                    self.arena.align(2);
                    let off = self.arena.len() as u64;
                    for g in &r.glyphs {
                        self.arena.i16(g.index);
                        self.arena.i16(g.advance);
                    }
                    self.arena.patch_ptr(patch, off);
                }
            }
            Character::None | Character::Video => {}
        }
        Ok(offset)
    }

    fn write_button(&mut self, b: &Button) -> Result<()> {
        self.arena.i32(b.is_menu as i32);
        self.rect(&b.hit_test_bounds);
        self.arena.i32(b.hit_test_triangles.len() as i32);
        self.arena.i32(b.hit_test_vertices.len() as i32);
        let vertices_patch = self.arena.ptr_patch();
        let indices_patch = self.arena.ptr_patch();
        self.arena.i32(b.records.len() as i32);
        let records_patch = self.arena.ptr_patch();
        self.arena.i32(b.actions.len() as i32);
        let conditions_patch = self.arena.ptr_patch();
        let sound_patch = self.arena.ptr_patch();

        self.arena.align(4);
        let vertices_off = self.arena.len() as u64;
        for &(x, y) in &b.hit_test_vertices {
            self.arena.f32(x);
            self.arena.f32(y);
        }
        self.arena.patch_ptr(vertices_patch, vertices_off);

        self.arena.align(2);
        let indices_off = self.arena.len() as u64;
        for tri in &b.hit_test_triangles {
            for &i in tri {
                self.arena.i16(i);
            }
        }
        self.arena.patch_ptr(indices_patch, indices_off);

        self.arena.align_ptr();
        let records_off = self.arena.len() as u64;
        for r in &b.records {
            self.arena.align_ptr();
            self.arena.i32(r.states);
            self.arena.ptr_value(r.character_id as u64);
            self.arena.i32(r.layer);
            self.matrix(&r.matrix);
            self.float_cxform(&r.cxform);
        }
        self.arena.patch_ptr(records_patch, records_off);

        // Condition blocks, then their streams in array order (const sequencing).
        self.arena.align_ptr();
        let conditions_off = self.arena.len() as u64;
        let mut stream_patches = Vec::new();
        for act in &b.actions {
            self.arena.align_ptr();
            self.arena.i32(act.conditions);
            stream_patches.push(self.arena.ptr_patch());
        }
        self.arena.patch_ptr(conditions_patch, conditions_off);
        for (patch, act) in stream_patches.into_iter().zip(&b.actions) {
            let off = self.stream(&act.actions)?;
            self.arena.patch_ptr(patch, off);
        }

        if let Some(s) = &b.sounds {
            self.arena.align_ptr();
            let off = self.arena.len() as u64;
            self.arena.ptr_value(s.over_up_to_idle as u64);
            self.arena.ptr_value(s.idle_to_over_up as u64);
            self.arena.ptr_value(s.over_up_to_over_down as u64);
            self.arena.ptr_value(s.over_down_to_over_up as u64);
            self.arena.patch_ptr(sound_patch, off);
        }
        Ok(())
    }

    fn write_frames(&mut self, frames: &[Frame]) -> Result<u64> {
        // A frameless movie (e.g. an empty placeholder sprite) must serialize a
        // NULL frame pointer, not an offset into whatever bytes follow: the
        // engine only null-checks the frame pointer before dereferencing
        // `aFrames->nControls`/`apControls`, so a non-null pointer into unrelated
        // data makes it walk garbage and crash (seen in AptCharacterAnimation::
        // ResetInitIndicators at shutdown).
        if frames.is_empty() {
            return Ok(0);
        }
        self.arena.align_ptr();
        let frames_off = self.arena.len() as u64;
        let mut control_array_patches = Vec::with_capacity(frames.len());
        for f in frames {
            self.arena.align_ptr();
            self.arena.i32(f.controls.len() as i32);
            control_array_patches.push(self.arena.ptr_patch());
        }
        for (patch, f) in control_array_patches.into_iter().zip(frames) {
            self.arena.align_ptr();
            let array_off = self.arena.len() as u64;
            let mut slots = Vec::with_capacity(f.controls.len());
            for _ in &f.controls {
                slots.push(self.arena.ptr_patch());
            }
            self.arena.patch_ptr(patch, array_off);
            for (slot, control) in slots.into_iter().zip(&f.controls) {
                let off = self.write_control(control)?;
                self.arena.patch_ptr(slot, off);
            }
        }
        Ok(frames_off)
    }

    fn write_control(&mut self, control: &Control) -> Result<u64> {
        self.arena.align_ptr();
        let off = self.arena.len() as u64;
        self.arena.i32(control.type_id());
        self.arena.align_ptr();
        match control {
            Control::Action(stream) => {
                let patch = self.arena.ptr_patch();
                let s = self.stream(stream)?;
                self.arena.patch_ptr(patch, s);
            }
            Control::FrameLabel(label) => {
                self.deferred.string(&mut self.arena, label);
            }
            Control::PlaceObject(p) => {
                let mut flags = 0i32;
                if p.is_move {
                    flags |= 0x01;
                }
                if p.character_id.is_some() {
                    flags |= 0x02;
                }
                if p.matrix.is_some() {
                    flags |= 0x04;
                }
                if p.cxform.is_some() {
                    flags |= 0x08;
                }
                if p.ratio.is_some() {
                    flags |= 0x10;
                }
                if p.name.is_some() {
                    flags |= 0x20;
                }
                if p.clip_depth.is_some() {
                    flags |= 0x40;
                }
                if p.clip_actions.is_some() {
                    flags |= 0x80;
                }
                let a = &mut self.arena;
                a.i32(flags);
                a.i32(p.depth);
                a.i32(p.character_id.unwrap_or(-1));
                let m = p.matrix.unwrap_or_default();
                a.f32(m.a);
                a.f32(m.b);
                a.f32(m.c);
                a.f32(m.d);
                a.f32(m.tx);
                a.f32(m.ty);
                let cx = p.cxform.unwrap_or_default();
                a.u32(cx.scale);
                a.u32(cx.bias);
                a.f32(p.ratio.unwrap_or(0.0));
                match &p.name {
                    Some(name) => self.deferred.string(&mut self.arena, name),
                    None => self.arena.ptr_value(0),
                }
                // APT uses nClipDepth as a signed sentinel: `>= 0` marks a
                // clipping mask (clip up to that depth), `< 0` a normal drawn
                // object. A non-clipping placement must therefore serialize -1,
                // NOT 0 — writing 0 makes the engine treat every object as a
                // mask and nothing renders to screen (AptDisplayList.cpp:1671).
                self.arena.i32(p.clip_depth.unwrap_or(-1));
                let actions_patch = self.arena.ptr_patch();

                let mut po3_patches = None;
                if p.is_place_object_3() {
                    self.arena.i32(p.blend_mode.unwrap_or(-1));
                    self.arena.u32(p.filters.len() as u32);
                    po3_patches = Some(self.arena.ptr_patch());
                }

                // Event action set + blocks + streams, in order.
                if let Some(blocks) = &p.clip_actions {
                    self.arena.align_ptr();
                    let set_off = self.arena.len() as u64;
                    self.arena.i32(blocks.len() as i32);
                    let blocks_patch = self.arena.ptr_patch();
                    self.arena.patch_ptr(actions_patch, set_off);

                    self.arena.align_ptr();
                    let blocks_off = self.arena.len() as u64;
                    let mut stream_patches = Vec::with_capacity(blocks.len());
                    for blk in blocks {
                        self.arena.align_ptr();
                        self.arena.i32(blk.triggers);
                        self.arena.i32(blk.key_code);
                        stream_patches.push(self.arena.ptr_patch());
                    }
                    self.arena.patch_ptr(blocks_patch, blocks_off);
                    for (patch, blk) in stream_patches.into_iter().zip(blocks) {
                        let s = self.stream(&blk.actions)?;
                        self.arena.patch_ptr(patch, s);
                    }
                }

                if let Some(filters_patch) = po3_patches {
                    self.arena.align_ptr();
                    let array_off = self.arena.len() as u64;
                    let mut slots = Vec::with_capacity(p.filters.len());
                    for _ in &p.filters {
                        slots.push(self.arena.ptr_patch());
                    }
                    self.arena.patch_ptr(filters_patch, array_off);
                    for (slot, filter) in slots.into_iter().zip(&p.filters) {
                        let f = self.write_filter(filter)?;
                        self.arena.patch_ptr(slot, f);
                    }
                }
            }
            Control::RemoveObject { depth } => self.arena.i32(*depth),
            Control::BackgroundColor(color) => self.arena.u32(*color),
            Control::StartSound { sound_id } | Control::StartSoundStream { sound_id } => {
                self.arena.i32(*sound_id)
            }
            Control::InitAction { sprite_id, actions } => {
                self.arena.i32(*sprite_id);
                let patch = self.arena.ptr_patch();
                let s = self.stream(actions)?;
                self.arena.patch_ptr(patch, s);
            }
        }
        Ok(off)
    }

    fn write_filter(&mut self, filter: &Filter) -> Result<u64> {
        self.arena.align_ptr();
        let off = self.arena.len() as u64;
        self.arena.u32(filter.filter_id());
        match filter {
            Filter::DropShadow {
                color,
                blur_x,
                blur_y,
                angle,
                distance,
                strength,
                flags,
            } => {
                let a = &mut self.arena;
                a.u32(*color);
                a.u32(*blur_x);
                a.u32(*blur_y);
                a.u32(*angle);
                a.u32(*distance);
                a.u16(*strength);
                a.u16(*flags);
            }
            Filter::Blur {
                blur_x,
                blur_y,
                flags,
            } => {
                let a = &mut self.arena;
                a.u32(*blur_x);
                a.u32(*blur_y);
                a.u16(*flags);
                a.u16(0);
            }
            Filter::Glow {
                color,
                blur_x,
                blur_y,
                strength,
                flags,
            } => {
                let a = &mut self.arena;
                a.u32(*color);
                a.u32(*blur_x);
                a.u32(*blur_y);
                a.u16(*strength);
                a.u16(*flags);
            }
            Filter::Bevel {
                highlight_color,
                shadow_color,
                blur_x,
                blur_y,
                angle,
                distance,
                strength,
                flags,
            } => {
                let a = &mut self.arena;
                a.u32(*highlight_color);
                a.u32(*shadow_color);
                a.u32(*blur_x);
                a.u32(*blur_y);
                a.u32(*angle);
                a.u32(*distance);
                a.u16(*strength);
                a.u16(*flags);
            }
            Filter::GradientGlow {
                is_bevel: _,
                colors,
                ratios,
                blur_x,
                blur_y,
                angle,
                distance,
                strength,
                flags,
            } => {
                if colors.len() != ratios.len() {
                    return Err(Error::Other(
                        "gradient filter colors/ratios length mismatch".into(),
                    ));
                }
                self.arena.u32(colors.len() as u32);
                let colors_patch = self.arena.ptr_patch();
                let ratios_patch = self.arena.ptr_patch();
                let a = &mut self.arena;
                a.u32(*blur_x);
                a.u32(*blur_y);
                a.u32(*angle);
                a.u32(*distance);
                a.u16(*strength);
                a.u16(*flags);
                a.align(4);
                let colors_off = a.len() as u64;
                for c in colors {
                    a.u32(*c);
                }
                a.patch_ptr(colors_patch, colors_off);
                let ratios_off = a.len() as u64;
                for r in ratios {
                    a.u8(*r);
                }
                a.patch_ptr(ratios_patch, ratios_off);
            }
            Filter::ColorMatrix { values } => {
                for v in values {
                    self.arena.f32(*v);
                }
            }
        }
        Ok(off)
    }

    fn stream(&mut self, stream: &ActionStream) -> Result<u64> {
        encode_stream(
            &mut self.arena,
            stream,
            &mut self.consts,
            &mut self.deferred,
        )
    }

    fn rect(&mut self, r: &Rect) {
        let a = &mut self.arena;
        a.f32(r.left);
        a.f32(r.top);
        a.f32(r.right);
        a.f32(r.bottom);
    }

    fn matrix(&mut self, m: &Matrix) {
        let a = &mut self.arena;
        a.f32(m.a);
        a.f32(m.b);
        a.f32(m.c);
        a.f32(m.d);
        a.f32(m.tx);
        a.f32(m.ty);
    }

    fn float_cxform(&mut self, cx: &FloatCxForm) {
        for v in cx.scale.iter().chain(cx.translate.iter()) {
            self.arena.f32(*v);
        }
    }
}
