fn main() {
    let base = std::path::PathBuf::from(std::env::args().nth(1).unwrap());
    let f = apt::AptFile::load(&base.with_extension("apt")).unwrap();
    let scan = |frames: &[apt::Frame], who: &str| {
        for (fi, fr) in frames.iter().enumerate() {
            for c in &fr.controls {
                if let apt::Control::PlaceObject(p) = c {
                    if let Some(cid) = p.character_id {
                        if [2, 4, 5, 6, 8, 10, 11].contains(&cid) {
                            println!("{who} f{fi} places {cid}");
                        }
                    }
                }
            }
        }
    };
    scan(&f.movie.frames, "root");
    for (i, slot) in f.movie.characters.iter().enumerate() {
        if let Some(apt::Character::Sprite(sp)) = slot.as_character() {
            scan(&sp.frames, &format!("sprite {i}"));
        }
    }
    println!("(if nothing printed above, none of these bitmaps are ever placed directly)");
}
