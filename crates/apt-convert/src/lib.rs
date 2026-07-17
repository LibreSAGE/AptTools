//! Conversion between EA's APT format and standard Adobe SWF.
//!
//! - [`apt_to_swf`] builds a standard SWF (viewable in Ruffle or any Flash
//!   player) from a parsed [`apt::AptFile`] plus its auxiliary geometry.
//! - [`swf_to_apt`] parses a standard SWF into the APT model.
//!
//! The ActionScript bytecode bridge lives in [`bytecode`]: APT keeps SWF opcode
//! numbers but re-encodes operands and adds EA-specific shorthand opcodes, so
//! going APT -> SWF *expands* those shorthands into standard AVM1 actions, and
//! going SWF -> APT parses standard AVM1 into the APT instruction model.

pub mod assets;
pub mod bitmaps;
pub mod bytecode;
pub mod from_swf;
pub mod geometry;
pub mod imports;
pub mod to_swf;

use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("apt error: {0}")]
    Apt(#[from] apt::Error),
    #[error("apt-aux error: {0}")]
    Aux(String),
    #[error("swf read error: {0}")]
    SwfRead(String),
    #[error("swf write error: {0}")]
    SwfWrite(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

pub use assets::{Assets, DiskAssets, NoAssets};
pub use from_swf::swf_to_apt;
pub use to_swf::apt_to_swf;

/// How to convert a movie from disk.
#[derive(Debug, Clone, Copy)]
pub struct ConvertOptions {
    /// Embed textures as SWF bitmap fills rather than drawing textured shapes
    /// with their flat vertex color.
    pub textures: bool,
    /// Copy imported characters into the movie instead of keeping the import.
    ///
    /// Off by default: imports are preserved as SWF `ImportAssets`, mirroring
    /// how the game ships a shared library movie (BFME's `MenuExport`) that the
    /// screens reference. That means the imported movies must be converted too
    /// — see [`convert_movie_with_imports`]. Turn this on for a single
    /// self-contained SWF at the cost of duplicating the shared art.
    pub inline_imports: bool,
    /// Replace the movie's own background color (RGB). Menu movies often set a
    /// dark backdrop that in game sits over the 3D shell map; a viewer usually
    /// wants a neutral background instead. `None` keeps the movie's color.
    pub override_background: Option<u32>,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        ConvertOptions {
            textures: true,
            inline_imports: false,
            override_background: None,
        }
    }
}

/// One converted movie.
pub struct ConvertedMovie {
    /// Movie base name, e.g. `MenuExport` — the SWF belongs at
    /// `<name>.swf` next to its siblings for imports to resolve.
    pub name: String,
    /// Base path the movie was read from (no extension).
    pub base: PathBuf,
    pub swf: Vec<u8>,
}

/// Convert the movie at `base` (a path with or without an extension) to SWF.
///
/// Unless `options.inline_imports` is set, the result may reference sibling
/// movies via `ImportAssets`; use [`convert_movie_with_imports`] to convert
/// those too.
pub fn convert_movie(base: &Path, options: &ConvertOptions) -> Result<Vec<u8>> {
    let base = apt::base_path(base);
    let file = apt::AptFile::load(&base)?;
    convert_loaded_movie(&base, &file, options)
}

/// As [`convert_movie`], for a movie that is already parsed.
pub fn convert_loaded_movie(
    base: &Path,
    file: &apt::AptFile,
    options: &ConvertOptions,
) -> Result<Vec<u8>> {
    if options.inline_imports && !file.movie.imports.is_empty() {
        let mut warn = |msg: String| log::warn!("{msg}");
        let resolved = imports::resolve(base, file, &mut warn)?;
        let assets = DiskAssets::new(resolved.origins, options.textures);
        to_swf::apt_to_swf_with(&resolved.file, &assets, options.override_background)
    } else {
        let assets = DiskAssets::for_movie(base, file.movie.characters.len(), options.textures);
        to_swf::apt_to_swf_with(file, &assets, options.override_background)
    }
}

/// Convert the movie at `base` together with every movie it imports, directly
/// or transitively.
///
/// The movie itself is always first. Writing all of them side by side gives a
/// playable set: each `ImportAssets` resolves against its sibling `.swf`.
/// With `options.inline_imports` the list is just the one movie.
pub fn convert_movie_with_imports(
    base: &Path,
    options: &ConvertOptions,
) -> Result<Vec<ConvertedMovie>> {
    let base = apt::base_path(base);

    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(base);

    while let Some(base) = queue.pop_front() {
        let name = match base.file_name().and_then(|n| n.to_str()) {
            Some(n) if seen.insert(n.to_string()) => n.to_string(),
            _ => continue, // unnamed, or already converted
        };
        let file = apt::AptFile::load(&base)?;
        if !options.inline_imports {
            for import in &file.movie.imports {
                if !seen.contains(&import.movie) {
                    queue.push_back(imports::resolve_movie(&base, &import.movie));
                }
            }
        }
        let swf = convert_loaded_movie(&base, &file, options)?;
        out.push(ConvertedMovie { name, base, swf });
    }
    Ok(out)
}
