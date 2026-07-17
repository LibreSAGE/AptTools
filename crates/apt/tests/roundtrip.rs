//! Correctness tests against the real game corpus (if present).
//!
//! Byte-exact reproduction of the original files is *not* a goal: the classic
//! `swfc` compiler used its own allocation order (arrays and strings first, the
//! root Animation partway into the blob, a 12-byte header). We instead verify
//! the two properties that actually matter:
//!
//! 1. **Semantic round-trip**: `parse(write(parse(x))) == parse(x)`. Proves the
//!    reader and writer agree on every field and the written blob is internally
//!    consistent (all offsets resolve, alignment is right).
//! 2. **Constant-index sequencing**: the engine asserts at load that each Push
//!    item's constant index equals a per-file counter incremented in resolve
//!    order. We confirm that parsing an original file yields Push indices in
//!    strictly increasing 0,1,2,... order under our traversal — i.e. our
//!    traversal matches the engine's resolve order, so the tables we write are
//!    correctly sequenced.

use std::path::{Path, PathBuf};

use apt::write::WriteOptions;
use apt::AptFile;

fn corpus_root() -> Option<PathBuf> {
    let p = PathBuf::from("/home/stephan/Devel/APT");
    p.is_dir().then_some(p)
}

fn find_apt_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            find_apt_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("apt")
            && path.with_extension("const").is_file()
        {
            out.push(path);
        }
    }
}

#[test]
fn semantic_roundtrip_corpus() {
    let Some(root) = corpus_root() else {
        eprintln!("corpus not present; skipping");
        return;
    };
    let mut files = Vec::new();
    find_apt_files(&root, &mut files);
    assert!(
        !files.is_empty(),
        "no .apt files found under {}",
        root.display()
    );

    let mut ok = 0usize;
    let mut failures = Vec::new();

    for path in &files {
        let apt_data = std::fs::read(path).unwrap();
        let const_data = std::fs::read(path.with_extension("const")).unwrap();

        let a = match AptFile::read(&apt_data, &const_data) {
            Ok(f) => f,
            Err(e) => {
                failures.push(format!("{}: parse: {e}", path.display()));
                continue;
            }
        };
        let opts = WriteOptions::from_header(&a.header);
        let (out_apt, out_const) = match a.write(&opts) {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("{}: write: {e}", path.display()));
                continue;
            }
        };
        let b = match AptFile::read(&out_apt, &out_const) {
            Ok(f) => f,
            Err(e) => {
                failures.push(format!("{}: reparse: {e}", path.display()));
                continue;
            }
        };
        if a != b {
            failures.push(format!(
                "{}: model mismatch after round-trip",
                path.display()
            ));
            continue;
        }

        // Constant-index sequencing: our write-walk order must match the
        // engine's resolve order, which the original file encodes. Compare the
        // constant tables in file order — equality proves our ordering.
        use apt::constfile::ConstFile;
        let orig = ConstFile::read(&const_data, a.header.ptr_size).unwrap();
        let ours = ConstFile::read(&out_const, a.header.ptr_size).unwrap();
        if orig.constants != ours.constants {
            failures.push(format!(
                "{}: constant table order differs ({} vs {} entries)",
                path.display(),
                orig.constants.len(),
                ours.constants.len()
            ));
            continue;
        }
        ok += 1;
    }

    eprintln!("semantic round-trip: {}/{} ok", ok, files.len());
    for f in failures.iter().take(30) {
        eprintln!("  FAIL {f}");
    }
    assert!(
        failures.is_empty(),
        "{} files failed semantic round-trip",
        failures.len()
    );
}

/// Writing an already-parsed file, then writing again, must be byte-stable
/// (our own layout is deterministic — a weaker but exact guarantee).
#[test]
fn write_is_deterministic() {
    let Some(root) = corpus_root() else { return };
    let mut files = Vec::new();
    find_apt_files(&root, &mut files);
    let mut checked = 0;
    for path in files.iter().take(50) {
        let apt_data = std::fs::read(path).unwrap();
        let const_data = std::fs::read(path.with_extension("const")).unwrap();
        let Ok(a) = AptFile::read(&apt_data, &const_data) else {
            continue;
        };
        let opts = WriteOptions::from_header(&a.header);
        let first = a.write(&opts).unwrap();
        let second = a.write(&opts).unwrap();
        assert_eq!(
            first,
            second,
            "non-deterministic write for {}",
            path.display()
        );
        checked += 1;
    }
    eprintln!("deterministic write: {checked} files");
}
