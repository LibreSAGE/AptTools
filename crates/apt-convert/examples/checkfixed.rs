use swf::*;
fn fillmat(m: [f32; 6]) -> Option<Matrix> {
    let (a, b, c, d, tx, ty) = (m[0], m[1], m[2], m[3], m[4], m[5]);
    let det = a * d - b * c;
    if !det.is_finite() || det.abs() < 1e-12 {
        return None;
    }
    let (i00, i01) = (d / det, -b / det);
    let (i10, i11) = (-c / det, a / det);
    let trans_x = -(i00 * tx + i01 * ty);
    let trans_y = -(i10 * tx + i11 * ty);
    let scale = [i00, i10, i01, i11].map(|v| v as f64 * 20.0);
    if scale.iter().any(|v| !v.is_finite() || v.abs() >= 32768.0)
        || !trans_x.is_finite()
        || !trans_y.is_finite()
        || trans_x.abs() >= 5_000_000.0
        || trans_y.abs() >= 5_000_000.0
    {
        return None;
    }
    Some(Matrix {
        a: Fixed16::from_f64(scale[0]),
        b: Fixed16::from_f64(scale[1]),
        c: Fixed16::from_f64(scale[2]),
        d: Fixed16::from_f64(scale[3]),
        tx: Twips::from_pixels(trans_x as f64),
        ty: Twips::from_pixels(trans_y as f64),
    })
}
fn try_write_matrix(m: Matrix) -> String {
    // Wrap in a minimal DefineShape using this as a bitmap fill.
    let px = |v: f64| Twips::from_pixels(v);
    let shape = Shape {
        version: 3,
        id: 1,
        shape_bounds: Rectangle {
            x_min: px(0.0),
            x_max: px(10.0),
            y_min: px(0.0),
            y_max: px(10.0),
        },
        edge_bounds: Rectangle {
            x_min: px(0.0),
            x_max: px(10.0),
            y_min: px(0.0),
            y_max: px(10.0),
        },
        flags: ShapeFlag::empty(),
        styles: ShapeStyles {
            fill_styles: vec![FillStyle::Bitmap {
                id: 2,
                matrix: m,
                is_smoothed: true,
                is_repeating: false,
            }],
            line_styles: vec![],
        },
        shape: vec![
            ShapeRecord::StyleChange(Box::new(StyleChangeData {
                move_to: Some(Point::new(px(0.0), px(0.0))),
                fill_style_0: None,
                fill_style_1: Some(1),
                line_style: None,
                new_styles: None,
            })),
            ShapeRecord::StraightEdge {
                delta: PointDelta {
                    dx: px(10.0),
                    dy: Twips::ZERO,
                },
            },
            ShapeRecord::StraightEdge {
                delta: PointDelta {
                    dx: Twips::ZERO,
                    dy: px(10.0),
                },
            },
            ShapeRecord::StraightEdge {
                delta: PointDelta {
                    dx: px(-10.0),
                    dy: Twips::ZERO,
                },
            },
            ShapeRecord::StraightEdge {
                delta: PointDelta {
                    dx: Twips::ZERO,
                    dy: px(-10.0),
                },
            },
        ],
    };
    let header = Header {
        compression: Compression::None,
        version: 6,
        stage_size: Rectangle {
            x_min: Twips::ZERO,
            x_max: px(100.0),
            y_min: Twips::ZERO,
            y_max: px(100.0),
        },
        frame_rate: Fixed8::from_f64(30.0),
        num_frames: 1,
    };
    let mut out = Vec::new();
    match swf::write_swf(
        &header,
        &vec![Tag::DefineShape(shape), Tag::ShowFrame, Tag::End],
        &mut out,
    ) {
        Ok(_) => format!("OK {} bytes", out.len()),
        Err(e) => format!("FAIL: {e}"),
    }
}
fn check(m: [f32; 6], label: &str) {
    match fillmat(m) {
        Some(mm) => println!("{label}: matrix={mm:?} write={}", try_write_matrix(mm)),
        None => println!("{label}: rejected by guard (solid fallback)"),
    }
}
fn main() {
    check(
        [
            -0.080331, -0.000495, 0.211964, -0.000187, 195.337959, -0.556968,
        ],
        "35 unit0",
    );
    check(
        [
            -0.176978, 0.000218, -0.055828, -0.000692, 268.287863, -2.114762,
        ],
        "35 unit1",
    );
    check(
        [
            -0.000040,
            0.000993,
            -0.232458,
            -0.000000,
            -235.526086,
            0.852474,
        ],
        "102 unit0",
    );
    check(
        [
            0.000128,
            0.003748,
            -0.684263,
            0.000001,
            -692.838249,
            4.616418,
        ],
        "102 unit1",
    );
    check(
        [0.012498, 0.0, 0.0, 0.011168, 1016.892836, 468.90335],
        "102 unit2",
    );
}
