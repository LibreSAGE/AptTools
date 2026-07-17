//! The `.dat` texture-map format: bitmap character ID -> texture ID.
//!
//! Line-oriented ASCII; `;` begins a comment, blank lines ignored. Two forms:
//! - `<bitmapCharID>-><textureID>` — mapped
//! - `<bitmapCharID>=<textureID>`  — unmapped (texture ID equals the char ID)

use std::collections::BTreeMap;
use std::path::Path;

use crate::{Error, Result};

/// A parsed `.dat` texture map.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TextureMap {
    /// bitmap character ID -> texture ID.
    pub entries: BTreeMap<u32, u32>,
}

impl TextureMap {
    pub fn parse(data: &[u8]) -> Result<TextureMap> {
        let text = String::from_utf8_lossy(data);
        let mut entries = BTreeMap::new();
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with(';') {
                continue;
            }
            let (char_id, tex_id) = if let Some((l, r)) = line.split_once("->") {
                (parse_id(l)?, parse_id(r)?)
            } else if let Some((l, _r)) = line.split_once('=') {
                // Unmapped: texture ID is the character ID itself.
                let id = parse_id(l)?;
                (id, id)
            } else {
                return Err(Error::Dat(format!("unrecognized line: {line:?}")));
            };
            entries.insert(char_id, tex_id);
        }
        Ok(TextureMap { entries })
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut out = String::new();
        for (char_id, tex_id) in &self.entries {
            if char_id == tex_id {
                out.push_str(&format!("{char_id}={tex_id}\r\n"));
            } else {
                out.push_str(&format!("{char_id}->{tex_id}\r\n"));
            }
        }
        out.into_bytes()
    }

    pub fn load(path: &Path) -> Result<TextureMap> {
        Self::parse(&std::fs::read(path)?)
    }

    /// Resolve a bitmap character ID to a texture ID (identity if absent).
    pub fn texture_id(&self, bitmap_character_id: u32) -> u32 {
        self.entries
            .get(&bitmap_character_id)
            .copied()
            .unwrap_or(bitmap_character_id)
    }
}

fn parse_id(s: &str) -> Result<u32> {
    s.trim()
        .parse()
        .map_err(|_| Error::Dat(format!("bad id: {s:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mapped_and_unmapped() {
        let m = TextureMap::parse(b"; comment\n5->12\n7=7\n").unwrap();
        assert_eq!(m.texture_id(5), 12);
        assert_eq!(m.texture_id(7), 7);
        assert_eq!(m.texture_id(99), 99); // absent -> identity
                                          // Round-trip.
        let m2 = TextureMap::parse(&m.serialize()).unwrap();
        assert_eq!(m, m2);
    }
}
