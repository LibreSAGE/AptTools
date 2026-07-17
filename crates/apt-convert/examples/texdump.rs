use apt_convert::Assets;
fn main() {
    let base = std::path::PathBuf::from(std::env::args().nth(1).unwrap());
    let f = apt::AptFile::load(&base.with_extension("apt")).unwrap();
    let assets = apt_convert::DiskAssets::for_movie(&base, f.movie.characters.len(), true);
    for (i, slot) in f.movie.characters.iter().enumerate() {
        if matches!(slot.as_character(), Some(apt::Character::Bitmap)) {
            match assets.texture(i as u32, i as u32) {
                Some((key, t)) => println!("[{i}] tex key={key} {}x{}", t.width, t.height),
                None => println!("[{i}] no texture"),
            }
        }
    }
}
