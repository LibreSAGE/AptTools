// Binary-search which triangle unit/vertex causes an unencodable shape.
use apt_aux::{GeometryFormat, Style};
use swf::*;

fn edge(from: (f32, f32), to: (f32, f32)) -> ShapeRecord {
    ShapeRecord::StraightEdge {
        delta: PointDelta {
            dx: Twips::from_pixels((to.0 - from.0) as f64),
            dy: Twips::from_pixels((to.1 - from.1) as f64),
        },
    }
}
const MAX_EDGE_PX: f32 = 65535.0 / 20.0;
fn edge_chain(from: (f32, f32), to: (f32, f32)) -> Vec<ShapeRecord> {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let steps = (dx.abs().max(dy.abs()) / MAX_EDGE_PX).ceil().max(1.0) as usize;
    let mut records = Vec::with_capacity(steps);
    let mut prev = from;
    for i in 1..=steps {
        let t = i as f32 / steps as f32;
        let next = (from.0 + dx * t, from.1 + dy * t);
        records.push(edge(prev, next));
        prev = next;
    }
    records
}

fn try_shape(verts: &[(f32, f32)]) -> std::result::Result<usize, String> {
    let mut records = vec![ShapeRecord::StyleChange(Box::new(StyleChangeData {
        move_to: Some(Point::new(
            Twips::from_pixels(verts[0].0 as f64),
            Twips::from_pixels(verts[0].1 as f64),
        )),
        fill_style_0: None,
        fill_style_1: Some(1),
        line_style: None,
        new_styles: None,
    }))];
    for tri in verts.chunks(3) {
        if tri.len() < 3 {
            continue;
        }
        records.extend(edge_chain(tri[0], tri[1]));
        records.extend(edge_chain(tri[1], tri[2]));
        records.extend(edge_chain(tri[2], tri[0]));
    }
    let (mut xmin, mut ymin, mut xmax, mut ymax) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for &(x, y) in verts {
        xmin = xmin.min(x);
        xmax = xmax.max(x);
        ymin = ymin.min(y);
        ymax = ymax.max(y);
    }
    let b = Rectangle {
        x_min: Twips::from_pixels(xmin as f64),
        x_max: Twips::from_pixels(xmax as f64),
        y_min: Twips::from_pixels(ymin as f64),
        y_max: Twips::from_pixels(ymax as f64),
    };
    let shape = Shape {
        version: 3,
        id: 1,
        shape_bounds: b.clone(),
        edge_bounds: b,
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
    let header = Header {
        compression: Compression::None,
        version: 6,
        stage_size: Rectangle {
            x_min: Twips::from_pixels(-5000.0),
            x_max: Twips::from_pixels(5000.0),
            y_min: Twips::from_pixels(-5000.0),
            y_max: Twips::from_pixels(5000.0),
        },
        frame_rate: Fixed8::from_f64(30.0),
        num_frames: 1,
    };
    let mut out = Vec::new();
    swf::write_swf(
        &header,
        &vec![Tag::DefineShape(shape), Tag::ShowFrame, Tag::End],
        &mut out,
    )
    .map(|_| out.len())
    .map_err(|e| e.to_string())
}

fn main() {
    let base = std::path::PathBuf::from(std::env::args().nth(1).unwrap());
    let idx: u32 = std::env::args().nth(2).unwrap().parse().unwrap();
    let g = apt_aux::ru::RuFormat.load(&base, idx).unwrap();
    for (ui, unit) in g.units.iter().enumerate() {
        if matches!(unit.style, Style::Line { .. }) {
            continue;
        }
        let tri_count = unit.vertices.len() / 3;
        for t in 0..tri_count {
            let tri = &unit.vertices[t * 3..t * 3 + 3];
            if let Err(e) = try_shape(tri) {
                println!("unit {ui} triangle {t}: FAILS: {e}  verts={tri:?}");
            }
        }
    }
    println!("done ({} units)", g.units.len());
}
