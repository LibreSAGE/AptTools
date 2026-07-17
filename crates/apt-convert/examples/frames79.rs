fn main() {
    let base = std::path::PathBuf::from(std::env::args().nth(1).unwrap());
    let idx: usize = std::env::args().nth(2).unwrap().parse().unwrap();
    let f = apt::AptFile::load(&base.with_extension("apt")).unwrap();
    let Some(apt::Character::Sprite(sp)) = f.movie.characters[idx].as_character() else {
        return;
    };
    for (fi, fr) in sp.frames.iter().enumerate() {
        for c in &fr.controls {
            match c {
                apt::Control::PlaceObject(p) => {
                    if let Some(cid) = p.character_id {
                        let kind = f
                            .movie
                            .characters
                            .get(cid as usize)
                            .and_then(|s| s.as_character())
                            .map(|c| c.type_name())
                            .unwrap_or("import");
                        println!("f{fi:>2} place depth={} char={cid} [{kind}]", p.depth);
                    }
                }
                apt::Control::RemoveObject { depth } => println!("f{fi:>2} remove depth={depth}"),
                apt::Control::FrameLabel(l) => println!("f{fi:>2} LABEL {l:?}"),
                _ => {}
            }
        }
    }
}
