//! Import resolution: inlining characters from sibling movies.
//!
//! An APT movie can leave character slots empty and declare an import for them
//! (`MenuExport:buttonReg_up -> character 1`); the engine fills those slots at
//! load time from the other movie's export table. Nothing renders without it —
//! BFME's menus keep nearly all their art in a shared `MenuExport` movie.
//!
//! We resolve imports by copying the referenced character — and everything it
//! transitively references — into the movie, so the converted SWF is
//! self-contained. Copied characters keep an [`Origin`], because their geometry
//! and textures still live next to the movie they came from.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use apt::{AptFile, Character, CharacterSlot, Control, Frame, Movie};

use crate::{Error, Result};

/// Which movie a character in a resolved movie came from, and its index there.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Origin {
    /// Movie base path (no extension).
    pub base: PathBuf,
    /// Character index within that movie.
    pub index: u32,
}

/// A movie with its imports inlined, plus the origin of every character.
pub struct Resolved {
    pub file: AptFile,
    /// One entry per character in `file.movie.characters`.
    pub origins: Vec<Origin>,
}

/// Inline every import in `file` (recursively), loading sibling movies from
/// `base`'s directory. Imports that can't be resolved are left empty and
/// reported through `on_warning`.
pub fn resolve(
    base: &Path,
    file: &AptFile,
    on_warning: &mut dyn FnMut(String),
) -> Result<Resolved> {
    let mut r = Resolver {
        loaded: HashMap::new(),
        memo: HashMap::new(),
        movie: file.movie.clone(),
        origins: Vec::new(),
        warn: on_warning,
    };
    r.origins = (0..r.movie.characters.len())
        .map(|i| Origin {
            base: base.to_path_buf(),
            index: i as u32,
        })
        .collect();
    for i in 0..r.movie.characters.len() {
        r.memo.insert((base.to_path_buf(), i as u32), i as u32);
    }

    let imports = r.movie.imports.clone();
    for import in &imports {
        let source = resolve_movie(base, &import.movie);
        match r.import_slot(&source, &import.name, import.character_id) {
            Ok(()) => {}
            Err(e) => (r.warn)(format!(
                "unresolved import {}:{} -> character {}: {e}",
                import.movie, import.name, import.character_id
            )),
        }
    }

    // The characters are inlined now, so the movie no longer imports anything.
    r.movie.imports.clear();
    Ok(Resolved {
        file: AptFile {
            header: file.header,
            const_magic: file.const_magic,
            movie: r.movie,
        },
        origins: r.origins,
    })
}

/// The base path of the movie `from_base` imports under the name `movie`.
///
/// Import names are authoring-time paths from EA's Flash source tree, with
/// either separator (RA3 ships `..\common\shell\...\fe_missionBriefingAssets`
/// and `../cafe/mouseComponents/std_MouseButton`). The games ship every movie
/// flat in one archive and resolve by name, so the directory part is dropped —
/// the same thing the reference aux does in `loadAnimation`.
pub fn resolve_movie(from_base: &Path, movie: &str) -> PathBuf {
    let name = movie.rsplit(['/', '\\']).next().unwrap_or(movie);
    from_base.parent().unwrap_or(Path::new(".")).join(name)
}

struct Resolver<'a> {
    loaded: HashMap<PathBuf, AptFile>,
    /// (source movie base, source index) -> index in the merged movie.
    memo: HashMap<(PathBuf, u32), u32>,
    movie: Movie,
    origins: Vec<Origin>,
    warn: &'a mut dyn FnMut(String),
}

impl Resolver<'_> {
    fn load(&mut self, base: &Path) -> Result<&AptFile> {
        if !self.loaded.contains_key(base) {
            let file = AptFile::load(base).map_err(|e| {
                Error::Apt(apt::Error::Other(format!(
                    "loading {}: {e}",
                    base.display()
                )))
            })?;
            self.loaded.insert(base.to_path_buf(), file);
        }
        Ok(&self.loaded[base])
    }

    /// Fill `slot` in the merged movie with `source`'s export named `name`.
    fn import_slot(&mut self, source: &Path, name: &str, slot: u32) -> Result<()> {
        let src_index = self.export_index(source, name)?;
        self.copy(source, src_index, Some(slot))?;
        Ok(())
    }

    fn export_index(&mut self, source: &Path, name: &str) -> Result<u32> {
        let file = self.load(source)?;
        file.movie
            .exports
            .iter()
            .find(|e| e.name == name)
            .map(|e| e.character_id)
            .ok_or_else(|| Error::Apt(apt::Error::Other(format!("no export {name:?}"))))
    }

    /// Copy character `index` of movie `source` into the merged movie,
    /// recursively copying whatever it references. Returns the merged index.
    ///
    /// `target` places the copy in an existing (import) slot; otherwise a new
    /// slot is appended.
    fn copy(&mut self, source: &Path, index: u32, target: Option<u32>) -> Result<u32> {
        let key = (source.to_path_buf(), index);
        if let Some(&existing) = self.memo.get(&key) {
            return Ok(existing);
        }

        // An empty slot in the source is itself an import: follow it instead of
        // copying nothing. This is what makes chains of libraries work.
        let src_file = self.load(source)?;
        let is_empty = matches!(
            src_file.movie.characters.get(index as usize),
            Some(CharacterSlot::Empty) | None
        );
        if is_empty {
            let onward = src_file
                .movie
                .imports
                .iter()
                .find(|i| i.character_id == index)
                .map(|i| (i.movie.clone(), i.name.clone()));
            let Some((movie, name)) = onward else {
                return Err(Error::Apt(apt::Error::Other(format!(
                    "character {index} of {} is empty and not imported",
                    source.display()
                ))));
            };
            let next = resolve_movie(source, &movie);
            let next_index = self.export_index(&next, &name)?;
            let merged = self.copy(&next, next_index, target)?;
            self.memo.insert(key, merged);
            return Ok(merged);
        }

        // Reserve the slot before recursing, so reference cycles terminate.
        let merged_index = match target {
            Some(slot) => slot,
            None => {
                self.movie.characters.push(CharacterSlot::Empty);
                self.origins.push(Origin {
                    base: source.to_path_buf(),
                    index,
                });
                (self.movie.characters.len() - 1) as u32
            }
        };
        self.memo.insert(key, merged_index);
        self.origins[merged_index as usize] = Origin {
            base: source.to_path_buf(),
            index,
        };

        let Some(CharacterSlot::Character(character)) = self
            .load(source)?
            .movie
            .characters
            .get(index as usize)
            .cloned()
        else {
            return Err(Error::Apt(apt::Error::Other(format!(
                "character {index} of {} is not a character",
                source.display()
            ))));
        };

        let remapped = self.remap(source, character)?;
        self.movie.characters[merged_index as usize] = CharacterSlot::Character(remapped);
        Ok(merged_index)
    }

    /// Rewrite every character reference inside `character` (which came from
    /// `source`) to the merged movie's indices.
    fn remap(&mut self, source: &Path, character: Character) -> Result<Character> {
        Ok(match character {
            Character::Sprite(mut sprite) => {
                self.remap_frames(source, &mut sprite.frames)?;
                Character::Sprite(sprite)
            }
            Character::Button(mut button) => {
                for record in &mut button.records {
                    record.character_id = self.copy(source, record.character_id, None)?;
                }
                if let Some(sounds) = &mut button.sounds {
                    for id in [
                        &mut sounds.over_up_to_idle,
                        &mut sounds.idle_to_over_up,
                        &mut sounds.over_up_to_over_down,
                        &mut sounds.over_down_to_over_up,
                    ] {
                        // 0 means "no sound", not character 0.
                        if *id != 0 {
                            *id = self.copy(source, *id, None)?;
                        }
                    }
                }
                Character::Button(button)
            }
            Character::Font(mut font) => {
                for glyph in &mut font.glyphs {
                    *glyph = self.copy(source, *glyph, None)?;
                }
                Character::Font(font)
            }
            Character::Morph(mut morph) => {
                morph.start_character_id = self.copy(source, morph.start_character_id, None)?;
                morph.end_character_id = self.copy(source, morph.end_character_id, None)?;
                Character::Morph(morph)
            }
            Character::Text(mut text) => {
                if text.font_id >= 0 {
                    text.font_id = self.copy(source, text.font_id as u32, None)? as i32;
                }
                Character::Text(text)
            }
            Character::StaticText(mut st) => {
                for record in &mut st.records {
                    if record.font_id >= 0 {
                        record.font_id = self.copy(source, record.font_id as u32, None)? as i32;
                    }
                }
                Character::StaticText(st)
            }
            other => other,
        })
    }

    fn remap_frames(&mut self, source: &Path, frames: &mut [Frame]) -> Result<()> {
        for frame in frames.iter_mut() {
            for control in &mut frame.controls {
                match control {
                    Control::PlaceObject(place) => {
                        if let Some(id) = place.character_id {
                            if id >= 0 {
                                place.character_id =
                                    Some(self.copy(source, id as u32, None)? as i32);
                            }
                        }
                    }
                    Control::InitAction { sprite_id, .. } if *sprite_id >= 0 => {
                        *sprite_id = self.copy(source, *sprite_id as u32, None)? as i32;
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_names_drop_their_authoring_path() {
        let base = Path::new("/game/MainMenu");
        for (import, want) in [
            ("MenuExport", "/game/MenuExport"),
            ("screens/fe_m_assetsGameSetup", "/game/fe_m_assetsGameSetup"),
            (
                ".\\Components\\fe_onlineComponents",
                "/game/fe_onlineComponents",
            ),
            (
                "..\\common\\shell\\fe_missionBriefingAssets",
                "/game/fe_missionBriefingAssets",
            ),
            (
                "../cafe/mouseComponents/std_MouseButton",
                "/game/std_MouseButton",
            ),
        ] {
            assert_eq!(
                resolve_movie(base, import),
                Path::new(want),
                "import {import:?}"
            );
        }
    }
}
