//! Verifies texture embedding against the real corpus: that every bitmap fill
//! resolves to an embedded image, that the image data matches the source file
//! byte-for-byte, and that the fill matrix really is the inverse of the `.ru`
//! UV matrix (i.e. it maps a vertex back to its own position).
//!
//! Skips when the corpus at `/home/stephan/Devel/APT` is absent.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use apt::AptFile;
use apt_aux::{FileTextureSource, GeometryFormat, Style, TextureSource};
use apt_convert::ConvertOptions;
use swf::{FillStyle, Tag};

fn corpus() -> Option<PathBuf> {
    let p = PathBuf::from("/home/stephan/Devel/APT/BFME");
    p.is_dir().then_some(p)
}

/// Premultiply straight RGBA into the ARGB byte order SWF wants.
fn premultiply_argb(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len());
    for px in rgba.chunks_exact(4) {
        let (r, g, b, a) = (px[0] as u32, px[1] as u32, px[2] as u32, px[3] as u32);
        let mul = |c: u32| ((c * a + 127) / 255) as u8;
        out.extend_from_slice(&[a as u8, mul(r), mul(g), mul(b)]);
    }
    out
}

fn inflate(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data)
        .read_to_end(&mut out)
        .expect("inflate bitmap data");
    out
}

/// Convert one movie with textures and check the embedded bitmaps and fills.
fn check_movie(apt_path: &Path) {
    let base = apt::base_path(apt_path);
    let file = AptFile::load(apt_path).expect("load apt");
    let textures = FileTextureSource::new(&base);

    // Convert this movie alone: the test re-derives bitmap ids from its own
    // character table, which import inlining would renumber.
    let options = ConvertOptions {
        textures: true,
        inline_imports: false,
        ..Default::default()
    };
    let swf_bytes = apt_convert::convert_loaded_movie(&base, &file, &options).expect("convert");
    let buf = swf::decompress_swf(&swf_bytes[..]).expect("decompress swf");
    let parsed = swf::parse_swf(&buf).expect("parse swf");

    // Collect the embedded bitmaps.
    let mut embedded: HashMap<u16, (u16, u16, Vec<u8>)> = HashMap::new();
    for tag in &parsed.tags {
        if let Tag::DefineBitsLossless(b) = tag {
            assert_eq!(b.version, 2, "must be DefineBitsLossless2 (has alpha)");
            assert!(embedded
                .insert(b.id, (b.width, b.height, inflate(&b.data)))
                .is_none());
        }
    }

    // Every bitmap fill must resolve to an embedded bitmap of the right size.
    let mut fills = 0;
    for tag in &parsed.tags {
        let Tag::DefineShape(shape) = tag else {
            continue;
        };
        for style in &shape.styles.fill_styles {
            let FillStyle::Bitmap { id, .. } = style else {
                continue;
            };
            fills += 1;
            let (w, h, data) = embedded
                .get(id)
                .unwrap_or_else(|| panic!("fill references missing bitmap {id}"));
            assert_eq!(
                data.len(),
                *w as usize * *h as usize * 4,
                "bitmap {id} data size"
            );
        }
    }
    assert!(fills > 0, "{} produced no bitmap fills", base.display());

    // Pixel data must match some referenced source image exactly. Matched by
    // content (dimensions + pixels) rather than replaying the converter's id
    // assignment order, which is an internal detail this test shouldn't be
    // coupled to.
    let candidates = referenced_textures(&file, &base, &textures);
    for (id, (w, h, data)) in &embedded {
        let premultiplied = |t: &apt_aux::Texture| premultiply_argb(&t.rgba);
        let matched = candidates
            .iter()
            .any(|t| t.width as u16 == *w && t.height as u16 == *h && premultiplied(t) == *data);
        assert!(
            matched,
            "bitmap {id} ({w}x{h}) doesn't match any referenced source texture"
        );
    }

    check_fill_matrices(&file, &base, &textures, &parsed);
}

/// Every distinct texture the movie's shapes and directly-placed bitmap
/// characters reference (by content, not by the converter's internal ids).
fn referenced_textures(
    file: &AptFile,
    base: &Path,
    textures: &FileTextureSource,
) -> Vec<apt_aux::Texture> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let mut collect = |tex_id: u32| {
        if seen.insert(tex_id) {
            if let Some(t) = textures.texture(tex_id) {
                out.push(t);
            }
        }
    };
    for (i, slot) in file.movie.characters.iter().enumerate() {
        match slot.as_character() {
            Some(apt::Character::Shape(_)) => {
                let Ok(geom) = apt_aux::ru::RuFormat.load(base, i as u32) else {
                    continue;
                };
                for unit in &geom.units {
                    let Style::Textured {
                        bitmap_character_id,
                        ..
                    } = &unit.style
                    else {
                        continue;
                    };
                    collect(textures.texture_id(*bitmap_character_id));
                }
            }
            Some(apt::Character::Bitmap) => collect(textures.texture_id(i as u32)),
            _ => {}
        }
    }
    out
}

/// The SWF bitmap fill matrix maps bitmap space (bitmap pixels x 20) to shape
/// twips. Applying it to a vertex's UV must give back that vertex's position.
fn check_fill_matrices(
    file: &AptFile,
    base: &Path,
    textures: &FileTextureSource,
    parsed: &swf::Swf,
) {
    let shapes: HashMap<u16, &swf::Shape> = parsed
        .tags
        .iter()
        .filter_map(|t| match t {
            Tag::DefineShape(s) => Some((s.id, s)),
            _ => None,
        })
        .collect();

    let mut checked = 0;
    for (i, slot) in file.movie.characters.iter().enumerate() {
        let Some(apt::Character::Shape(_)) = slot.as_character() else {
            continue;
        };
        let Ok(geom) = apt_aux::ru::RuFormat.load(base, i as u32) else {
            continue;
        };
        let Some(shape) = shapes.get(&(i as u16)) else {
            continue;
        };

        for (unit_index, unit) in geom.units.iter().enumerate() {
            let Style::Textured {
                bitmap_character_id,
                matrix,
                ..
            } = &unit.style
            else {
                continue;
            };
            let Some(FillStyle::Bitmap { matrix: fill, .. }) =
                shape.styles.fill_styles.get(unit_index)
            else {
                continue;
            };
            if textures
                .texture(textures.texture_id(*bitmap_character_id))
                .is_none()
            {
                continue;
            }

            for &(x, y) in unit.vertices.iter().take(6) {
                // Texture pixels per the .ru matrix; the fill matrix maps
                // bitmap pixels to shape twips.
                let u = (matrix[0] * x + matrix[1] * y + matrix[4]) as f64;
                let v = (matrix[2] * x + matrix[3] * y + matrix[5]) as f64;

                // Apply the SWF fill matrix.
                let sx = fill.a.to_f64() * u + fill.c.to_f64() * v + fill.tx.get() as f64;
                let sy = fill.b.to_f64() * u + fill.d.to_f64() * v + fill.ty.get() as f64;

                let (want_x, want_y) = (x as f64 * 20.0, y as f64 * 20.0);
                let tol = 2.0 + 0.01 * want_x.abs().max(want_y.abs());
                assert!(
                    (sx - want_x).abs() <= tol && (sy - want_y).abs() <= tol,
                    "shape {i} unit {unit_index}: fill matrix maps uv ({u}, {v}) to \
                     ({sx}, {sy}) twips, expected ({want_x}, {want_y})"
                );
                checked += 1;
            }
        }
    }
    assert!(
        checked > 0,
        "no textured vertices checked for {}",
        base.display()
    );
}

#[test]
fn bitmap_fills_match_sources_and_invert_uv_matrix() {
    let Some(dir) = corpus() else {
        eprintln!("corpus absent; skipping");
        return;
    };
    // Movies known to carry textured geometry.
    let mut checked = 0;
    for name in [
        "MainMenu",
        "Palantir",
        "CampaignReview",
        "GuiFX",
        "ScoreScreen",
    ] {
        let apt = dir.join(format!("{name}.apt"));
        if apt.is_file() {
            check_movie(&apt);
            checked += 1;
        }
    }
    assert!(checked > 0, "no test movies found");
}
