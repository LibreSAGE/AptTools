//! Textures referenced by shape geometry.
//!
//! Shapes reference a *bitmap character ID*; the `.dat` map ([`crate::dat`])
//! resolves that to a *texture ID*, which a [`TextureSource`] turns into pixels.
//! Games differ in where they keep the image files, so the lookup is behind the
//! trait; [`FileTextureSource`] implements the conventions used by the classic
//! games:
//!
//! - `art/Textures/apt_<base>_<texid>.<ext>` — BFME / BFME2 (shared texture dir)
//! - `<base>_textures/<texid>.<ext>` — C&C3 / KW / RA3 (per-movie dir)

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rectutils::pack::RectPacker;

use crate::dat::TextureMap;
use crate::{Error, Result};

/// A decoded texture: 8-bit RGBA, **straight** (not premultiplied) alpha,
/// top-left origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Texture {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes, row-major RGBA.
    pub rgba: Vec<u8>,
}

impl Texture {
    /// Decode an image from bytes. The format is sniffed from the content, and
    /// from `hint_path`'s extension when sniffing is inconclusive (TGA has no
    /// magic number).
    pub fn decode(data: &[u8], hint_path: Option<&Path>) -> Result<Texture> {
        let format = image::guess_format(data).ok().or_else(|| {
            hint_path
                .and_then(|p| p.extension())
                .and_then(|e| e.to_str())
                .and_then(image::ImageFormat::from_extension)
        });
        let img = match format {
            Some(f) => image::load_from_memory_with_format(data, f),
            None => image::load_from_memory(data),
        }
        .map_err(|e| Error::Texture(format!("decode failed: {e}")))?;
        let rgba = img.to_rgba8();
        Ok(Texture {
            width: rgba.width(),
            height: rgba.height(),
            rgba: rgba.into_raw(),
        })
    }

    pub fn load(path: &Path) -> Result<Texture> {
        let data = std::fs::read(path)?;
        Texture::decode(&data, Some(path))
    }

    /// Encode and write this texture to `path`; the image format is chosen from
    /// the path's extension by the `image` crate.
    ///
    /// TGA is written **uncompressed** (truecolor type 2): the classic games'
    /// texture loaders reject RLE-compressed TGA, and `image` would otherwise
    /// default to RLE.
    pub fn save(&self, path: &Path) -> Result<()> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        if ext.as_deref() == Some("tga") {
            use image::ImageEncoder;
            let file = std::fs::File::create(path)?;
            image::codecs::tga::TgaEncoder::new(std::io::BufWriter::new(file))
                .disable_rle()
                .write_image(
                    &self.rgba,
                    self.width,
                    self.height,
                    image::ExtendedColorType::Rgba8,
                )
                .map_err(|e| Error::Texture(format!("tga encode failed: {e}")))?;
            return Ok(());
        }
        let img = image::RgbaImage::from_raw(self.width, self.height, self.rgba.clone())
            .ok_or_else(|| Error::Texture("rgba buffer does not match dimensions".into()))?;
        img.save(path)
            .map_err(|e| Error::Texture(format!("save failed: {e}")))
    }
}

/// Where one input texture landed inside a packed atlas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackedRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// 1px transparent gutter kept between packed sub-textures (and around the
/// atlas edge) so bilinear sampling can't bleed across neighbors.
const ATLAS_PADDING: u32 = 1;

/// Try to place every texture (each grown by [`ATLAS_PADDING`]) into a
/// `side`x`side` page with the `rectutils` binary-tree packer. Returns each
/// input's placement (indexed like `textures`) on success, or `None` if any
/// texture doesn't fit.
fn try_pack(textures: &[Texture], order: &[usize], side: u32) -> Option<Vec<PackedRect>> {
    let mut packer = RectPacker::<u32>::new(side, side);
    let mut rects = vec![
        PackedRect {
            x: 0,
            y: 0,
            width: 0,
            height: 0
        };
        textures.len()
    ];
    for &i in order {
        let t = &textures[i];
        let free = packer.find_free(t.width + ATLAS_PADDING, t.height + ATLAS_PADDING)?;
        rects[i] = PackedRect {
            x: free.position.x,
            y: free.position.y,
            width: t.width,
            height: t.height,
        };
    }
    Some(rects)
}

/// Pack textures into one shared **square, power-of-two** atlas (512x512,
/// 1024x1024, ...) using the `rectutils` packer. Tries the smallest square
/// power-of-two that could hold the content and doubles until everything fits.
///
/// Returns `None` if the textures don't fit within a `max_side`x`max_side`
/// square — the caller should then skip packing and export each texture
/// standalone.
///
/// On success, returns the atlas plus each input's placement, indexed the same
/// as `textures` (largest are placed first internally for a tighter fit).
pub fn pack_textures(textures: &[Texture], max_side: u32) -> Option<(Texture, Vec<PackedRect>)> {
    if textures.is_empty() {
        return None;
    }

    // Largest first: the packer fills better when big rectangles are placed
    // while the page is still mostly empty.
    let mut order: Vec<usize> = (0..textures.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(textures[i].width * textures[i].height));

    // Smallest square power-of-two that could conceivably hold the content: it
    // must fit the largest single (padded) side and have enough total area.
    let min_side = textures
        .iter()
        .map(|t| t.width.max(t.height) + ATLAS_PADDING)
        .max()
        .unwrap_or(1);
    let total_area: u64 = textures
        .iter()
        .map(|t| (t.width + ATLAS_PADDING) as u64 * (t.height + ATLAS_PADDING) as u64)
        .sum();
    let mut side = 1u32;
    while side < min_side || (side as u64) * (side as u64) < total_area {
        side <<= 1;
    }

    let rects = loop {
        if side > max_side {
            return None;
        }
        if let Some(rects) = try_pack(textures, &order, side) {
            break rects;
        }
        side <<= 1;
    };

    let mut rgba = vec![0u8; (side as usize) * (side as usize) * 4];
    for (i, t) in textures.iter().enumerate() {
        let r = rects[i];
        for y in 0..t.height {
            let src_start = (y * t.width * 4) as usize;
            let src = &t.rgba[src_start..src_start + (t.width * 4) as usize];
            let dst_row = r.y + y;
            let dst_start = ((dst_row * side + r.x) * 4) as usize;
            rgba[dst_start..dst_start + (t.width * 4) as usize].copy_from_slice(src);
        }
    }
    Some((
        Texture {
            width: side,
            height: side,
            rgba,
        },
        rects,
    ))
}

#[cfg(test)]
mod pack_tests {
    use super::*;

    fn solid(width: u32, height: u32, color: [u8; 4]) -> Texture {
        let mut rgba = vec![0u8; (width * height * 4) as usize];
        for px in rgba.chunks_mut(4) {
            px.copy_from_slice(&color);
        }
        Texture {
            width,
            height,
            rgba,
        }
    }

    #[test]
    fn packs_without_overlap_and_preserves_pixels() {
        let textures = vec![
            solid(4, 4, [255, 0, 0, 255]),
            solid(3, 6, [0, 255, 0, 255]),
            solid(5, 2, [0, 0, 255, 255]),
        ];
        let (atlas, rects) = pack_textures(&textures, 64).expect("must fit in 64x64");
        assert_eq!(rects.len(), 3);
        // Square power-of-two atlas.
        assert_eq!(atlas.width, atlas.height);
        assert!(atlas.width.is_power_of_two());

        // Every rect's placed pixels must match its source texture exactly,
        // and rects (with 1px padding) must not overlap each other.
        for (i, t) in textures.iter().enumerate() {
            let r = rects[i];
            assert_eq!(r.width, t.width);
            assert_eq!(r.height, t.height);
            for y in 0..t.height {
                for x in 0..t.width {
                    let src = &t.rgba[((y * t.width + x) * 4) as usize..][..4];
                    let dst_off = (((r.y + y) * atlas.width + (r.x + x)) * 4) as usize;
                    assert_eq!(
                        &atlas.rgba[dst_off..dst_off + 4],
                        src,
                        "mismatch at texture {i} ({x},{y})"
                    );
                }
            }
        }
        for a in 0..rects.len() {
            for b in (a + 1)..rects.len() {
                let (ra, rb) = (rects[a], rects[b]);
                let overlap = ra.x < rb.x + rb.width + 1
                    && rb.x < ra.x + ra.width + 1
                    && ra.y < rb.y + rb.height + 1
                    && rb.y < ra.y + ra.height + 1;
                assert!(!overlap, "rects {a} and {b} overlap: {ra:?} vs {rb:?}");
            }
        }
    }
}

/// Resolves the textures a movie's shapes reference.
pub trait TextureSource {
    /// Map a bitmap character ID to a texture ID. Several characters may share
    /// one texture (e.g. a packed atlas), so callers can dedupe on this.
    fn texture_id(&self, bitmap_character_id: u32) -> u32 {
        bitmap_character_id
    }

    /// Load the texture with the given texture ID, or `None` if unavailable.
    fn texture(&self, texture_id: u32) -> Option<Texture>;
}

/// A [`TextureSource`] with no textures.
pub struct NoTextures;
impl TextureSource for NoTextures {
    fn texture(&self, _: u32) -> Option<Texture> {
        None
    }
}

/// Loads textures from the file layouts used by the classic games, applying the
/// movie's `.dat` map. Decoded textures are cached.
pub struct FileTextureSource {
    /// Movie base path (no extension).
    base: PathBuf,
    map: TextureMap,
    cache: RefCell<HashMap<u32, Option<Texture>>>,
}

/// Extensions tried, in order, for each candidate location.
const EXTENSIONS: &[&str] = &["tga", "dds", "png", "jpg", "jpeg"];

impl FileTextureSource {
    /// Build a source for the movie at `base` (path without extension), loading
    /// `<base>.dat` if it exists.
    pub fn new(base: &Path) -> FileTextureSource {
        let map = TextureMap::load(&base.with_extension("dat")).unwrap_or_default();
        FileTextureSource {
            base: base.to_path_buf(),
            map,
            cache: RefCell::new(HashMap::new()),
        }
    }

    fn movie_name(&self) -> &str {
        self.base
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("apt")
    }

    /// Candidate files for a texture ID, most likely first.
    fn candidates(&self, texture_id: u32) -> Vec<PathBuf> {
        let dir = self.base.parent().unwrap_or(Path::new("."));
        let name = self.movie_name();
        let mut out = Vec::new();
        for ext in EXTENSIONS {
            // BFME / BFME2: one shared texture directory.
            out.push(
                dir.join("art/Textures")
                    .join(format!("apt_{name}_{texture_id}.{ext}")),
            );
            // C&C3 / KW / RA3: per-movie texture directory.
            out.push(
                dir.join(format!("{name}_textures"))
                    .join(format!("{texture_id}.{ext}")),
            );
        }
        out
    }
}

impl TextureSource for FileTextureSource {
    fn texture_id(&self, bitmap_character_id: u32) -> u32 {
        self.map.texture_id(bitmap_character_id)
    }

    fn texture(&self, texture_id: u32) -> Option<Texture> {
        if let Some(hit) = self.cache.borrow().get(&texture_id) {
            return hit.clone();
        }
        let found = self
            .candidates(texture_id)
            .into_iter()
            .find(|p| p.is_file())
            .and_then(|p| match Texture::load(&p) {
                Ok(t) => Some(t),
                Err(e) => {
                    eprintln!("warning: failed to load texture {}: {e}", p.display());
                    None
                }
            });
        self.cache.borrow_mut().insert(texture_id, found.clone());
        found
    }
}
