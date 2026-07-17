# AptTools — notes for Claude

Rust framework for EA's APT format (SWF-derived in-game UI). Cargo workspace;
all crates under `crates/`.

## Crate graph
- `apt` — core `.apt`/`.const` reader+writer + AVM1 action model. No SWF dep.
- `apt-aux` — game-side aux assets: `GeometryFormat` trait + `.ru` impl, `.dat`.
- `apt-convert` — APT⟷SWF (uses `swf` = Ruffle's crate). `bytecode` submodule is
  the AVM1 bridge; `to_swf`/`from_swf` are structural.
- `aptinfo`/`apt2swf`/`swf2apt`/`aptview` — thin CLIs over the libs.

## Format essentials (see docs/ for the full spec)
- `.apt` = memory image; pointer slots hold file-relative offsets (0 = NULL),
  character indices, or magics (`0x09876543` parent, `0x98765432`/`0x12345678`
  function pools). Header tag `"Apt Data:<decoupled>:<swfver>:<ptrsize>"`,
  sniffed positionally; classic short tag `"Apt Data:6\x1a"` => coupled, v6,
  4-byte. Everything little-endian.
- `.const` magic is `"Apt constant file\x1a\0\0"` (20 bytes) for the shipped
  games (NOT "Apt1" — that was the FIFA-era pipeline). Preserved verbatim on
  write. Holds `pMainCharacter` = offset of the root Animation in the `.apt`.
- Pointer size (4/8) changes struct sizes AND offsets. Inline action structs
  align to ptr size on blob-relative offsets; 1/2/4-byte immediates unaligned.
- Constant-index sequencing: Push items reference `.const` entries by index, and
  those indices must be globally sequential in the engine's resolve-walk order.
  The writer's traversal matches that order (verified: written const tables
  equal the originals in file order).
- Decoupled variant: only `AptCharacter` grows (a 4-byte data word — the shape's
  backing bitmap id — plus a dead ptr slot). Everything else identical.

## Invariants when editing the writer
- Never place data at offset 0 (the 16-byte header occupies it).
- Emit character streams in index order, root movie first; within a movie,
  frames then controls in order, PlaceObject event blocks in order — this is
  what keeps constant indices sequential.
- Zero all padding.

## Testing
`cargo test` — auto-skips if corpus `/home/stephan/Devel/APT` is absent.
- `apt/tests/roundtrip.rs` — 541/541 semantic round-trip + const ordering.
- `apt/tests/layouts.rs` — 64-bit & decoupled re-emit consistency.
- `apt-convert/tests/bytecode.rs` — every corpus stream lowers to AVM1 that the
  `swf` crate re-parses.

## Known gaps (see README "Status & limitations")
SWF shape triangulation → `.ru` not done; textured fills approximated; fonts /
morph / sounds partial; `Try` not translated to SWF.
