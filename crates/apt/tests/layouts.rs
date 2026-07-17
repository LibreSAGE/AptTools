//! Cross-layout consistency: the same movie serialized under a different
//! pointer size or the decoupled variant must re-parse to the same model.
//!
//! The corpus is entirely 32-bit coupled, so this is how we exercise the 8-byte
//! and decoupled writer/reader paths: take a real movie, re-emit it in the
//! target layout, and confirm the model survives (modulo the header/decoupled
//! shape fields, which the reader can only recover in decoupled mode).

use std::path::{Path, PathBuf};

use apt::write::WriteOptions;
use apt::{AptFile, PtrSize};

fn corpus_root() -> Option<PathBuf> {
    let p = PathBuf::from("/home/stephan/Devel/APT");
    p.is_dir().then_some(p)
}

fn find(root: &Path, out: &mut Vec<PathBuf>, limit: usize) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for e in entries.flatten() {
        if out.len() >= limit {
            return;
        }
        let p = e.path();
        if p.is_dir() {
            find(&p, out, limit);
        } else if p.extension().and_then(|e| e.to_str()) == Some("apt")
            && p.with_extension("const").is_file()
        {
            out.push(p);
        }
    }
}

/// Re-emit a 32-bit movie as 64-bit and confirm the model is preserved.
#[test]
fn reemit_as_64bit_preserves_model() {
    let Some(root) = corpus_root() else { return };
    let mut files = Vec::new();
    find(&root, &mut files, 60);

    let mut checked = 0;
    for path in &files {
        let apt = std::fs::read(path).unwrap();
        let cst = std::fs::read(path.with_extension("const")).unwrap();
        let Ok(a) = AptFile::read(&apt, &cst) else {
            continue;
        };
        assert_eq!(a.header.ptr_size, PtrSize::Four);

        let opts = WriteOptions::new(PtrSize::Eight, false, a.header.swf_version);
        let (out_apt, out_cst) = a.write(&opts).unwrap();
        let b = AptFile::read(&out_apt, &out_cst).unwrap();

        assert_eq!(b.header.ptr_size, PtrSize::Eight, "{}", path.display());
        // The movie model (characters, frames, actions, constants) is layout-
        // independent and must match exactly.
        assert_eq!(
            a.movie,
            b.movie,
            "movie changed re-emitting {} as 64-bit",
            path.display()
        );
        checked += 1;
    }
    assert!(checked > 0, "no files checked");
    eprintln!("re-emit as 64-bit: {checked} movies preserved");
}

/// Re-emit as the decoupled variant and confirm structural fields survive.
#[test]
fn reemit_as_decoupled_preserves_model() {
    let Some(root) = corpus_root() else { return };
    let mut files = Vec::new();
    find(&root, &mut files, 60);

    let mut checked = 0;
    for path in &files {
        let apt = std::fs::read(path).unwrap();
        let cst = std::fs::read(path.with_extension("const")).unwrap();
        let Ok(a) = AptFile::read(&apt, &cst) else {
            continue;
        };

        for ptr in [PtrSize::Four, PtrSize::Eight] {
            let opts = WriteOptions::new(ptr, true, a.header.swf_version);
            let (out_apt, out_cst) = a.write(&opts).unwrap();
            let b = AptFile::read(&out_apt, &out_cst).unwrap();
            assert!(b.header.decoupled, "{}", path.display());
            assert_eq!(b.header.ptr_size, ptr);
            // Coupled shapes have bitmap_character_id: None; the decoupled
            // reader fills in Some(0) for untextured shapes, so compare frames
            // and character count rather than the exact shape field.
            assert_eq!(a.movie.frames, b.movie.frames, "{}", path.display());
            assert_eq!(a.movie.characters.len(), b.movie.characters.len());
        }
        checked += 1;
    }
    assert!(checked > 0);
    eprintln!("re-emit as decoupled: {checked} movies");
}
