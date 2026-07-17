//! Byte-level reading/writing helpers.
//!
//! APT files are little-endian memory images: every "pointer" is a 4- or
//! 8-byte slot holding a file-relative offset (0 = NULL), a character index,
//! or a magic value. Alignment matters: pointer slots and inline action
//! structs are aligned to the file's pointer size *on blob-relative offsets*.

use crate::error::Error;
use crate::types::PtrSize;
use crate::Result;

/// Read cursor over an APT blob.
#[derive(Clone)]
pub struct Cursor<'a> {
    pub data: &'a [u8],
    pub pos: usize,
    pub ptr_size: PtrSize,
}

impl<'a> Cursor<'a> {
    pub fn new(data: &'a [u8], ptr_size: PtrSize) -> Self {
        Cursor {
            data,
            pos: 0,
            ptr_size,
        }
    }

    pub fn at(&self, pos: usize) -> Cursor<'a> {
        Cursor {
            data: self.data,
            pos,
            ptr_size: self.ptr_size,
        }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos + n > self.data.len() {
            return Err(Error::OutOfBounds {
                offset: self.pos,
                need: n,
                len: self.data.len(),
            });
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn align(&mut self, n: usize) {
        self.pos = (self.pos + n - 1) & !(n - 1);
    }

    pub fn align_ptr(&mut self) {
        self.align(self.ptr_size.bytes());
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn i16(&mut self) -> Result<i16> {
        Ok(self.u16()? as i16)
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn i32(&mut self) -> Result<i32> {
        Ok(self.u32()? as i32)
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.u32()?))
    }

    /// Read a pointer-width slot (aligned). Returns the raw value
    /// (an offset, an index, or a magic).
    pub fn ptr(&mut self) -> Result<u64> {
        self.align_ptr();
        match self.ptr_size {
            PtrSize::Four => Ok(self.u32()? as u64),
            PtrSize::Eight => self.u64(),
        }
    }

    /// Read a NUL-terminated latin-1 string at an absolute offset.
    pub fn string_at(&self, offset: usize) -> Result<String> {
        let data = self.data;
        if offset >= data.len() {
            return Err(Error::OutOfBounds {
                offset,
                need: 1,
                len: data.len(),
            });
        }
        let end = data[offset..]
            .iter()
            .position(|&b| b == 0)
            .ok_or(Error::UnterminatedString(offset))?;
        Ok(latin1_to_string(&data[offset..offset + end]))
    }

    /// Read a pointer slot that must be a string offset; 0 yields "".
    pub fn ptr_string(&mut self) -> Result<String> {
        let off = self.ptr()?;
        if off == 0 {
            Ok(String::new())
        } else {
            self.string_at(off as usize)
        }
    }
}

pub fn latin1_to_string(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

pub fn string_to_latin1(s: &str) -> Result<Vec<u8>> {
    s.chars()
        .map(|c| {
            let v = c as u32;
            if v <= 0xFF {
                Ok(v as u8)
            } else {
                Err(Error::NonLatin1String(s.to_string()))
            }
        })
        .collect()
}

/// A pointer slot in the output blob awaiting its final value.
#[derive(Debug, Clone, Copy)]
#[must_use]
pub struct Patch(usize);

/// Growable output blob with pointer-slot patching.
pub struct Arena {
    pub buf: Vec<u8>,
    pub ptr_size: PtrSize,
}

impl Arena {
    pub fn new(ptr_size: PtrSize) -> Self {
        Arena {
            buf: Vec::new(),
            ptr_size,
        }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn align(&mut self, n: usize) {
        let target = (self.buf.len() + n - 1) & !(n - 1);
        self.buf.resize(target, 0);
    }

    pub fn align_ptr(&mut self) {
        self.align(self.ptr_size.bytes());
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn i16(&mut self, v: i16) {
        self.u16(v as u16);
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn i32(&mut self, v: i32) {
        self.u32(v as u32);
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn f32(&mut self, v: f32) {
        self.u32(v.to_bits());
    }

    pub fn bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    /// Write a pointer-width slot holding a known raw value (index/magic/0).
    pub fn ptr_value(&mut self, v: u64) {
        self.align_ptr();
        match self.ptr_size {
            PtrSize::Four => self.u32(v as u32),
            PtrSize::Eight => self.u64(v),
        }
    }

    /// Reserve a pointer-width slot to be patched later with an offset.
    pub fn ptr_patch(&mut self) -> Patch {
        self.align_ptr();
        let pos = self.buf.len();
        self.ptr_value(0);
        Patch(pos)
    }

    /// Reserve a 4-byte int to be patched later.
    pub fn i32_patch(&mut self) -> Patch {
        let pos = self.buf.len();
        self.u32(0);
        Patch(pos)
    }

    pub fn patch_ptr(&mut self, p: Patch, v: u64) {
        match self.ptr_size {
            PtrSize::Four => self.buf[p.0..p.0 + 4].copy_from_slice(&(v as u32).to_le_bytes()),
            PtrSize::Eight => self.buf[p.0..p.0 + 8].copy_from_slice(&v.to_le_bytes()),
        }
    }

    pub fn patch_i32(&mut self, p: Patch, v: i32) {
        self.buf[p.0..p.0 + 4].copy_from_slice(&(v as u32).to_le_bytes());
    }

    /// Append a NUL-terminated latin-1 string, returning its offset.
    pub fn string(&mut self, s: &str) -> Result<u64> {
        let off = self.buf.len() as u64;
        let bytes = string_to_latin1(s)?;
        self.buf.extend_from_slice(&bytes);
        self.buf.push(0);
        Ok(off)
    }
}

/// Out-of-line data referenced from structs being serialized (strings, pointer
/// arrays, function parameter tables). Collected while writing a struct or
/// action stream, then flushed after it so the stream bytes stay contiguous.
#[derive(Default)]
pub struct Deferred {
    strings: Vec<(Patch, String)>,
    /// Arrays of pointer-width slots holding raw values (indices etc.).
    ptr_arrays: Vec<(Patch, Vec<u64>)>,
    /// Arrays of pointer-width slots, each pointing to a string.
    string_arrays: Vec<(Patch, Vec<String>)>,
    /// `AptRegisterParam[]`: (register, param name) pairs.
    reg_params: Vec<(Patch, Vec<(u32, String)>)>,
}

impl Deferred {
    pub fn string(&mut self, arena: &mut Arena, s: &str) {
        let p = arena.ptr_patch();
        self.strings.push((p, s.to_string()));
    }

    pub fn ptr_array(&mut self, arena: &mut Arena, values: Vec<u64>) {
        let p = arena.ptr_patch();
        self.ptr_arrays.push((p, values));
    }

    pub fn string_array(&mut self, arena: &mut Arena, values: Vec<String>) {
        let p = arena.ptr_patch();
        self.string_arrays.push((p, values));
    }

    pub fn reg_params(&mut self, arena: &mut Arena, params: Vec<(u32, String)>) {
        let p = arena.ptr_patch();
        self.reg_params.push((p, params));
    }

    /// Emit all deferred data into the arena and patch the referring slots.
    pub fn flush(&mut self, arena: &mut Arena) -> Result<()> {
        for (patch, values) in std::mem::take(&mut self.ptr_arrays) {
            arena.align_ptr();
            let off = arena.len() as u64;
            for v in values {
                arena.ptr_value(v);
            }
            arena.patch_ptr(patch, off);
        }
        for (patch, values) in std::mem::take(&mut self.string_arrays) {
            arena.align_ptr();
            let off = arena.len() as u64;
            let mut slots = Vec::with_capacity(values.len());
            for _ in &values {
                slots.push(arena.ptr_patch());
            }
            arena.patch_ptr(patch, off);
            for (slot, s) in slots.into_iter().zip(values) {
                self.strings.push((slot, s));
            }
        }
        for (patch, params) in std::mem::take(&mut self.reg_params) {
            arena.align_ptr();
            let off = arena.len() as u64;
            let mut slots = Vec::with_capacity(params.len());
            for (register, _) in &params {
                arena.align_ptr();
                arena.u32(*register);
                slots.push(arena.ptr_patch());
            }
            arena.patch_ptr(patch, off);
            for (slot, (_, name)) in slots.into_iter().zip(params) {
                self.strings.push((slot, name));
            }
        }
        // Strings last: deduplicate identical strings.
        let strings = std::mem::take(&mut self.strings);
        let mut offsets: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        for (patch, s) in strings {
            let off = match offsets.get(&s) {
                Some(&off) => off,
                None => {
                    let off = arena.string(&s)?;
                    offsets.insert(s, off);
                    off
                }
            };
            arena.patch_ptr(patch, off);
        }
        Ok(())
    }
}
