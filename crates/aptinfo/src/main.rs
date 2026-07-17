//! `aptinfo` — display metadata about an APT file.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use apt::actions::{opcode_name, ActionStream, Instruction};
use apt::{AptFile, Character, CharacterSlot, Control, PtrSize};
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "aptinfo",
    version,
    about = "Display metadata about an APT file"
)]
struct Cli {
    /// Path to the `.apt` file (or its base name); the `.const` sibling is read too.
    file: PathBuf,

    /// Also list every character with its type and index.
    #[arg(short, long)]
    characters: bool,

    /// Disassemble all action streams in the movie.
    #[arg(short, long)]
    actions: bool,

    /// Print the constant table.
    #[arg(long)]
    constants: bool,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let cli = Cli::parse();

    let file = AptFile::load(&cli.file)
        .with_context(|| format!("loading APT movie {}", cli.file.display()))?;

    print_summary(&file);

    if cli.characters {
        println!("\nCharacters:");
        for (i, slot) in file.movie.characters.iter().enumerate() {
            let desc = match slot {
                CharacterSlot::Root => "Root (Animation)".to_string(),
                CharacterSlot::Empty => "<empty / import slot>".to_string(),
                CharacterSlot::Character(c) => describe_character(c),
            };
            println!("  [{i:>4}] {desc}");
        }
    }

    if cli.constants {
        let base = apt::base_path(&cli.file);
        let const_data = std::fs::read(base.with_extension("const"))?;
        let cf = apt::ConstFile::read(&const_data, file.header.ptr_size)?;
        println!("\nConstants ({}):", cf.constants.len());
        for (i, v) in cf.constants.iter().enumerate() {
            println!("  [{i:>4}] {v:?}");
        }
    }

    if cli.actions {
        println!("\nAction streams:");
        let mut counter = 0;
        for (fi, frame) in file.movie.frames.iter().enumerate() {
            for control in &frame.controls {
                if let Some(stream) = control_stream(control) {
                    println!("  -- root frame {fi} {} --", control_label(control));
                    disassemble(stream, 2);
                    counter += 1;
                }
            }
        }
        for (i, slot) in file.movie.characters.iter().enumerate() {
            if let CharacterSlot::Character(c) = slot {
                for (label, stream) in character_streams(c) {
                    println!("  -- character {i} {label} --");
                    disassemble(stream, 2);
                    counter += 1;
                }
            }
        }
        if counter == 0 {
            println!("  (none)");
        }
    }

    Ok(())
}

fn print_summary(file: &AptFile) {
    let h = &file.header;
    println!("APT movie");
    println!(
        "  pointer size : {}",
        match h.ptr_size {
            PtrSize::Four => "4 bytes (32-bit)",
            PtrSize::Eight => "8 bytes (64-bit)",
        }
    );
    println!(
        "  decoupled    : {}",
        if h.decoupled { "yes" } else { "no" }
    );
    println!("  SWF version  : {}", h.swf_version);
    let tag: String = h
        .raw_tag
        .iter()
        .take_while(|&&b| b != 0)
        .map(|&b| b as char)
        .collect();
    println!("  header tag   : {tag:?}");
    println!(
        "  dimensions   : {} x {}",
        file.movie.width, file.movie.height
    );
    println!(
        "  frame rate   : {} ms/frame{}",
        file.movie.ms_per_frame,
        if file.movie.ms_per_frame > 0 {
            format!(" (~{:.1} fps)", 1000.0 / file.movie.ms_per_frame as f32)
        } else {
            String::new()
        }
    );
    println!("  frames       : {}", file.movie.frames.len());
    println!("  characters   : {}", file.movie.characters.len());
    println!("  imports      : {}", file.movie.imports.len());
    println!("  exports      : {}", file.movie.exports.len());

    let mut hist: BTreeMap<&str, usize> = BTreeMap::new();
    for slot in &file.movie.characters {
        let key = match slot {
            CharacterSlot::Root => "Animation(root)",
            CharacterSlot::Empty => "empty",
            CharacterSlot::Character(c) => c.type_name(),
        };
        *hist.entry(key).or_default() += 1;
    }
    println!("  by type      :");
    for (k, n) in hist {
        println!("      {k:<16} {n}");
    }

    if !file.movie.exports.is_empty() {
        println!("  exported symbols:");
        for e in &file.movie.exports {
            println!("      {} -> character {}", e.name, e.character_id);
        }
    }
    if !file.movie.imports.is_empty() {
        println!("  imported symbols:");
        for i in &file.movie.imports {
            println!(
                "      {}:{} -> character {}",
                i.movie, i.name, i.character_id
            );
        }
    }
}

fn describe_character(c: &Character) -> String {
    match c {
        Character::Shape(s) => format!(
            "Shape bounds=({:.0},{:.0})-({:.0},{:.0}){}",
            s.bounds.left,
            s.bounds.top,
            s.bounds.right,
            s.bounds.bottom,
            s.bitmap_character_id
                .filter(|&id| id != 0)
                .map(|id| format!(" texture=char#{id}"))
                .unwrap_or_default()
        ),
        Character::Text(t) => format!("EditText font=#{} text={:?}", t.font_id, t.initial_text),
        Character::Font(f) => format!("Font {:?} ({} glyphs)", f.name, f.glyphs.len()),
        Character::Button(b) => format!(
            "Button ({} records, {} actions)",
            b.records.len(),
            b.actions.len()
        ),
        Character::Sprite(s) => format!("Sprite ({} frames)", s.frames.len()),
        Character::Sound => "Sound".to_string(),
        Character::Bitmap => "Bitmap".to_string(),
        Character::Morph(m) => {
            format!("Morph #{} -> #{}", m.start_character_id, m.end_character_id)
        }
        Character::StaticText(s) => format!("StaticText ({} records)", s.records.len()),
        Character::None => "None (packed texture?)".to_string(),
        Character::Video => "Video".to_string(),
    }
}

fn control_stream(c: &Control) -> Option<&ActionStream> {
    match c {
        Control::Action(s) | Control::InitAction { actions: s, .. } => Some(s),
        _ => None,
    }
}

fn control_label(c: &Control) -> &'static str {
    match c {
        Control::Action(_) => "DoAction",
        Control::InitAction { .. } => "DoInitAction",
        _ => "",
    }
}

fn character_streams(c: &Character) -> Vec<(String, &ActionStream)> {
    let mut out = Vec::new();
    match c {
        Character::Sprite(s) => collect_frame_streams(&s.frames, &mut out),
        Character::Button(b) => {
            for (i, a) in b.actions.iter().enumerate() {
                out.push((
                    format!("button action {i} (cond {:#x})", a.conditions),
                    &a.actions,
                ));
            }
        }
        _ => {}
    }
    out
}

fn collect_frame_streams<'a>(frames: &'a [apt::Frame], out: &mut Vec<(String, &'a ActionStream)>) {
    for (fi, frame) in frames.iter().enumerate() {
        for control in &frame.controls {
            match control {
                Control::Action(s) => out.push((format!("frame {fi} DoAction"), s)),
                Control::InitAction { actions, .. } => {
                    out.push((format!("frame {fi} InitAction"), actions))
                }
                Control::PlaceObject(p) => {
                    if let Some(blocks) = &p.clip_actions {
                        for (bi, b) in blocks.iter().enumerate() {
                            out.push((format!("frame {fi} clip event {bi}"), &b.actions));
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn disassemble(stream: &ActionStream, indent: usize) {
    let pad = " ".repeat(indent);
    for insn in &stream.instructions {
        match insn {
            Instruction::Simple(op) => println!("{pad}{}", opcode_name(*op)),
            Instruction::End => println!("{pad}End"),
            Instruction::Push(items) => println!("{pad}Push {items:?}"),
            Instruction::DefineDictionary(items) => {
                println!("{pad}DefineDictionary ({} items)", items.len())
            }
            Instruction::DefineFunction { name, params, body } => {
                println!("{pad}DefineFunction {name:?}({}) {{", params.join(", "));
                disassemble(body, indent + 2);
                println!("{pad}}}");
            }
            Instruction::DefineFunction2 {
                name, params, body, ..
            } => {
                let ps: Vec<_> = params.iter().map(|(_, n)| n.clone()).collect();
                println!("{pad}DefineFunction2 {name:?}({}) {{", ps.join(", "));
                disassemble(body, indent + 2);
                println!("{pad}}}");
            }
            other => println!("{pad}{other:?}"),
        }
    }
}
