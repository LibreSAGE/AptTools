//! `swf2apt` — convert a standard SWF into an APT movie (`.apt` + `.const`).
//!
//! The two format-shaping options requested at build time:
//!   --ptr-size {4|8}  the pointer size to generate the APT for
//!   --decouple        emit the decoupled-rendering variant

use std::path::PathBuf;

use anyhow::{Context, Result};
use apt::write::WriteOptions;
use apt::PtrSize;
use apt_convert::from_swf::SwfToAptOptions;
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "swf2apt",
    version,
    about = "Convert a SWF file to EA's APT format"
)]
struct Cli {
    /// Input `.swf` file.
    input: PathBuf,

    /// Output base path (writes `<base>.apt` and `<base>.const`).
    /// Defaults to the input file's base name.
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Pointer size to generate the APT for: 4 (32-bit) or 8 (64-bit).
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u8).range(4..=8))]
    ptr_size: u8,

    /// Generate the decoupled-rendering variant of the format.
    #[arg(long)]
    decouple: bool,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();

    let ptr_size = match cli.ptr_size {
        4 => PtrSize::Four,
        8 => PtrSize::Eight,
        n => anyhow::bail!("unsupported pointer size {n} (must be 4 or 8)"),
    };

    let swf_data =
        std::fs::read(&cli.input).with_context(|| format!("reading {}", cli.input.display()))?;
    let file = apt_convert::swf_to_apt(
        &swf_data,
        SwfToAptOptions {
            ptr_size,
            decoupled: cli.decouple,
        },
    )
    .context("converting SWF to APT")?;

    let opts = WriteOptions::new(ptr_size, cli.decouple, file.header.swf_version);
    let (apt_bytes, const_bytes) = file.write(&opts).context("serializing APT")?;

    let base = cli.output.unwrap_or_else(|| apt::base_path(&cli.input));
    let apt_path = base.with_extension("apt");
    let const_path = base.with_extension("const");
    std::fs::write(&apt_path, &apt_bytes)
        .with_context(|| format!("writing {}", apt_path.display()))?;
    std::fs::write(&const_path, &const_bytes)
        .with_context(|| format!("writing {}", const_path.display()))?;

    log::info!(
        "wrote {} ({} bytes) and {} ({} bytes): {}-byte ptr, {}, {} characters, {} frames",
        apt_path.display(),
        apt_bytes.len(),
        const_path.display(),
        const_bytes.len(),
        ptr_size.bytes(),
        if cli.decouple { "decoupled" } else { "coupled" },
        file.movie.characters.len(),
        file.movie.frames.len(),
    );
    log::warn!("shape geometry (.ru) and textures are not yet extracted from SWF; the APT will render without shape fills");
    Ok(())
}
