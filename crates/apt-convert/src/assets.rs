//! Where a movie's shape geometry and textures come from.
//!
//! After import resolution a movie's characters can originate from several
//! movies at once, and geometry/textures live next to the movie that *defined*
//! a character — not the one being converted. So lookups are keyed by the
//! shape's index in the converted movie, and the implementation maps that back
//! to the right directory.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

use apt_aux::{FileTextureSource, GeometryFormat, ShapeGeometry, Texture, TextureSource};

use crate::imports::Origin;

/// A texture's identity, used to embed shared images only once. Two shapes that
/// resolve to the same file must produce equal keys.
pub type TextureKey = String;

/// Supplies the assets a shape needs.
pub trait Assets {
    /// Geometry for the shape at `shape_index`.
    fn geometry(&self, shape_index: u32) -> Option<ShapeGeometry>;

    /// The texture a shape's geometry refers to by `bitmap_character_id`
    /// (an id in the id-space of the movie that defined the shape).
    fn texture(&self, shape_index: u32, bitmap_character_id: u32) -> Option<(TextureKey, Texture)>;
}

/// Geometry only; textured shapes fall back to their vertex color.
pub struct GeometryOnly<G>(pub G);

impl<G: Fn(u32) -> Option<ShapeGeometry>> Assets for GeometryOnly<G> {
    fn geometry(&self, shape_index: u32) -> Option<ShapeGeometry> {
        (self.0)(shape_index)
    }

    fn texture(&self, _: u32, _: u32) -> Option<(TextureKey, Texture)> {
        None
    }
}

/// Nothing at all: shapes come out empty.
pub struct NoAssets;

impl Assets for NoAssets {
    fn geometry(&self, _: u32) -> Option<ShapeGeometry> {
        None
    }

    fn texture(&self, _: u32, _: u32) -> Option<(TextureKey, Texture)> {
        None
    }
}

/// Loads `.ru` geometry and textures from disk, following each character back to
/// the movie it came from.
pub struct DiskAssets {
    origins: Vec<Origin>,
    with_textures: bool,
    /// One texture source per origin movie, created on demand.
    sources: RefCell<HashMap<std::path::PathBuf, FileTextureSource>>,
}

impl DiskAssets {
    /// `origins` comes from [`crate::imports::resolve`]; pass `with_textures`
    /// to embed images rather than flat colors.
    pub fn new(origins: Vec<Origin>, with_textures: bool) -> DiskAssets {
        DiskAssets {
            origins,
            with_textures,
            sources: RefCell::new(HashMap::new()),
        }
    }

    /// Assets for a single movie with no imports resolved.
    pub fn for_movie(base: &Path, character_count: usize, with_textures: bool) -> DiskAssets {
        let origins = (0..character_count)
            .map(|i| Origin {
                base: base.to_path_buf(),
                index: i as u32,
            })
            .collect();
        DiskAssets::new(origins, with_textures)
    }

    fn origin(&self, shape_index: u32) -> Option<&Origin> {
        self.origins.get(shape_index as usize)
    }

    fn with_source<T>(&self, base: &Path, f: impl FnOnce(&FileTextureSource) -> T) -> T {
        let mut sources = self.sources.borrow_mut();
        let source = sources
            .entry(base.to_path_buf())
            .or_insert_with(|| FileTextureSource::new(base));
        f(source)
    }
}

impl Assets for DiskAssets {
    fn geometry(&self, shape_index: u32) -> Option<ShapeGeometry> {
        let origin = self.origin(shape_index)?;
        apt_aux::ru::RuFormat.load(&origin.base, origin.index).ok()
    }

    fn texture(&self, shape_index: u32, bitmap_character_id: u32) -> Option<(TextureKey, Texture)> {
        if !self.with_textures {
            return None;
        }
        let origin = self.origin(shape_index)?;
        self.with_source(&origin.base, |source| {
            let texture_id = source.texture_id(bitmap_character_id);
            let texture = source.texture(texture_id)?;
            Some((format!("{}#{texture_id}", origin.base.display()), texture))
        })
    }
}
