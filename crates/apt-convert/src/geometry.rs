//! Reconstruct triangulated fill/stroke geometry from a SWF shape's edges.
//!
//! SWF stores a shape as a flat list of edges (straight or quadratic-curve),
//! each carrying up to two fill-style indices (the style to its left/right,
//! per the SWF19 "two-sided" edge convention) and a line-style index. To get
//! back to APT's pre-tessellated triangle lists we have to:
//!
//! 1. Flatten curved edges to line segments ([`flatten_quadratic`]).
//! 2. Walk the edges per fill style into closed polygon contours
//!    ([`build_contours`]) — an edge contributes to `fill_style_1`'s contour
//!    in its own direction, and to `fill_style_0`'s contour reversed (SWF's
//!    left/right convention: reversing a directed edge swaps which side is
//!    "the right").
//! 3. Triangulate each fill's contours with a constrained Delaunay
//!    triangulation, then classify each resulting triangle as inside/outside
//!    the original (possibly multi-contour, holes-included) fill region —
//!    a CDT triangulates the convex hull of its input, it does not know
//!    which triangles are real fill vs. hole vs. exterior.
//!
//! Gradients aren't representable in `.ru` (solid/textured triangles only);
//! they're approximated by their first gradient stop's color.

use std::collections::HashMap;

use apt_aux::{RenderUnit, ShapeGeometry, Style};
use spade::{ConstrainedDelaunayTriangulation, Point2, Triangulation};
use swf::{Color, FillStyle, Matrix, Shape, ShapeFlag, ShapeRecord, ShapeStyles, Twips};

/// An exact (twips) point; `Twips` is integer-backed so shared edge endpoints
/// compare equal without float drift, which contour-chaining depends on.
type TPoint = (Twips, Twips);

/// One flattened straight segment tagged with the fill/line styles active
/// when it was drawn, and which style *table* (see `new_styles`) is active.
struct TaggedEdge {
    from: TPoint,
    to: TPoint,
    fill0: Option<u32>,
    fill1: Option<u32>,
    line: Option<u32>,
    generation: u32,
}

/// Extract per-fill and per-line-style triangle/segment geometry for a shape.
pub fn extract_shape_geometry(shape: &Shape) -> ShapeGeometry {
    let nonzero = shape.flags.contains(ShapeFlag::NON_ZERO_WINDING_RULE);
    let (edges, style_tables) = flatten_edges(&shape.shape, &shape.styles);

    let mut fill_edges: HashMap<(u32, u32), Vec<(TPoint, TPoint)>> = HashMap::new();
    let mut line_segments: HashMap<(u32, u32), Vec<(TPoint, TPoint)>> = HashMap::new();
    for e in &edges {
        if let Some(id) = e.fill1 {
            if id > 0 {
                fill_edges
                    .entry((e.generation, id))
                    .or_default()
                    .push((e.from, e.to));
            }
        }
        if let Some(id) = e.fill0 {
            if id > 0 {
                // Reversed: fill_style_0 is to the *left* of travel, i.e. to
                // the right when the edge is walked backwards.
                fill_edges
                    .entry((e.generation, id))
                    .or_default()
                    .push((e.to, e.from));
            }
        }
        if let Some(id) = e.line {
            if id > 0 {
                line_segments
                    .entry((e.generation, id))
                    .or_default()
                    .push((e.from, e.to));
            }
        }
    }

    let mut units = Vec::new();

    let mut fill_keys: Vec<_> = fill_edges.keys().copied().collect();
    fill_keys.sort_unstable();
    for key @ (generation, style_index) in fill_keys {
        let directed = &fill_edges[&key];
        let contours = build_contours(directed);
        if contours.is_empty() {
            continue;
        }
        let triangles = triangulate_fill(&contours, nonzero);
        if triangles.is_empty() {
            continue;
        }
        let fill_style = style_tables[generation as usize]
            .fill_styles
            .get(style_index as usize - 1);
        let style = style_for_fill(fill_style);
        let mut vertices = Vec::with_capacity(triangles.len() * 3);
        for tri in triangles {
            vertices.extend_from_slice(&tri);
        }
        units.push(RenderUnit { style, vertices });
    }

    let mut line_keys: Vec<_> = line_segments.keys().copied().collect();
    line_keys.sort_unstable();
    for key @ (generation, style_index) in line_keys {
        let segments = &line_segments[&key];
        let line_style = style_tables[generation as usize]
            .line_styles
            .get(style_index as usize - 1);
        let (color, width) = match line_style {
            Some(ls) => {
                let color = match ls.fill_style() {
                    FillStyle::Color(c) => pack_color(c),
                    other => style_for_fill(Some(other)).fallback_color(),
                };
                (color, ls.width().to_pixels() as f32)
            }
            None => (0xFF000000, 1.0),
        };
        let mut vertices = Vec::with_capacity(segments.len() * 2);
        for (a, b) in segments {
            vertices.push((a.0.to_pixels() as f32, a.1.to_pixels() as f32));
            vertices.push((b.0.to_pixels() as f32, b.1.to_pixels() as f32));
        }
        units.push(RenderUnit {
            style: Style::Line { color, width },
            vertices,
        });
    }

    ShapeGeometry { units }
}

impl StyleExt for Style {
    fn fallback_color(&self) -> u32 {
        match self {
            Style::Solid { color } | Style::Textured { color, .. } | Style::Line { color, .. } => {
                *color
            }
        }
    }
}
trait StyleExt {
    fn fallback_color(&self) -> u32;
}

fn style_for_fill(fill_style: Option<&FillStyle>) -> Style {
    match fill_style {
        Some(FillStyle::Bitmap {
            id,
            matrix,
            is_repeating,
            ..
        }) => match texture_matrix_from_swf_bitmap(matrix) {
            Some(m) => Style::Textured {
                color: 0xFFFF_FFFF,
                bitmap_character_id: *id as u32,
                matrix: m,
                clipped: !is_repeating,
            },
            None => Style::Solid { color: 0xFFFF_FFFF },
        },
        Some(FillStyle::Color(c)) => Style::Solid {
            color: pack_color(c),
        },
        // Gradients have no `.ru` equivalent; approximate with the first stop.
        Some(FillStyle::LinearGradient(g) | FillStyle::RadialGradient(g)) => Style::Solid {
            color: g
                .records
                .first()
                .map(|r| pack_color(&r.color))
                .unwrap_or(0xFFFF_FFFF),
        },
        Some(FillStyle::FocalGradient { gradient, .. }) => Style::Solid {
            color: gradient
                .records
                .first()
                .map(|r| pack_color(&r.color))
                .unwrap_or(0xFFFF_FFFF),
        },
        None => Style::Solid { color: 0xFFFF_FFFF },
    }
}

fn pack_color(c: &Color) -> u32 {
    u32::from_le_bytes([c.b, c.g, c.r, c.a])
}

/// Walk a shape's records, tracking the cursor and active fill/line styles,
/// flattening curves, and stamping each atomic straight segment with the
/// styles/table-generation active when it was drawn.
fn flatten_edges(
    records: &[ShapeRecord],
    styles: &ShapeStyles,
) -> (Vec<TaggedEdge>, Vec<ShapeStyles>) {
    let mut edges = Vec::new();
    let mut generations = vec![styles.clone()];
    let mut generation = 0u32;
    let mut cursor: TPoint = (Twips::ZERO, Twips::ZERO);
    let mut fill0: Option<u32> = None;
    let mut fill1: Option<u32> = None;
    let mut line: Option<u32> = None;

    for record in records {
        match record {
            ShapeRecord::StyleChange(data) => {
                if let Some(mv) = data.move_to {
                    cursor = (mv.x, mv.y);
                }
                if let Some(ns) = &data.new_styles {
                    generations.push(ns.clone());
                    generation += 1;
                    fill0 = None;
                    fill1 = None;
                    line = None;
                }
                if let Some(f0) = data.fill_style_0 {
                    fill0 = Some(f0);
                }
                if let Some(f1) = data.fill_style_1 {
                    fill1 = Some(f1);
                }
                if let Some(l) = data.line_style {
                    line = Some(l);
                }
            }
            ShapeRecord::StraightEdge { delta } => {
                let next = (cursor.0 + delta.dx, cursor.1 + delta.dy);
                edges.push(TaggedEdge {
                    from: cursor,
                    to: next,
                    fill0,
                    fill1,
                    line,
                    generation,
                });
                cursor = next;
            }
            ShapeRecord::CurvedEdge {
                control_delta,
                anchor_delta,
            } => {
                let control = (cursor.0 + control_delta.dx, cursor.1 + control_delta.dy);
                let anchor = (control.0 + anchor_delta.dx, control.1 + anchor_delta.dy);
                for (a, b) in flatten_quadratic(cursor, control, anchor) {
                    edges.push(TaggedEdge {
                        from: a,
                        to: b,
                        fill0,
                        fill1,
                        line,
                        generation,
                    });
                }
                cursor = anchor;
            }
        }
    }
    (edges, generations)
}

/// Flatten a quadratic Bezier (start `p0`, control `p1`, end `p2`) to line
/// segments via adaptive De Casteljau subdivision.
fn flatten_quadratic(p0: TPoint, p1: TPoint, p2: TPoint) -> Vec<(TPoint, TPoint)> {
    let mut points = Vec::new();
    subdivide(to_f64(p0), to_f64(p1), to_f64(p2), 0, &mut points);
    let mut segs = Vec::with_capacity(points.len() + 1);
    let mut prev = p0;
    for pt in points {
        let cur = from_f64(pt);
        if cur != prev {
            segs.push((prev, cur));
            prev = cur;
        }
    }
    if prev != p2 {
        segs.push((prev, p2));
    }
    segs
}

/// Max deviation of a quadratic Bezier from its chord, in twips (1/20 px).
const FLATNESS_TOLERANCE_TWIPS: f64 = 4.0;
const MAX_SUBDIVISION_DEPTH: u32 = 10;

fn subdivide(
    p0: (f64, f64),
    p1: (f64, f64),
    p2: (f64, f64),
    depth: u32,
    out: &mut Vec<(f64, f64)>,
) {
    let dx = p0.0 - 2.0 * p1.0 + p2.0;
    let dy = p0.1 - 2.0 * p1.1 + p2.1;
    let deviation = 0.25 * (dx * dx + dy * dy).sqrt();
    if depth >= MAX_SUBDIVISION_DEPTH || deviation <= FLATNESS_TOLERANCE_TWIPS {
        out.push(p2);
        return;
    }
    let p01 = mid(p0, p1);
    let p12 = mid(p1, p2);
    let p012 = mid(p01, p12);
    subdivide(p0, p01, p012, depth + 1, out);
    subdivide(p012, p12, p2, depth + 1, out);
}

fn mid(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
    ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0)
}

fn to_f64(p: TPoint) -> (f64, f64) {
    (p.0.get() as f64, p.1.get() as f64)
}

fn from_f64(p: (f64, f64)) -> TPoint {
    (
        Twips::new(p.0.round() as i32),
        Twips::new(p.1.round() as i32),
    )
}

/// Chain directed edges sharing endpoints into closed polygon loops. Multiple
/// independent loops (e.g. an outer boundary plus holes, or disjoint
/// sub-shapes sharing one fill style) are all returned; a well-formed SWF
/// shape has every edge's end point matched by exactly one other edge's
/// start point, so this always closes back to the loop's start.
fn build_contours(directed: &[(TPoint, TPoint)]) -> Vec<Vec<TPoint>> {
    let mut by_start: HashMap<TPoint, Vec<usize>> = HashMap::new();
    for (i, (from, _)) in directed.iter().enumerate() {
        by_start.entry(*from).or_default().push(i);
    }
    let mut used = vec![false; directed.len()];
    let mut contours = Vec::new();
    for start_idx in 0..directed.len() {
        if used[start_idx] {
            continue;
        }
        let loop_start = directed[start_idx].0;
        let mut contour = Vec::new();
        let mut idx = start_idx;
        loop {
            used[idx] = true;
            let (from, to) = directed[idx];
            contour.push(from);
            if to == loop_start {
                break;
            }
            let next = by_start
                .get(&to)
                .and_then(|v| v.iter().find(|&&j| !used[j]).copied());
            match next {
                Some(j) => idx = j,
                None => break, // open/degenerate chain from malformed input; keep the partial loop
            }
        }
        if contour.len() >= 3 {
            contours.push(contour);
        }
    }
    contours
}

/// Triangulate a fill's (possibly multiple, possibly holed) contours with a
/// constrained Delaunay triangulation, keeping only the triangles that fall
/// inside the fill region under the shape's winding rule.
fn triangulate_fill(contours_twips: &[Vec<TPoint>], nonzero: bool) -> Vec<[(f32, f32); 3]> {
    let mut cdt: ConstrainedDelaunayTriangulation<Point2<f64>> =
        ConstrainedDelaunayTriangulation::new();
    let mut handles: HashMap<TPoint, spade::handles::FixedVertexHandle> = HashMap::new();
    let mut contours_px: Vec<Vec<(f64, f64)>> = Vec::new();

    for contour in contours_twips {
        let mut px_contour = Vec::with_capacity(contour.len());
        let mut contour_handles = Vec::with_capacity(contour.len());
        for &pt in contour {
            let px = (pt.0.to_pixels(), pt.1.to_pixels());
            px_contour.push(px);
            let handle = *handles.entry(pt).or_insert_with(|| {
                cdt.insert(Point2::new(px.0, px.1))
                    .expect("finite contour coordinate")
            });
            contour_handles.push(handle);
        }
        for i in 0..contour_handles.len() {
            let a = contour_handles[i];
            let b = contour_handles[(i + 1) % contour_handles.len()];
            if a != b {
                cdt.add_constraint(a, b);
            }
        }
        contours_px.push(px_contour);
    }

    let mut triangles = Vec::new();
    for face in cdt.inner_faces() {
        let verts = face.vertices();
        let pts: Vec<(f64, f64)> = verts
            .iter()
            .map(|v| v.position())
            .map(|p| (p.x, p.y))
            .collect();
        let centroid = (
            (pts[0].0 + pts[1].0 + pts[2].0) / 3.0,
            (pts[0].1 + pts[1].1 + pts[2].1) / 3.0,
        );
        if point_in_contours(centroid, &contours_px, nonzero) {
            triangles.push([
                (pts[0].0 as f32, pts[0].1 as f32),
                (pts[1].0 as f32, pts[1].1 as f32),
                (pts[2].0 as f32, pts[2].1 as f32),
            ]);
        }
    }
    triangles
}

/// Even-odd or non-zero-winding point-in-polygon test against a set of
/// contours (holes are just contours with opposite winding, handled
/// naturally by either rule).
fn point_in_contours(pt: (f64, f64), contours: &[Vec<(f64, f64)>], nonzero: bool) -> bool {
    let mut winding = 0i32;
    for c in contours {
        for i in 0..c.len() {
            let (x0, y0) = c[i];
            let (x1, y1) = c[(i + 1) % c.len()];
            if (y0 <= pt.1) != (y1 <= pt.1) {
                let t = (pt.1 - y0) / (y1 - y0);
                let x_cross = x0 + t * (x1 - x0);
                if x_cross > pt.0 {
                    winding += if y1 > y0 { 1 } else { -1 };
                }
            }
        }
    }
    if nonzero {
        winding != 0
    } else {
        winding.rem_euclid(2) != 0
    }
}

/// Convert a SWF bitmap-fill matrix (bitmap space in twips-equivalent units
/// -> shape space in twips) into the `.ru` convention: a matrix taking a
/// vertex position *in pixels* to texture *pixels*
/// (`u = a*x + b*y + tx`, `v = c*x + d*y + ty`). This is the algebraic
/// inverse of `to_swf::bitmap_fill_matrix`, which derives a SWF matrix from
/// exactly this form.
pub(crate) fn texture_matrix_from_swf_bitmap(m: &Matrix) -> Option<[f32; 6]> {
    // SWF: shape_px = [[a,c],[b,d]] * (bitmap_unit/20) + (tx_px, ty_px).
    let a = m.a.to_f32() / 20.0;
    let b = m.b.to_f32() / 20.0;
    let c = m.c.to_f32() / 20.0;
    let d = m.d.to_f32() / 20.0;
    let tx = m.tx.to_pixels() as f32;
    let ty = m.ty.to_pixels() as f32;

    let det = a * d - c * b;
    if !det.is_finite() || det.abs() < 1e-12 {
        return None;
    }
    let ia = d / det;
    let ic = -c / det;
    let ib = -b / det;
    let id = a / det;
    let itx = -(ia * tx + ic * ty);
    let ity = -(ib * tx + id * ty);
    if [ia, ic, ib, id, itx, ity].iter().any(|v| !v.is_finite()) {
        return None;
    }
    Some([ia, ic, ib, id, itx, ity])
}

#[cfg(test)]
mod tests {
    use super::*;
    use swf::{Fixed16, PointDelta, Rectangle, StyleChangeData};

    fn straight(dx: f64, dy: f64) -> ShapeRecord {
        ShapeRecord::StraightEdge {
            delta: PointDelta::new(Twips::from_pixels(dx), Twips::from_pixels(dy)),
        }
    }

    fn move_and_set(x: f64, y: f64, fill1: u32) -> ShapeRecord {
        ShapeRecord::StyleChange(Box::new(StyleChangeData {
            move_to: Some(swf::Point::new(
                Twips::from_pixels(x),
                Twips::from_pixels(y),
            )),
            fill_style_0: None,
            fill_style_1: Some(fill1),
            line_style: None,
            new_styles: None,
        }))
    }

    #[test]
    fn triangulates_a_square() {
        // A 10x10px square, wound so fill_style_1 (right of travel) is the
        // square's interior when walking clockwise in screen space (y down).
        let records = vec![
            move_and_set(0.0, 0.0, 1),
            straight(10.0, 0.0),
            straight(0.0, 10.0),
            straight(-10.0, 0.0),
            straight(0.0, -10.0),
        ];
        let shape = Shape {
            version: 3,
            id: 1,
            shape_bounds: Rectangle {
                x_min: Twips::ZERO,
                x_max: Twips::from_pixels(10.0),
                y_min: Twips::ZERO,
                y_max: Twips::from_pixels(10.0),
            },
            edge_bounds: Rectangle {
                x_min: Twips::ZERO,
                x_max: Twips::from_pixels(10.0),
                y_min: Twips::ZERO,
                y_max: Twips::from_pixels(10.0),
            },
            flags: ShapeFlag::empty(),
            styles: ShapeStyles {
                fill_styles: vec![FillStyle::Color(Color {
                    r: 255,
                    g: 0,
                    b: 0,
                    a: 255,
                })],
                line_styles: vec![],
            },
            shape: records,
        };
        let geom = extract_shape_geometry(&shape);
        assert_eq!(geom.units.len(), 1);
        let unit = &geom.units[0];
        assert!(matches!(unit.style, Style::Solid { .. }));
        // 2 triangles cover the square; total area must equal 100 px^2.
        assert_eq!(unit.vertices.len() % 3, 0);
        let mut area = 0.0f64;
        for tri in unit.vertices.chunks(3) {
            let (x0, y0) = (tri[0].0 as f64, tri[0].1 as f64);
            let (x1, y1) = (tri[1].0 as f64, tri[1].1 as f64);
            let (x2, y2) = (tri[2].0 as f64, tri[2].1 as f64);
            area += ((x1 - x0) * (y2 - y0) - (x2 - x0) * (y1 - y0)).abs() / 2.0;
        }
        assert!((area - 100.0).abs() < 1e-6, "expected area 100, got {area}");
    }

    #[test]
    fn ignores_unset_shape_record_flag_zero() {
        // A StyleChange with all-zero flags is the SWF end-of-shape sentinel
        // and the swf crate never emits it as a record (it returns None from
        // the reader); this just documents that `build_contours` copes with
        // an empty edge list.
        assert!(build_contours(&[]).is_empty());
    }

    #[test]
    fn bitmap_matrix_roundtrips_through_to_swf_inverse() {
        // Identity texture matrix (1:1 px-to-texel) must round-trip.
        let ru_matrix = [1.0f32, 0.0, 0.0, 1.0, 0.0, 0.0];
        let swf_matrix = Matrix {
            a: Fixed16::from_f64(20.0),
            b: Fixed16::from_f64(0.0),
            c: Fixed16::from_f64(0.0),
            d: Fixed16::from_f64(20.0),
            tx: Twips::ZERO,
            ty: Twips::ZERO,
        };
        let back = texture_matrix_from_swf_bitmap(&swf_matrix).unwrap();
        for (a, b) in ru_matrix.iter().zip(back.iter()) {
            assert!((a - b).abs() < 1e-3, "{ru_matrix:?} != {back:?}");
        }
    }
}
