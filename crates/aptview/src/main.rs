//! `aptview` — view an APT movie (or a plain SWF) in an embedded Ruffle player.
//!
//! An APT movie is converted to SWF in memory and handed straight to Ruffle, so
//! there is no intermediate file and no external player to install. Passing a
//! `.swf` plays it as-is, which makes it easy to compare an original Flash movie
//! against the APT the game shipped:
//!
//! ```text
//! aptview MainMenu.apt          # the APT, converted on the fly
//! aptview MainMenu.swf          # the original SWF, for comparison
//! ```
//!
//! This is a plain viewer: no debug UI, no menus.

mod player;
mod shot;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use apt_convert::ConvertOptions;
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "aptview",
    version,
    about = "View an APT movie (or SWF) in an embedded Ruffle player"
)]
struct Cli {
    /// Input `.apt` file (or its base name), or a `.swf` to play directly.
    input: PathBuf,

    /// Don't embed the movie's textures; draw textured shapes with their flat
    /// vertex color instead. Textures are embedded by default so the movie
    /// looks like it does in game. Ignored for `.swf` input.
    #[arg(long)]
    no_textures: bool,

    /// Write the converted SWF(s) to this directory as well as playing them.
    #[arg(short, long)]
    output_dir: Option<PathBuf>,

    /// Copy imported characters in rather than loading the imported movies
    /// alongside. Useful to check a movie in isolation.
    #[arg(long)]
    inline_imports: bool,

    /// Keep the movie's own background color. By default the viewer clears to
    /// white like the reference AptViewer — game menus often set a dark
    /// backdrop that in game sits over the 3D shell map.
    #[arg(long)]
    movie_background: bool,

    /// Convert only; don't open a window.
    #[arg(long)]
    no_play: bool,

    /// Render a frame to this PNG instead of opening a window.
    #[arg(long, value_name = "PNG")]
    screenshot: Option<PathBuf>,

    /// Which frame to screenshot (1-based). Later frames give scripts time to
    /// build the screen.
    #[arg(long, default_value_t = 1)]
    frame: u32,

    /// Move the mouse to `x,y` (stage pixels) midway through the screenshot
    /// frames, to exercise hover states headlessly.
    #[arg(long, value_name = "X,Y")]
    hover: Option<String>,

    /// Also press and release the left button at the --hover position.
    #[arg(long)]
    click: bool,
}

fn main() -> Result<()> {
    // One subscriber for everything: our own logs, Ruffle's `tracing`
    // diagnostics, and AVM trace() output. Tunable via RUST_LOG.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("info,wgpu_core=warn,wgpu_hal=warn,naga=warn")
            }),
        )
        .init();
    let cli = Cli::parse();

    // A .swf plays as-is; an APT movie is converted first, along with every
    // movie it imports (the player loads those siblings by name at runtime).
    let is_swf = cli.input.extension().and_then(|e| e.to_str()) == Some("swf");
    let (swf_bytes, dir, file_name) = if is_swf {
        let bytes = std::fs::read(&cli.input)
            .with_context(|| format!("reading {}", cli.input.display()))?;
        let dir = parent_dir(&cli.input);
        let name = cli
            .input
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("movie.swf")
            .to_string();
        (bytes, dir, name)
    } else {
        let base = apt::base_path(&cli.input);
        let options = ConvertOptions {
            textures: !cli.no_textures,
            inline_imports: cli.inline_imports,
            override_background: if cli.movie_background {
                None
            } else {
                Some(0xFFFFFF)
            },
        };
        let converted = apt_convert::convert_movie_with_imports(&base, &options)
            .with_context(|| format!("converting {}", base.display()))?;
        for movie in &converted {
            log::info!("converted {} ({} bytes)", movie.name, movie.swf.len());
        }

        // The movies have to exist side by side on disk for imports to resolve.
        let dir = match &cli.output_dir {
            Some(dir) => dir.clone(),
            None => scratch_dir()?,
        };
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        for movie in &converted {
            let path = dir.join(format!("{}.swf", movie.name));
            std::fs::write(&path, &movie.swf)
                .with_context(|| format!("writing {}", path.display()))?;
        }
        let root = converted.first().context("nothing converted")?;
        (root.swf.clone(), dir, format!("{}.swf", root.name))
    };

    if cli.no_play {
        return Ok(());
    }
    if swf_bytes.is_empty() {
        bail!("no SWF data to play");
    }

    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
    let url = format!("file://{}/{}", dir.display(), file_name);
    match &cli.screenshot {
        Some(png) => {
            let hover = cli
                .hover
                .as_deref()
                .and_then(|s| s.split_once(','))
                .and_then(|(x, y)| Some((x.trim().parse().ok()?, y.trim().parse().ok()?)));
            if cli.hover.is_some() && hover.is_none() {
                bail!("--hover expects X,Y (e.g. --hover 320,700)");
            }
            shot::capture(swf_bytes, url, dir, cli.frame, png, hover, cli.click)
        }
        None => player::run(swf_bytes, url, dir, title(&cli.input)),
    }
}

/// The directory holding `path`. A bare filename has an *empty* parent rather
/// than none, which is not a directory anything can be resolved against.
fn parent_dir(path: &Path) -> PathBuf {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// A per-run directory for converted movies, cleaned up by the OS.
fn scratch_dir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("aptview-{}", std::process::id()));
    Ok(dir)
}

fn title(input: &Path) -> String {
    let name = input
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("movie");
    format!("aptview — {name}")
}
