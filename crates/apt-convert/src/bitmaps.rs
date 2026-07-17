//! Decode SWF-embedded bitmap characters into [`apt_aux::Texture`] (straight-
//! alpha RGBA8, top-left origin) so they can be re-packed into a texture
//! atlas for the APT side.
//!
//! Covers the three tag families actually used by DefineShape bitmap fills:
//! `DefineBitsJpeg2` (plain JPEG), `DefineBitsJpeg3` (JPEG + separate zlib
//! alpha channel), and `DefineBitsLossless`/`DefineBitsLossless2` (zlib-
//! compressed indexed/RGB15/RGB32 pixels). Plain `DefineBits` (needs a
//! separate shared `JpegTables` tag) isn't handled — it's a legacy SWF1
//! encoding not used by the C&C/BFME-era exporter this crate targets.

use std::borrow::Cow;
use std::io::Read;

use apt_aux::Texture;
use flate2::read::ZlibDecoder;
use swf::{BitmapFormat, DefineBitsJpeg3, DefineBitsLossless};

/// Decode a plain JPEG (`DefineBits`/`DefineBitsJpeg2` payload).
///
/// SWF8+ permits `DefineBitsJPEG2/3` to actually carry PNG or GIF instead of
/// JPEG, so we sniff the payload and fall back to `load_from_memory` when it's
/// not JPEG.
pub fn decode_jpeg(data: &[u8]) -> Option<Texture> {
    let cleaned = clean_jpeg_data(data);
    let img = image::load_from_memory_with_format(&cleaned, image::ImageFormat::Jpeg)
        .or_else(|_| image::load_from_memory(&cleaned))
        .inspect_err(|e| log::warn!("failed to decode embedded JPEG: {e}"))
        .ok()?;
    let rgba = img.to_rgba8();
    Some(Texture {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    })
}

/// Strip the erroneous `FF D9 FF D8` (EOI followed by SOI) marker pair that
/// the Flash JPEG exporter inserts into `DefineBits*` streams — once at the
/// very start, and once between the encoding tables and the image data. A
/// standard JPEG decoder stops at that premature EOI, so it must be removed
/// before decoding. (This mirrors Ruffle's `remove_invalid_jpeg_data`; a
/// mid-stream `FF D9 FF D8` never occurs in a well-formed JPEG, so removing it
/// is always safe.)
fn clean_jpeg_data(data: &[u8]) -> Cow<'_, [u8]> {
    const MARKER: [u8; 4] = [0xFF, 0xD9, 0xFF, 0xD8];
    let mut data = data;
    let mut stripped_leading = false;
    if data.starts_with(&MARKER) {
        data = &data[4..];
        stripped_leading = true;
    }
    match data.windows(4).position(|w| w == MARKER) {
        None if !stripped_leading => Cow::Borrowed(data),
        None => Cow::Owned(data.to_vec()),
        Some(pos) => {
            let mut out = Vec::with_capacity(data.len() - 4);
            out.extend_from_slice(&data[..pos]);
            out.extend_from_slice(&data[pos + 4..]);
            Cow::Owned(out)
        }
    }
}

/// Decode a `DefineBitsJpeg3` tag: the JPEG payload plus its separate
/// zlib-compressed 8-bit alpha channel (one byte per pixel, row-major).
pub fn decode_jpeg3(tag: &DefineBitsJpeg3) -> Option<Texture> {
    let mut tex = decode_jpeg(tag.data)?;
    if !tag.alpha_data.is_empty() {
        let mut alpha = Vec::new();
        if ZlibDecoder::new(tag.alpha_data)
            .read_to_end(&mut alpha)
            .is_ok()
        {
            let expected = (tex.width * tex.height) as usize;
            if alpha.len() >= expected {
                for (i, a) in alpha.iter().take(expected).enumerate() {
                    tex.rgba[i * 4 + 3] = *a;
                }
            } else {
                log::warn!(
                    "DefineBitsJpeg3 alpha channel too short ({} < {expected}), leaving opaque",
                    alpha.len()
                );
            }
        }
    }
    Some(tex)
}

/// Decode a `DefineBitsLossless`/`DefineBitsLossless2` tag.
pub fn decode_lossless(tag: &DefineBitsLossless) -> Option<Texture> {
    let mut raw = Vec::new();
    if let Err(e) = ZlibDecoder::new(&tag.data[..]).read_to_end(&mut raw) {
        log::warn!("failed to inflate DefineBitsLossless pixel data: {e}");
        return None;
    }
    let (width, height) = (tag.width as usize, tag.height as usize);
    let has_alpha = tag.version == 2;

    match tag.format {
        BitmapFormat::ColorMap8 { num_colors } => {
            let entry_size = if has_alpha { 4 } else { 3 };
            let palette_len = (num_colors as usize + 1) * entry_size;
            if raw.len() < palette_len {
                log::warn!("DefineBitsLossless: palette truncated");
                return None;
            }
            let (palette, pixels) = raw.split_at(palette_len);
            let stride = (width + 3) & !3;
            let mut rgba = vec![0u8; width * height * 4];
            for y in 0..height {
                let Some(row) = pixels.get(y * stride..y * stride + width) else {
                    break;
                };
                for (x, &idx) in row.iter().enumerate() {
                    let base = idx as usize * entry_size;
                    if base + entry_size > palette.len() {
                        continue;
                    }
                    let (r, g, b, a) = if has_alpha {
                        (
                            palette[base],
                            palette[base + 1],
                            palette[base + 2],
                            palette[base + 3],
                        )
                    } else {
                        (palette[base], palette[base + 1], palette[base + 2], 255)
                    };
                    let out = (y * width + x) * 4;
                    rgba[out..out + 4].copy_from_slice(&[r, g, b, a]);
                }
            }
            Some(Texture {
                width: width as u32,
                height: height as u32,
                rgba,
            })
        }
        BitmapFormat::Rgb15 => {
            let stride = (width * 2 + 3) & !3;
            let mut rgba = vec![0u8; width * height * 4];
            let expand5 = |c: u16| ((c as u32 * 255 + 15) / 31) as u8;
            for y in 0..height {
                let Some(row) = raw.get(y * stride..y * stride + width * 2) else {
                    break;
                };
                for x in 0..width {
                    let px = u16::from_be_bytes([row[x * 2], row[x * 2 + 1]]);
                    let (r, g, b) = ((px >> 10) & 0x1F, (px >> 5) & 0x1F, px & 0x1F);
                    let out = (y * width + x) * 4;
                    rgba[out..out + 4].copy_from_slice(&[expand5(r), expand5(g), expand5(b), 255]);
                }
            }
            Some(Texture {
                width: width as u32,
                height: height as u32,
                rgba,
            })
        }
        BitmapFormat::Rgb32 => {
            let stride = width * 4;
            let mut rgba = vec![0u8; width * height * 4];
            // DefineBitsLossless2 (has_alpha) stores premultiplied alpha;
            // Texture wants straight alpha.
            let unpremultiply = |c: u8, a: u8| {
                if a == 0 {
                    0
                } else {
                    ((c as u32 * 255) / a as u32).min(255) as u8
                }
            };
            for y in 0..height {
                let Some(row) = raw.get(y * stride..y * stride + stride) else {
                    break;
                };
                for x in 0..width {
                    let px = &row[x * 4..x * 4 + 4];
                    let out = (y * width + x) * 4;
                    if has_alpha {
                        let (a, r, g, b) = (px[0], px[1], px[2], px[3]);
                        rgba[out..out + 4].copy_from_slice(&[
                            unpremultiply(r, a),
                            unpremultiply(g, a),
                            unpremultiply(b, a),
                            a,
                        ]);
                    } else {
                        // (reserved, R, G, B)
                        rgba[out..out + 4].copy_from_slice(&[px[1], px[2], px[3], 255]);
                    }
                }
            }
            Some(Texture {
                width: width as u32,
                height: height as u32,
                rgba,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn decodes_rgb32_with_straight_alpha() {
        // 1x1 pixel, half-transparent premultiplied red: alpha=128, premult
        // red = 128 (i.e. 255*0.5 rounded), straight should recover ~255.
        let pixel = [128u8, 128, 0, 0]; // (a, r, g, b)
        let tag = DefineBitsLossless {
            version: 2,
            id: 1,
            format: BitmapFormat::Rgb32,
            width: 1,
            height: 1,
            data: std::borrow::Cow::Owned(zlib(&pixel)),
        };
        let tex = decode_lossless(&tag).unwrap();
        assert_eq!(tex.width, 1);
        assert_eq!(tex.height, 1);
        assert_eq!(tex.rgba[3], 128); // alpha preserved
        assert!(tex.rgba[0] >= 250); // unpremultiplied red back near 255
    }

    #[test]
    fn decodes_colormap8_opaque() {
        // Palette: index 0 = red. One row, one pixel, padded to 4 bytes.
        let mut raw = vec![255u8, 0, 0]; // 1-entry palette (num_colors=0), RGB
        raw.extend_from_slice(&[0, 0, 0, 0]); // pixel row (index 0) padded to 4 bytes
        let tag = DefineBitsLossless {
            version: 1,
            id: 1,
            format: BitmapFormat::ColorMap8 { num_colors: 0 },
            width: 1,
            height: 1,
            data: std::borrow::Cow::Owned(zlib(&raw)),
        };
        let tex = decode_lossless(&tag).unwrap();
        assert_eq!(&tex.rgba, &[255, 0, 0, 255]);
    }
}
