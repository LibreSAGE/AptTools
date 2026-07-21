use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("reading {}: {source}", path.display())]
    ReadFile {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("parsing APT movie {}: {source}", base.display())]
    Parse {
        base: PathBuf,
        source: Box<Error>,
    },

    #[error("not an APT file: bad header tag")]
    BadAptTag,

    #[error("not an APT const file: bad magic")]
    BadConstMagic,

    #[error("unsupported pointer size digit '{0}' in header tag")]
    BadPtrSize(char),

    #[error("read out of bounds at offset {offset:#x} (need {need} bytes, blob is {len})")]
    OutOfBounds {
        offset: usize,
        need: usize,
        len: usize,
    },

    #[error("unterminated string at offset {0:#x}")]
    UnterminatedString(usize),

    #[error("invalid character type {0}")]
    InvalidCharacterType(i32),

    #[error("invalid control type {0}")]
    InvalidControlType(i32),

    #[error("invalid filter id {0}")]
    InvalidFilterId(u32),

    #[error("invalid action opcode {opcode:#04x} at offset {offset:#x}")]
    InvalidOpcode { opcode: u8, offset: usize },

    #[error(
        "branch/block target {target:#x} is not on an instruction boundary (stream at {stream:#x})"
    )]
    BadBranchTarget { target: usize, stream: usize },

    #[error("constant index {index} out of range (table has {count} entries)")]
    BadConstantIndex { index: usize, count: usize },

    #[error("root character is not an Animation (type {0})")]
    RootNotAnimation(i32),

    #[error("string {0:?} contains characters not representable in the APT (latin-1) encoding")]
    NonLatin1String(String),

    #[error("invalid geometry file: {0}")]
    BadGeometry(String),

    #[error("invalid dat file: {0}")]
    BadDat(String),

    #[error("{0}")]
    Other(String),
}
