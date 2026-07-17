//! `apt2swf` — reassemble a standard SWF from an APT movie (`.apt` + `.const`
//! plus its `<base>_geometry/*.ru` files). The header is inspected to pick up
//! the 4/8-byte pointer size and decoupled variant automatically.

use std::path::PathBuf;

use anyhow::{Context, Result};
use apt_convert::ConvertOptions;
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "apt2swf",
    version,
    about = "Reassemble a SWF file from an APT movie"
)]
struct Cli {
    /// Input `.apt` file (or its base name). The `.const` and geometry files
    /// are located automatically.
    input: PathBuf,

    /// Output `.swf` path (default: input base name + `.swf`).
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Embed the movie's textures as SWF bitmap fills, so textured shapes look
    /// like they do in game. Images are found via the `.dat` map next to the
    /// movie; without this, textured shapes use their flat vertex color.
    #[arg(short = 't', long)]
    embed_textures: bool,

    /// Copy imported characters into the movie, producing one self-contained
    /// SWF. By default imports are preserved (as SWF `ImportAssets`) and every
    /// movie they reference is converted alongside this one.
    #[arg(long)]
    inline_imports: bool,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();

    let base = apt::base_path(&cli.input);
    let options = ConvertOptions {
        textures: cli.embed_textures,
        inline_imports: cli.inline_imports,
        ..Default::default()
    };

    let converted = apt_convert::convert_movie_with_imports(&base, &options)
        .with_context(|| format!("converting {}", base.display()))?;

    // The first entry is the movie itself; the rest are the libraries it
    // imports, which must land beside it for those imports to resolve.
    let output = cli.output.unwrap_or_else(|| base.with_extension("swf"));
    let dir = output.parent().unwrap_or(std::path::Path::new("."));
    for (i, movie) in converted.iter().enumerate() {
        let path = if i == 0 {
            output.clone()
        } else {
            dir.join(format!("{}.swf", movie.name))
        };
        std::fs::write(&path, &movie.swf).with_context(|| format!("writing {}", path.display()))?;
        log::info!(
            "wrote {} ({} bytes){}",
            path.display(),
            movie.swf.len(),
            if i == 0 { "" } else { " [imported]" }
        );
    }
    Ok(())
}
