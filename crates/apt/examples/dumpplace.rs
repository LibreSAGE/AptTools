//! Throwaway: dump root-frame PlaceObject transforms.
use apt::{CharacterSlot, Control};

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let base = apt::base_path(std::path::Path::new(&path));
    let apt_bytes = std::fs::read(base.with_extension("apt")).unwrap();
    let const_bytes = std::fs::read(base.with_extension("const")).unwrap();
    let file = apt::AptFile::read(&apt_bytes, &const_bytes).unwrap();
    println!(
        "stage {}x{}  chars {}  frames {}",
        file.movie.width,
        file.movie.height,
        file.movie.characters.len(),
        file.movie.frames.len()
    );
    // Which character to dump: root frames, or a specific sprite index.
    let target: Option<usize> = std::env::args().nth(2).and_then(|s| s.parse().ok());
    let frames: Vec<&apt::Frame> = match target {
        None => file.movie.frames.iter().collect(),
        Some(idx) => match &file.movie.characters[idx] {
            CharacterSlot::Character(apt::Character::Sprite(s)) => s.frames.iter().collect(),
            other => {
                println!("char {idx} is {:?}", other.as_ref());
                vec![]
            }
        },
    };
    for (fi, frame) in frames.iter().enumerate() {
        for c in &frame.controls {
            if let Control::PlaceObject(p) = c {
                let m = p
                    .matrix
                    .map(|m| {
                        format!(
                            "a{:.3} b{:.3} c{:.3} d{:.3} tx{:.1} ty{:.1}",
                            m.a, m.b, m.c, m.d, m.tx, m.ty
                        )
                    })
                    .unwrap_or("none".into());
                let ty = p
                    .character_id
                    .and_then(|id| file.movie.characters.get(id as usize))
                    .map(|s| match s {
                        CharacterSlot::Root => "Root",
                        CharacterSlot::Empty => "Empty",
                        CharacterSlot::Character(ch) => ch.type_name(),
                    })
                    .unwrap_or("?");
                println!(
                    "f{fi} depth{} char{:?}({ty}) clip{:?} move{} [{m}]",
                    p.depth, p.character_id, p.clip_depth, p.is_move
                );
            } else {
                println!("f{fi} {:?}", std::mem::discriminant(c));
            }
        }
    }
}

trait AsRefSlot {
    fn as_ref(&self) -> String;
}
impl AsRefSlot for CharacterSlot {
    fn as_ref(&self) -> String {
        match self {
            CharacterSlot::Root => "Root".into(),
            CharacterSlot::Empty => "Empty".into(),
            CharacterSlot::Character(c) => c.type_name().into(),
        }
    }
}
