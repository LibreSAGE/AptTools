fn main() {
    let base = std::path::PathBuf::from(std::env::args().nth(1).unwrap());
    let f = apt::AptFile::load(&base.with_extension("apt")).unwrap();
    for (i, slot) in f.movie.characters.iter().enumerate() {
        if let Some(apt::Character::Button(b)) = slot.as_character() {
            let bb = &b.hit_test_bounds;
            let (mut xmin, mut ymin, mut xmax, mut ymax) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
            for &(x, y) in &b.hit_test_vertices {
                xmin = xmin.min(x);
                xmax = xmax.max(x);
                ymin = ymin.min(y);
                ymax = ymax.max(y);
            }
            println!("[{i:>3}] bounds=({:.0},{:.0})-({:.0},{:.0}) mesh: {} verts {} tris bbox=({xmin:.0},{ymin:.0})-({xmax:.0},{ymax:.0})",
                bb.left, bb.top, bb.right, bb.bottom, b.hit_test_vertices.len(), b.hit_test_triangles.len());
            for r in &b.records {
                println!(
                    "      rec states={:#x} char={} matrix=({},{},{},{})",
                    r.states, r.character_id, r.matrix.a, r.matrix.d, r.matrix.tx, r.matrix.ty
                );
            }
            for a in &b.actions {
                print!("      cond={:#x}:", a.conditions);
                for ins in a.actions.instructions.iter().take(2) {
                    print!(" {ins:?};");
                }
                println!();
            }
        }
    }
}
