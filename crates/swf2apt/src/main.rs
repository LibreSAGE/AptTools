//! `swf2apt` — convert a standard SWF into an APT movie (`.apt` + `.const`).
//!
//! The two format-shaping options requested at build time:
//!   --ptr-size {4|8}  the pointer size to generate the APT for
//!   --decouple        emit the decoupled-rendering variant

use std::path::PathBuf;

use anyhow::{Context, Result};
use apt::write::WriteOptions;
use apt::PtrSize;
use apt_aux::ru::RuFormat;
use apt_aux::GeometryFormat;
use apt_convert::from_swf::SwfToAptOptions;
use clap::{Parser, ValueEnum};

/// Image container for the exported textures. TGA is the classic games'
/// native format (and the only one the reference viewers load); PNG/DDS are
/// offered for tooling that prefers them.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum TextureFormat {
    Tga,
    Png,
    Dds,
}

impl TextureFormat {
    fn ext(self) -> &'static str {
        match self {
            TextureFormat::Tga => "tga",
            TextureFormat::Png => "png",
            TextureFormat::Dds => "dds",
        }
    }
}

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

    /// Skip exporting the auxiliary assets (shape geometry `.ru` files, the
    /// `.dat` texture map, and the exported textures).
    #[arg(long)]
    no_aux: bool,

    /// Export each bitmap fill as its own standalone texture instead of packing
    /// them all into one shared atlas.
    #[arg(long)]
    no_pack_textures: bool,

    /// Container format for the exported textures.
    #[arg(long, value_enum, default_value_t = TextureFormat::Tga)]
    texture_format: TextureFormat,
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

    // Strip any extension so the `<base>_geometry` / `<base>_textures` aux
    // directories are named from the bare movie name, not `<name>.swf_...`.
    let base = cli
        .output
        .unwrap_or_else(|| apt::base_path(&cli.input))
        .with_extension("");
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
    if cli.no_aux {
        return Ok(());
    }

    // Export the game-side aux assets that live outside the `.apt` blob:
    //   <base>_geometry/<index>.ru            shape geometry
    //   <base>.dat                            bitmap-char -> texture-id map
    //   <dir>/art/Textures/apt_<name>_<id>.*  exported textures (the path and
    //                                         naming the reference viewers load)
    let assets = apt_convert::from_swf::extract_geometry(&swf_data, !cli.no_pack_textures)
        .context("extracting aux assets")?;

    let ru = RuFormat;
    for (&shape_index, geometry) in &assets.geometry {
        ru.store(&base, shape_index, geometry)
            .with_context(|| format!("writing geometry for shape {shape_index}"))?;
    }

    if !assets.texture_map.entries.is_empty() {
        let dat_path = base.with_extension("dat");
        std::fs::write(&dat_path, assets.texture_map.serialize())
            .with_context(|| format!("writing {}", dat_path.display()))?;
    }

    if !assets.textures.is_empty() {
        let name = base
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("apt")
            .to_string();
        let tex_dir = base
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("art/Textures");
        std::fs::create_dir_all(&tex_dir)
            .with_context(|| format!("creating {}", tex_dir.display()))?;
        let ext = cli.texture_format.ext();
        for (&tex_id, tex) in &assets.textures {
            let tex_path = tex_dir.join(format!("apt_{name}_{tex_id}.{ext}"));
            tex.save(&tex_path)
                .with_context(|| format!("writing {}", tex_path.display()))?;
        }
        log::info!(
            "wrote aux assets: {} shape(s), {} texture(s) ({}, {}) under {}",
            assets.geometry.len(),
            assets.textures.len(),
            if cli.no_pack_textures {
                "unpacked"
            } else {
                "packed atlas"
            },
            ext,
            tex_dir.display(),
        );
    } else {
        log::info!(
            "wrote aux assets: {} shape(s), no bitmap fills",
            assets.geometry.len()
        );
    }

    Ok(())
}
