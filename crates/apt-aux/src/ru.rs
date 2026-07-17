//! The `.ru` rendering-unit geometry text format used by classic
//! C&C/BFME-era games (`<base>_geometry/<index>.ru`).
//!
//! Line-oriented ASCII (CRLF or LF). The first token dispatches:
//! - `c`  — start a new rendering unit
//! - `s s:c0:c1:c2:c3`                             — solid triangles
//! - `s tc:c0:c1:c2:c3:bitmapId:a:b:c:d:tx:ty`     — textured (clipped; `tw` = tiled)
//!
//! The six textured-style floats are a 2x3 matrix taking a vertex position to
//! texture pixels: `u = a*x + b*y + tx`, `v = c*x + d*y + ty` (the engine loads
//! them into a column-major 4x4 as cells 0, 4, 1, 5, 12, 13 and divides row 0
//! by the texture width and row 1 by its height).
//! - `t f:f:...`  — append float vertex tokens to the current unit
//!
//! Colors on the line are `b:g:r:a` bytes packed into a u32 as ARGB.

use std::path::{Path, PathBuf};

use crate::{Error, GeometryFormat, RenderUnit, Result, ShapeGeometry, Style};

/// Parser/serializer for the classic `.ru` format.
#[derive(Debug, Clone, Copy, Default)]
pub struct RuFormat;

fn pack_color(c0: u8, c1: u8, c2: u8, c3: u8) -> u32 {
    c0 as u32 | (c1 as u32) << 8 | (c2 as u32) << 16 | (c3 as u32) << 24
}

fn unpack_color(c: u32) -> (u8, u8, u8, u8) {
    (c as u8, (c >> 8) as u8, (c >> 16) as u8, (c >> 24) as u8)
}

impl GeometryFormat for RuFormat {
    fn parse(&self, data: &[u8]) -> Result<ShapeGeometry> {
        let text = String::from_utf8_lossy(data);
        let mut units: Vec<RenderUnit> = Vec::new();
        // Flat x,y,x,y... floats accumulated per unit across its `t` lines.
        let mut coords: Vec<Vec<f32>> = Vec::new();

        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            let mut parts = line.split_whitespace();
            let cmd = parts.next().unwrap_or("");
            match cmd {
                "c" => {
                    units.push(RenderUnit {
                        style: Style::Solid { color: 0 },
                        vertices: Vec::new(),
                    });
                    coords.push(Vec::new());
                }
                "s" => {
                    let spec = parts
                        .next()
                        .ok_or_else(|| err("missing style spec after 's'"))?;
                    let style = parse_style(spec)?;
                    units
                        .last_mut()
                        .ok_or_else(|| err("'s' before any 'c'"))?
                        .style = style;
                }
                "t" | "l" => {
                    let spec = parts.next().unwrap_or("");
                    let buf = coords
                        .last_mut()
                        .ok_or_else(|| err("vertex line before any 'c'"))?;
                    for tok in spec.split(':').filter(|t| !t.is_empty()) {
                        buf.push(tok.parse().map_err(|_| err("bad vertex float"))?);
                    }
                }
                _ => {} // ignore unknown lines
            }
        }
        for (unit, buf) in units.iter_mut().zip(coords) {
            unit.vertices = buf.chunks_exact(2).map(|p| (p[0], p[1])).collect();
        }
        Ok(ShapeGeometry { units })
    }

    fn serialize(&self, geometry: &ShapeGeometry) -> Result<Vec<u8>> {
        let mut out = String::new();
        for unit in &geometry.units {
            out.push_str("c\r\n");
            match &unit.style {
                Style::Solid { color } => {
                    let (c0, c1, c2, c3) = unpack_color(*color);
                    out.push_str(&format!("s s:{c0}:{c1}:{c2}:{c3}\r\n"));
                }
                Style::Line { color, width } => {
                    let (c0, c1, c2, c3) = unpack_color(*color);
                    out.push_str(&format!("s l:{}:{c0}:{c1}:{c2}:{c3}\r\n", fmt_f32(*width)));
                }
                Style::Textured {
                    color,
                    bitmap_character_id,
                    matrix,
                    ..
                } => {
                    let (c0, c1, c2, c3) = unpack_color(*color);
                    out.push_str(&format!(
                        "s tc:{c0}:{c1}:{c2}:{c3}:{bitmapId}:{m0}:{m1}:{m2}:{m3}:{tx}:{ty}\r\n",
                        bitmapId = bitmap_character_id,
                        m0 = matrix[0],
                        m1 = matrix[1],
                        m2 = matrix[2],
                        m3 = matrix[3],
                        tx = matrix[4],
                        ty = matrix[5],
                    ));
                }
            }
            if !unit.vertices.is_empty() {
                let mut toks = Vec::with_capacity(unit.vertices.len() * 2);
                for &(x, y) in &unit.vertices {
                    toks.push(fmt_f32(x));
                    toks.push(fmt_f32(y));
                }
                let cmd = if matches!(unit.style, Style::Line { .. }) {
                    "l"
                } else {
                    "t"
                };
                out.push_str(cmd);
                out.push(' ');
                out.push_str(&toks.join(":"));
                out.push_str("\r\n");
            }
        }
        Ok(out.into_bytes())
    }

    fn path_for(&self, base: &Path, shape_index: u32) -> PathBuf {
        let dir = format!(
            "{}_geometry",
            base.file_name().and_then(|n| n.to_str()).unwrap_or("apt")
        );
        base.with_file_name(dir).join(format!("{shape_index}.ru"))
    }
}

fn parse_style(spec: &str) -> Result<Style> {
    let mut f = spec.split(':');
    match f.next() {
        Some("s") => {
            let (c0, c1, c2, c3) = parse_color(&mut f)?;
            Ok(Style::Solid {
                color: pack_color(c0, c1, c2, c3),
            })
        }
        Some("l") => {
            let width: f32 = f
                .next()
                .ok_or_else(|| err("missing line width"))?
                .parse()
                .map_err(|_| err("bad line width"))?;
            let (c0, c1, c2, c3) = parse_color(&mut f)?;
            Ok(Style::Line {
                color: pack_color(c0, c1, c2, c3),
                width,
            })
        }
        Some("tc") | Some("tw") => {
            let clipped = spec.starts_with("tc");
            let (c0, c1, c2, c3) = parse_color(&mut f)?;
            let bitmap_character_id: u32 = f
                .next()
                .ok_or_else(|| err("missing bitmap id"))?
                .parse()
                .map_err(|_| err("bad bitmap id"))?;
            let mut matrix = [0f32; 6];
            for m in &mut matrix {
                *m = f
                    .next()
                    .ok_or_else(|| err("missing matrix cell"))?
                    .parse()
                    .map_err(|_| err("bad matrix cell"))?;
            }
            Ok(Style::Textured {
                color: pack_color(c0, c1, c2, c3),
                bitmap_character_id,
                matrix,
                clipped,
            })
        }
        other => Err(err(&format!("unknown style tag {other:?}"))),
    }
}

fn parse_color<'a>(f: &mut impl Iterator<Item = &'a str>) -> Result<(u8, u8, u8, u8)> {
    let mut c = [0u8; 4];
    for b in &mut c {
        *b = f
            .next()
            .ok_or_else(|| err("missing color byte"))?
            .parse()
            .map_err(|_| err("bad color byte"))?;
    }
    Ok((c[0], c[1], c[2], c[3]))
}

fn fmt_f32(v: f32) -> String {
    if v == v.trunc() && v.is_finite() {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

fn err(msg: &str) -> Error {
    Error::Geometry(msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_solid_and_textured() {
        let src = "c\r\ns s:10:20:30:40\r\nt 0:0:10:0:10:10\r\nc\r\ns tc:1:2:3:4:7:1:0:0:1:5:6\r\nt 0:0:2:0:2:2\r\n";
        let g = RuFormat.parse(src.as_bytes()).unwrap();
        assert_eq!(g.units.len(), 2);
        match &g.units[0].style {
            Style::Solid { color } => assert_eq!(*color, 10 | 20 << 8 | 30 << 16 | 40 << 24),
            _ => panic!("expected solid"),
        }
        match &g.units[1].style {
            Style::Textured {
                bitmap_character_id,
                matrix,
                ..
            } => {
                assert_eq!(*bitmap_character_id, 7);
                assert_eq!(*matrix, [1.0, 0.0, 0.0, 1.0, 5.0, 6.0]);
            }
            _ => panic!("expected textured"),
        }
        assert_eq!(
            g.units[0].vertices,
            vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)]
        );

        // Re-serialize and re-parse: structure is stable.
        let out = RuFormat.serialize(&g).unwrap();
        let g2 = RuFormat.parse(&out).unwrap();
        assert_eq!(g, g2);
    }

    #[test]
    fn roundtrip_line_style() {
        let src =
            "c\r\ns l:1.5:131:83:39:255\r\nl 365.5:-73.5:370.5:-73.5:370.5:199.5:365.5:199.5\r\n";
        let g = RuFormat.parse(src.as_bytes()).unwrap();
        assert_eq!(g.units.len(), 1);
        match &g.units[0].style {
            Style::Line { color, width } => {
                assert_eq!(*color, 131 | 83 << 8 | 39 << 16 | 255 << 24);
                assert_eq!(*width, 1.5);
            }
            other => panic!("expected line, got {other:?}"),
        }
        assert_eq!(
            g.units[0].vertices,
            vec![
                (365.5, -73.5),
                (370.5, -73.5),
                (370.5, 199.5),
                (365.5, 199.5)
            ]
        );

        let out = RuFormat.serialize(&g).unwrap();
        let out_str = String::from_utf8_lossy(&out);
        assert!(
            out_str.contains("\r\nl "),
            "line vertices must use the 'l' command, got: {out_str:?}"
        );
        let g2 = RuFormat.parse(&out).unwrap();
        assert_eq!(g, g2);
    }
}
