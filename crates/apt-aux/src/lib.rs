//! Auxiliary (game-side) APT assets.
//!
//! The APT engine core only consumes `.apt`/`.const`; shape geometry and
//! textures are loaded through game-provided callbacks. This crate models
//! those aux assets in a game-agnostic way and implements the formats used by
//! the classic C&C/BFME-era games:
//!
//! - **Rendering units** (`GeometryFormat`): shape geometry keyed by shape
//!   character index. The classic format is `.ru` text files under
//!   `<base>_geometry/<index>.ru` ([`ru::RuFormat`]); other games may store
//!   geometry differently — implement [`GeometryFormat`] for those.
//! - **Texture maps** (`.dat`): bitmap character ID -> texture ID.

pub mod dat;
pub mod ru;
pub mod texture;

use std::path::Path;

use thiserror::Error;

pub use texture::{
    pack_textures, FileTextureSource, NoTextures, PackedRect, Texture, TextureSource,
};

#[derive(Debug, Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("geometry parse error: {0}")]
    Geometry(String),
    #[error("dat parse error: {0}")]
    Dat(String),
    #[error("texture error: {0}")]
    Texture(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// One drawable batch inside a shape: a style plus a vertex list.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderUnit {
    pub style: Style,
    /// (x, y) in pixels. Triangles consume 3 vertices each; lines 2.
    pub vertices: Vec<(f32, f32)>,
}

/// Colors are packed u32 in ARGB order (`a = byte 3 ... b = byte 0`).
#[derive(Debug, Clone, PartialEq)]
pub enum Style {
    /// Solid-color triangle list.
    Solid { color: u32 },
    /// Textured triangle list. `clipped` selects clamp vs wrap addressing.
    Textured {
        color: u32,
        /// Bitmap character ID (map through the `.dat` table to a texture ID).
        bitmap_character_id: u32,
        /// 2x3 matrix `[a, b, c, d, tx, ty]` taking a vertex position to
        /// texture *pixels*: `u = a*x + b*y + tx`, `v = c*x + d*y + ty`.
        matrix: [f32; 6],
        clipped: bool,
    },
    /// Solid-color line list.
    Line { color: u32, width: f32 },
}

/// The geometry of one Shape character.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ShapeGeometry {
    pub units: Vec<RenderUnit>,
}

/// A storage format for shape geometry, keyed by shape character index.
///
/// Classic C&C/BFME games use one `.ru` text file per shape under
/// `<base>_geometry/`; other games may use different encodings.
pub trait GeometryFormat {
    /// Parse one shape's geometry from raw bytes.
    fn parse(&self, data: &[u8]) -> Result<ShapeGeometry>;

    /// Serialize one shape's geometry.
    fn serialize(&self, geometry: &ShapeGeometry) -> Result<Vec<u8>>;

    /// Location of a shape's geometry relative to the movie base path
    /// (`<base>` without extension).
    fn path_for(&self, base: &Path, shape_index: u32) -> std::path::PathBuf;

    /// Load a shape's geometry for the movie at `base`.
    fn load(&self, base: &Path, shape_index: u32) -> Result<ShapeGeometry> {
        let data = std::fs::read(self.path_for(base, shape_index))?;
        self.parse(&data)
    }

    /// Store a shape's geometry for the movie at `base`.
    fn store(&self, base: &Path, shape_index: u32, geometry: &ShapeGeometry) -> Result<()> {
        let path = self.path_for(base, shape_index);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, self.serialize(geometry)?)?;
        Ok(())
    }
}
