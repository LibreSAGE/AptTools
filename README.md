# AptTools

A Rust framework for EA's **APT** file format — the SWF-derived in-game UI
format used by EA/Westwood games built on the SAGE/RNA engines
(C&C Generals, BFME 1/2, C&C3/Tiberium Wars, Kane's Wrath, Red Alert 3, …).

APT is a memory-image format: a `.apt` blob is a direct dump of the engine's
C structures with every pointer stored as a file-relative offset, paired with a
`.const` file (constant table + entry point) and out-of-band auxiliary assets
(`.ru` geometry, `.dat` texture maps, textures). The exact byte layout depends
on the **pointer size** the file was built for (4 or 8 bytes) and on whether it
targets **decoupled rendering**; both are recorded in the `.apt` header tag
`"Apt Data:<decoupled>:<swf version>:<ptr size>"`.

## Workspace layout

All crates live under `crates/`:

| Crate | Kind | What it does |
|-------|------|--------------|
| [`apt`](crates/apt) | lib | Byte-accurate reader/writer for `.apt` + `.const` (both pointer sizes, coupled and decoupled) and the AVM1 action-stream model. |
| [`apt-aux`](crates/apt-aux) | lib | Auxiliary game-side assets: the `GeometryFormat` trait with the classic `.ru` rendering-unit implementation, and `.dat` texture maps. Other games' geometry formats can plug in via the trait. |
| [`apt-convert`](crates/apt-convert) | lib | APT ⟷ standard SWF conversion, including the AVM1 bytecode bridge (expanding EA shorthand opcodes to plain AVM1 and back). |
| [`aptinfo`](crates/aptinfo) | bin | Print metadata, list characters, dump the constant table, disassemble action streams. |
| [`apt2swf`](crates/apt2swf) | bin | Reassemble a standard `.swf` from an APT movie. |
| [`swf2apt`](crates/swf2apt) | bin | Convert a `.swf` to APT with selectable pointer size and decoupled mode. |
| [`aptview`](crates/aptview) | bin | View an APT movie by converting it to SWF and launching a Flash player (Ruffle). |

## Building

```sh
cargo build --release
```

Binaries land in `target/release/{aptinfo,apt2swf,swf2apt,aptview}`.

## Usage

```sh
# Inspect a movie
aptinfo MainMenu.apt                 # summary
aptinfo -c MainMenu.apt              # + character list
aptinfo -a MainMenu.apt              # + action-stream disassembly
aptinfo --constants MainMenu.apt     # + constant table

# APT -> SWF (reads .const and <base>_geometry/*.ru automatically)
apt2swf MainMenu.apt -o MainMenu.swf

# SWF -> APT
swf2apt MainMenu.swf --ptr-size 4              # 32-bit, coupled (default)
swf2apt MainMenu.swf --ptr-size 8 --decouple   # 64-bit, decoupled
#   -> writes MainMenu.apt + MainMenu.const

# View (needs `ruffle` on PATH, or pass --player <exe>)
aptview MainMenu.apt
aptview MainMenu.apt --no-launch -o MainMenu.swf   # just emit the SWF
```

`swf2apt` accepts the two format-shaping options requested at build time:

- `--ptr-size {4|8}` — the pointer size the generated APT targets.
- `--decouple` — emit the decoupled-rendering struct variant (shape records
  carry the backing bitmap character ID; everything else is identical to the
  coupled layout).

## Correctness

The reader/writer are validated against a corpus of 541 shipped APT movies from
five games (BFME, BFME2, KW, RA3, TW):

- **541/541** movies parse, and re-serializing the parsed model reproduces an
  `.apt`/`.const` pair that re-parses to an **identical model** (semantic
  round-trip). The constant tables also match in file order, which proves the
  writer emits Push-item constant indices in the engine's resolve order — the
  one ordering the runtime asserts at load.
- **23097/23097** action streams in the corpus lower to standard SWF AVM1
  bytecode that the [`swf`](https://crates.io/crates/swf) crate's own (Ruffle)
  AVM1 reader accepts.
- The 8-byte and decoupled writer/reader paths are exercised by re-emitting the
  (32-bit, coupled) corpus in those layouts and confirming the model survives.

Run the tests (they auto-skip if the corpus at `/home/stephan/Devel/APT` is
absent):

```sh
cargo test
```

Byte-for-byte reproduction of the original files is **not** a goal: the classic
`swfc` compiler used its own allocation order (arrays and strings first, root
Animation partway into the blob, a 12-byte header). AptTools uses its own
deterministic, engine-compatible layout.

## Status & limitations

Fully working:

- `.apt`/`.const` read + write for all character types, controls, filters, and
  the complete AVM1 opcode set (standard + EA extensions), for 4- and 8-byte
  pointers and both rendering variants.
- `.ru` geometry and `.dat` texture-map parsing/serialization.
- `aptinfo` and the AVM1 bytecode bridge in both directions.

Partial / not yet implemented:

- **APT → SWF** produces a valid, player-loadable SWF with the timeline
  structure, sprites, placements, translated actions, per-instance blend modes
  and filters, buttons, and dynamic text (device-font by name at the original
  size). Shape geometry is rebuilt from the `.ru` triangle lists, with bitmap
  fills re-embedded from the atlas (solid vertex-color fallback when no texture
  is supplied). Not converted: embedded font glyph outlines (text uses a device
  font), morph interpolation, and static text — the APT stores no glyph/morph
  edge data to reconstruct these from. Sound characters carry no payload.
- **SWF → APT** recovers timeline structure, sprites, shape bounds+`.ru`
  geometry, bitmap-fill textures (atlas or per-image), actions, frame labels,
  clip event handlers (`onClipEvent`/`on`), per-instance filters and blend
  modes, and cross-movie imports. Text, button, and font characters are not yet
  converted (placed instances become blank placeholders); morph interpolation
  and sound payloads are not converted.
- `Try`/`Catch` action blocks are modeled and round-trip within APT, but are not
  yet translated to SWF.

## License

MIT
