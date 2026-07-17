//! The `.const` file: `"Apt1"` magic + timestamp, the entry-point offset into
//! the `.apt` blob, and the constant table referenced by Push items.

use crate::error::Error;
use crate::io::{string_to_latin1, Arena, Cursor};
use crate::types::{PtrSize, Value};
use crate::Result;

/// The magic emitted by classic C&C/BFME `swfc` builds (20 bytes incl. the
/// trailing 0x1a and two NULs).
pub const CONST_MAGIC: &[u8; 20] = b"Apt constant file\x1a\0\0";

#[derive(Debug, Clone, PartialEq)]
pub struct ConstFile {
    /// The 20-byte magic/header block, preserved verbatim for faithful re-emit.
    /// Classic files use `"Apt constant file\x1a\0\0"`; the FIFA-era pipeline
    /// used `"Apt1"` + a 16-byte timestamp.
    pub magic: [u8; 20],
    /// Offset of the root character inside the `.apt` blob.
    pub main_character_offset: u64,
    pub constants: Vec<Value>,
}

/// Constant entry type IDs (`AptVirtualFunctionTable_Indices`).
mod entry_type {
    pub const STRING: i32 = 1;
    pub const NONE: i32 = 3;
    pub const REGISTER: i32 = 4;
    pub const BOOLEAN: i32 = 5;
    pub const FLOAT: i32 = 6;
    pub const INTEGER: i32 = 7;
    pub const LOOKUP: i32 = 8;
}

impl ConstFile {
    pub fn read(data: &[u8], ptr_size: PtrSize) -> Result<ConstFile> {
        let mut c = Cursor::new(data, ptr_size);
        if data.len() < 20 {
            return Err(Error::BadConstMagic);
        }
        let mut magic = [0u8; 20];
        magic.copy_from_slice(&data[0..20]);
        c.pos = 20;
        let main_character_offset = c.ptr()?;
        let n_constants = c.i32()?;
        let table_offset = c.ptr()? as usize;

        let mut constants = Vec::with_capacity(n_constants.max(0) as usize);
        let mut tc = c.at(table_offset);
        for _ in 0..n_constants {
            tc.align_ptr();
            let entry_type = tc.i32()?;
            let value = match entry_type {
                entry_type::STRING => {
                    let off = tc.ptr()?;
                    Value::String(tc.string_at(off as usize)?)
                }
                entry_type::NONE => {
                    tc.ptr()?;
                    Value::Undefined
                }
                entry_type::REGISTER => {
                    tc.align_ptr();
                    let v = tc.i32()?;
                    finish_entry(&mut tc)?;
                    Value::Register(v)
                }
                entry_type::BOOLEAN => {
                    tc.align_ptr();
                    let v = tc.i32()?;
                    finish_entry(&mut tc)?;
                    Value::Boolean(v != 0)
                }
                entry_type::FLOAT => {
                    tc.align_ptr();
                    let v = tc.f32()?;
                    finish_entry(&mut tc)?;
                    Value::Float(v)
                }
                entry_type::INTEGER => {
                    tc.align_ptr();
                    let v = tc.i32()?;
                    finish_entry(&mut tc)?;
                    Value::Integer(v)
                }
                entry_type::LOOKUP => {
                    tc.align_ptr();
                    let v = tc.u32()?;
                    finish_entry(&mut tc)?;
                    Value::Lookup(v)
                }
                t => return Err(Error::Other(format!("invalid constant entry type {t}"))),
            };
            constants.push(value);
        }
        Ok(ConstFile {
            magic,
            main_character_offset,
            constants,
        })
    }

    /// Serialize: `[header][entry table][string pool]`.
    pub fn write(&self, ptr_size: PtrSize) -> Result<Vec<u8>> {
        let mut a = Arena::new(ptr_size);
        a.bytes(&self.magic);
        a.align_ptr();
        a.ptr_value(self.main_character_offset);
        a.i32(self.constants.len() as i32);
        let table_patch = a.ptr_patch();

        a.align_ptr();
        let table_off = a.len() as u64;
        a.patch_ptr(table_patch, table_off);
        let mut string_patches = Vec::new();
        for value in &self.constants {
            a.align_ptr();
            match value {
                Value::String(s) => {
                    a.i32(entry_type::STRING);
                    string_patches.push((a.ptr_patch(), s.clone()));
                }
                Value::Undefined => {
                    a.i32(entry_type::NONE);
                    a.ptr_value(0);
                }
                Value::Register(v) => {
                    a.i32(entry_type::REGISTER);
                    write_int_payload(&mut a, *v as u32);
                }
                Value::Boolean(v) => {
                    a.i32(entry_type::BOOLEAN);
                    write_int_payload(&mut a, *v as u32);
                }
                Value::Float(v) => {
                    a.i32(entry_type::FLOAT);
                    write_int_payload(&mut a, v.to_bits());
                }
                Value::Integer(v) => {
                    a.i32(entry_type::INTEGER);
                    write_int_payload(&mut a, *v as u32);
                }
                Value::Lookup(v) => {
                    a.i32(entry_type::LOOKUP);
                    write_int_payload(&mut a, *v);
                }
            }
        }
        // String pool, deduplicated.
        let mut offsets: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        for (patch, s) in string_patches {
            let off = match offsets.get(&s) {
                Some(&off) => off,
                None => {
                    let off = a.len() as u64;
                    a.bytes(&string_to_latin1(&s)?);
                    a.u8(0);
                    offsets.insert(s, off);
                    off
                }
            };
            a.patch_ptr(patch, off);
        }
        Ok(a.buf)
    }
}

/// Non-string payloads occupy the low 4 bytes of the pointer-width union slot.
fn write_int_payload(a: &mut Arena, v: u32) {
    a.align_ptr();
    match a.ptr_size {
        PtrSize::Four => a.u32(v),
        PtrSize::Eight => a.u64(v as u64),
    }
}

/// After reading the 4-byte payload of a pointer-width union slot, skip the
/// high half in 64-bit files.
fn finish_entry(c: &mut Cursor) -> Result<()> {
    if c.ptr_size == PtrSize::Eight {
        c.u32()?;
    }
    Ok(())
}
