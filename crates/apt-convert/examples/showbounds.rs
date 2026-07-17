fn main() {
    let base = std::path::PathBuf::from(std::env::args().nth(1).unwrap());
    let idx: usize = std::env::args().nth(2).unwrap().parse().unwrap();
    let f = apt::AptFile::load(&base.with_extension("apt")).unwrap();
    if let Some(apt::Character::Shape(s)) = f.movie.characters[idx].as_character() {
        println!("bounds: {:?}", s.bounds);
    }
}
