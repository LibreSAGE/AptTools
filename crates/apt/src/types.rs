//! In-memory data model for an APT movie, independent of the on-disk layout
//! (pointer size / decoupled variant).

use crate::actions::ActionStream;

/// Pointer size an `.apt`/`.const` pair was built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtrSize {
    Four,
    Eight,
}

impl PtrSize {
    pub fn bytes(self) -> usize {
        match self {
            PtrSize::Four => 4,
            PtrSize::Eight => 8,
        }
    }

    pub fn digit(self) -> char {
        match self {
            PtrSize::Four => '4',
            PtrSize::Eight => '8',
        }
    }
}

/// Info from the 16-byte `"Apt Data:<decoupled>:<swfver>:<ptrsize>"` tag.
///
/// Classic (Zero Hour / BFME era) files carry a short tag (`"Apt Data:6\x1a"`);
/// missing fields default to non-decoupled, SWF 6, 4-byte pointers — exactly
/// as the engine's sniffing does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub decoupled: bool,
    pub swf_version: u8,
    pub ptr_size: PtrSize,
    /// Raw first 16 bytes of the file, kept for diagnostics / faithful re-emit.
    pub raw_tag: [u8; 16],
}

pub const PARENT_ANIM_MAGIC: u32 = 0x0987_6543;
pub const FUNC_POOL_ITEMS_MAGIC: u32 = 0x9876_5432;
pub const FUNC_POOL_ARRAY_MAGIC: u32 = 0x1234_5678;

/// 2D axis-aligned rectangle (pixels).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

/// 2x3 affine transform: `[a b; c d]` + translation `(tx, ty)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Matrix {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub tx: f32,
    pub ty: f32,
}

impl Default for Matrix {
    fn default() -> Self {
        Matrix {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            tx: 0.0,
            ty: 0.0,
        }
    }
}

/// Color transform as float arrays: per-channel scale and translate (ARGB order).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FloatCxForm {
    pub scale: [f32; 4],
    pub translate: [f32; 4],
}

impl Default for FloatCxForm {
    fn default() -> Self {
        FloatCxForm {
            scale: [1.0; 4],
            translate: [0.0; 4],
        }
    }
}

/// Color transform packed as two u32s, one byte per ARGB channel.
/// Scale bytes map `byte/254` to a multiplier; bias bytes are additive 0-255.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PackedCxForm {
    pub scale: u32,
    pub bias: u32,
}

impl Default for PackedCxForm {
    fn default() -> Self {
        PackedCxForm {
            scale: 0xFEFE_FEFE,
            bias: 0,
        }
    }
}

/// A constant value as stored in the `.const` table / pushed by actions.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Undefined,
    String(String),
    Register(i32),
    Boolean(bool),
    Float(f32),
    Integer(i32),
    /// Index into the currently active runtime constant pool (DefineDictionary).
    Lookup(u32),
}

/// The root Animation character: the movie itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Movie {
    pub frames: Vec<Frame>,
    /// One slot per character ID. Index 0 is the root itself (`CharacterSlot::Root`).
    pub characters: Vec<CharacterSlot>,
    pub width: u32,
    pub height: u32,
    pub ms_per_frame: u32,
    pub imports: Vec<Import>,
    pub exports: Vec<Export>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CharacterSlot {
    /// Slot 0: the root animation itself.
    Root,
    /// Empty slot — filled at runtime by an import, or simply unused.
    Empty,
    Character(Character),
}

impl CharacterSlot {
    pub fn as_character(&self) -> Option<&Character> {
        match self {
            CharacterSlot::Character(c) => Some(c),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Import {
    /// Base name of the movie to import from.
    pub movie: String,
    /// Export name inside that movie.
    pub name: String,
    /// Character ID this import occupies.
    pub character_id: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Export {
    pub name: String,
    /// Exported character ID.
    pub character_id: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Character {
    Shape(Shape),
    /// EditText.
    Text(Text),
    Font(Font),
    Button(Button),
    Sprite(Sprite),
    Sound,
    Bitmap,
    Morph(Morph),
    StaticText(StaticText),
    /// "Might be a packed texture"; carries no payload.
    None,
    /// Ignored by the engine; no payload.
    Video,
}

impl Character {
    pub fn type_id(&self) -> i32 {
        match self {
            Character::Shape(_) => 1,
            Character::Text(_) => 2,
            Character::Font(_) => 3,
            Character::Button(_) => 4,
            Character::Sprite(_) => 5,
            Character::Sound => 6,
            Character::Bitmap => 7,
            Character::Morph(_) => 8,
            Character::StaticText(_) => 10,
            Character::None => 11,
            Character::Video => 12,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Character::Shape(_) => "Shape",
            Character::Text(_) => "Text",
            Character::Font(_) => "Font",
            Character::Button(_) => "Button",
            Character::Sprite(_) => "Sprite",
            Character::Sound => "Sound",
            Character::Bitmap => "Bitmap",
            Character::Morph(_) => "Morph",
            Character::StaticText(_) => "StaticText",
            Character::None => "None",
            Character::Video => "Video",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Shape {
    pub bounds: Rect,
    /// Decoupled files only: the bitmap character ID backing this shape
    /// (0 / `None` = untextured). Ignored when writing non-decoupled files.
    pub bitmap_character_id: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAlignment {
    Left = 0,
    Right = 1,
    Center = 2,
    None = 3,
    Justify = 4,
}

impl TextAlignment {
    pub fn from_i32(v: i32) -> TextAlignment {
        match v {
            0 => TextAlignment::Left,
            1 => TextAlignment::Right,
            2 => TextAlignment::Center,
            4 => TextAlignment::Justify,
            _ => TextAlignment::None,
        }
    }
}

/// EditText character.
#[derive(Debug, Clone, PartialEq)]
pub struct Text {
    pub bounds: Rect,
    /// Character ID of the Font.
    pub font_id: i32,
    pub alignment: TextAlignment,
    /// RGBA color, packed u32.
    pub color: u32,
    pub font_height: f32,
    pub read_only: bool,
    pub multiline: bool,
    pub word_wrap: bool,
    pub initial_text: String,
    /// Bound ActionScript variable path.
    pub variable: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Font {
    pub name: String,
    /// Character indices of the glyph Shape characters.
    pub glyphs: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Button {
    pub is_menu: bool,
    pub hit_test_bounds: Rect,
    /// Hit-test mesh: vertex (x, y) pairs and triangle index triples.
    pub hit_test_vertices: Vec<(f32, f32)>,
    pub hit_test_triangles: Vec<[i16; 3]>,
    pub records: Vec<ButtonRecord>,
    pub actions: Vec<ButtonAction>,
    pub sounds: Option<ButtonSounds>,
}

/// Bit flags for `ButtonRecord::states`: Up=1, Over=2, Down=4, HitTest=8.
#[derive(Debug, Clone, PartialEq)]
pub struct ButtonRecord {
    pub states: i32,
    /// Character index displayed for these states.
    pub character_id: u32,
    pub layer: i32,
    pub matrix: Matrix,
    pub cxform: FloatCxForm,
}

/// `conditions` is an `AptActionConditionFlag` mask; the key code lives in
/// bits 9-15 (mask 0xFE00).
#[derive(Debug, Clone, PartialEq)]
pub struct ButtonAction {
    pub conditions: i32,
    pub actions: ActionStream,
}

/// Character indices of transition sounds; 0 = none.
#[derive(Debug, Clone, PartialEq)]
pub struct ButtonSounds {
    pub over_up_to_idle: u32,
    pub idle_to_over_up: u32,
    pub over_up_to_over_down: u32,
    pub over_down_to_over_up: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sprite {
    pub frames: Vec<Frame>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Morph {
    pub start_character_id: u32,
    pub end_character_id: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StaticText {
    pub bounds: Rect,
    pub matrix: Matrix,
    pub records: Vec<StaticTextRecord>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StaticTextRecord {
    pub font_id: i32,
    pub cxform: FloatCxForm,
    pub x_offset: f32,
    pub y_offset: f32,
    pub scale: f32,
    pub glyphs: Vec<GlyphEntry>,
}

/// `index` indexes the font's glyph array; `advance` moves the pen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlyphEntry {
    pub index: i16,
    pub advance: i16,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Frame {
    pub controls: Vec<Control>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Control {
    Action(ActionStream),
    FrameLabel(String),
    PlaceObject(PlaceObject),
    RemoveObject {
        depth: i32,
    },
    BackgroundColor(u32),
    StartSound {
        sound_id: i32,
    },
    StartSoundStream {
        sound_id: i32,
    },
    InitAction {
        sprite_id: i32,
        actions: ActionStream,
    },
}

impl Control {
    pub fn type_id(&self) -> i32 {
        match self {
            Control::Action(_) => 1,
            Control::FrameLabel(_) => 2,
            Control::PlaceObject(p) if !p.is_place_object_3() => 3,
            Control::RemoveObject { .. } => 4,
            Control::BackgroundColor(_) => 5,
            Control::StartSound { .. } => 6,
            Control::StartSoundStream { .. } => 7,
            Control::InitAction { .. } => 8,
            Control::PlaceObject(_) => 9,
        }
    }
}

/// PlaceObject2 / PlaceObject3. Serialized as PlaceObject3 iff `blend_mode`
/// or `filters` is set.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlaceObject {
    /// `Move` flag: modify the object at `depth` instead of placing a new one.
    pub is_move: bool,
    pub depth: i32,
    pub character_id: Option<i32>,
    pub matrix: Option<Matrix>,
    pub cxform: Option<PackedCxForm>,
    /// Morph ratio.
    pub ratio: Option<f32>,
    pub name: Option<String>,
    pub clip_depth: Option<i32>,
    pub clip_actions: Option<Vec<EventAction>>,
    /// PlaceObject3 only; -1 = unset in the file.
    pub blend_mode: Option<i32>,
    /// PlaceObject3 only.
    pub filters: Vec<Filter>,
}

impl PlaceObject {
    pub fn is_place_object_3(&self) -> bool {
        self.blend_mode.is_some() || !self.filters.is_empty()
    }
}

/// `triggers` is an `AptEventActionFlag` mask (OnLoad=1, EnterFrame=2, ...,
/// KeyPress=0x20000, Construct=0x40000, Wheel=0x80000).
#[derive(Debug, Clone, PartialEq)]
pub struct EventAction {
    pub triggers: i32,
    pub key_code: i32,
    pub actions: ActionStream,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    DropShadow {
        color: u32,
        blur_x: u32,
        blur_y: u32,
        angle: u32,
        distance: u32,
        strength: u16,
        flags: u16,
    },
    Blur {
        blur_x: u32,
        blur_y: u32,
        flags: u16,
    },
    Glow {
        color: u32,
        blur_x: u32,
        blur_y: u32,
        strength: u16,
        flags: u16,
    },
    Bevel {
        highlight_color: u32,
        shadow_color: u32,
        blur_x: u32,
        blur_y: u32,
        angle: u32,
        distance: u32,
        strength: u16,
        flags: u16,
    },
    /// id 4 (glow) and 7 (bevel) share this struct.
    GradientGlow {
        is_bevel: bool,
        colors: Vec<u32>,
        ratios: Vec<u8>,
        blur_x: u32,
        blur_y: u32,
        angle: u32,
        distance: u32,
        strength: u16,
        flags: u16,
    },
    ColorMatrix {
        values: [f32; 20],
    },
}

impl Filter {
    pub fn filter_id(&self) -> u32 {
        match self {
            Filter::DropShadow { .. } => 0,
            Filter::Blur { .. } => 1,
            Filter::Glow { .. } => 2,
            Filter::Bevel { .. } => 3,
            Filter::GradientGlow {
                is_bevel: false, ..
            } => 4,
            Filter::ColorMatrix { .. } => 6,
            Filter::GradientGlow { is_bevel: true, .. } => 7,
        }
    }
}
