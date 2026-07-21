//! Core library for EA's APT file format — a SWF-derived UI format used by
//! EA games (C&C Generals, BFME, C&C3, RA3, ...).
//!
//! An APT "movie" is a family of loose files sharing a base name:
//! - `<base>.apt`   — memory image of characters/frames/actions (pointer slots hold file offsets)
//! - `<base>.const` — constant table + entry point (offset of the root character in the `.apt`)
//! - `<base>.dat`   — bitmap character ID -> texture ID mapping (text)
//! - `<base>_geometry/<id>.ru` — per-shape geometry (text)
//!
//! The binary layout depends on the pointer size the file was built for
//! (4 or 8 bytes) and on whether it targets decoupled rendering; both are
//! declared in the `.apt` header tag `"Apt Data:<decoupled>:<swfver>:<ptrsize>"`.

pub mod actions;
pub mod constfile;
pub mod error;
mod io;
pub mod parse;
pub mod types;
pub mod write;

pub use constfile::ConstFile;
pub use error::Error;
pub use types::*;

pub type Result<T> = std::result::Result<T, Error>;

use std::path::{Path, PathBuf};

/// A fully loaded APT movie: parsed `.apt` + `.const` pair.
#[derive(Debug, Clone, PartialEq)]
pub struct AptFile {
    /// Header info as found in / destined for the `.apt` tag.
    pub header: Header,
    /// The 20-byte `.const` magic/header block, preserved for faithful re-emit.
    pub const_magic: [u8; 20],
    /// The root character (always an Animation / movie).
    pub movie: Movie,
}

impl AptFile {
    /// Parse an APT movie from raw `.apt` and `.const` bytes.
    pub fn read(apt_data: &[u8], const_data: &[u8]) -> Result<AptFile> {
        parse::parse(apt_data, const_data)
    }

    /// Serialize to (`.apt` bytes, `.const` bytes) for the given target layout.
    pub fn write(&self, options: &write::WriteOptions) -> Result<(Vec<u8>, Vec<u8>)> {
        write::write(self, options)
    }

    /// Load from `<base>.apt` + `<base>.const`. `path` may be either file or the base name.
    pub fn load(path: &Path) -> Result<AptFile> {
        let base = base_path(path);
        let apt_path = base.with_extension("apt");
        let const_path = base.with_extension("const");
        let apt_data = std::fs::read(&apt_path).map_err(|source| Error::ReadFile {
            path: apt_path,
            source,
        })?;
        let const_data = std::fs::read(&const_path).map_err(|source| Error::ReadFile {
            path: const_path,
            source,
        })?;
        Self::read(&apt_data, &const_data).map_err(|source| Error::Parse {
            base,
            source: Box::new(source),
        })
    }
}

/// Strip a `.apt`/`.const` extension to get the movie base path.
pub fn base_path(path: &Path) -> PathBuf {
    match path.extension().and_then(|e| e.to_str()) {
        Some("apt") | Some("const") => path.with_extension(""),
        _ => path.to_path_buf(),
    }
}
