//! APT ActionScript bytecode: decoding and encoding of action streams.
//!
//! A stream is `[opcode: u8][payload]*` terminated by `End` (0x00). APT keeps
//! the SWF opcode numbers but re-encodes payloads as C structs aligned to the
//! file's pointer size (on blob-relative offsets); 1/2/4-byte immediates are
//! unaligned. EA adds shorthand opcodes (0x56-0x77) and constant-dictionary
//! variants (0xA1-0xB9).

use crate::error::Error;
use crate::io::{Arena, Cursor, Deferred, Patch};
use crate::types::{Value, FUNC_POOL_ARRAY_MAGIC, FUNC_POOL_ITEMS_MAGIC};
use crate::Result;

/// A decoded action stream. The terminating `End` is stored explicitly (as are
/// any `End`s inside function bodies), so encoding is verbatim.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ActionStream {
    pub instructions: Vec<Instruction>,
}

/// Where a caught exception goes for `Try`.
#[derive(Debug, Clone, PartialEq)]
pub enum CatchVar {
    Register(u8),
    Variable(String),
}

/// Branch/`With`/`Try` extents are expressed in *instruction indices* into the
/// enclosing stream (an index equal to `instructions.len()` means "past the
/// end"); byte deltas are recomputed on encode, which is what makes streams
/// portable across pointer sizes.
#[derive(Debug, Clone, PartialEq)]
pub enum Instruction {
    End,
    /// Any opcode without payload (standard SWF ops and EA shorthands).
    Simple(u8),
    GotoFrame(i32),
    GetUrl {
        url: String,
        target: String,
    },
    StoreRegister(i32),
    SetTarget(String),
    GotoLabel(String),
    GotoFrame2 {
        play: bool,
    },
    Push(Vec<Value>),
    /// Same payload as `Push`; establishes the active constant dictionary.
    DefineDictionary(Vec<Value>),
    DefineFunction {
        name: String,
        params: Vec<String>,
        body: ActionStream,
    },
    DefineFunction2 {
        name: String,
        /// (register, name) per parameter.
        params: Vec<(u32, String)>,
        register_count: i16,
        /// `AptDefinefunction2FlagsType` bits (preloadThis, suppressThis, ...).
        flags: i16,
        body: ActionStream,
    },
    Try {
        /// Instruction counts of the three blocks following this instruction.
        try_count: usize,
        catch_count: usize,
        finally_count: usize,
        has_catch: bool,
        has_finally: bool,
        catch_var: Option<CatchVar>,
    },
    With {
        /// Index of the first instruction past the with-block.
        end_target: usize,
    },
    BranchAlways {
        target: usize,
    },
    BranchIfTrue {
        target: usize,
    },
    BranchIfFalse {
        target: usize,
    },
    /// EA 0x77: 4 opaque bytes.
    TraceStart(u32),
    /// EA 0xA1/0xA4-0xA7: push an inline string, optionally with a follow-up op.
    PushString {
        kind: PushStringKind,
        value: String,
    },
    /// EA dictionary-index opcodes: reference the active constant pool.
    DictRef {
        kind: DictRefKind,
        index: u16,
    },
    PushFloat(f32),
    PushByte(u8),
    PushWord(u16),
    PushDWord(u32),
    PushRegister(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushStringKind {
    Push,      // 0xA1
    GetVar,    // 0xA4
    GetMember, // 0xA5
    SetVar,    // 0xA6
    SetMember, // 0xA7
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictRefKind {
    PushByte,         // 0xA2 (1-byte index)
    PushWord,         // 0xA3 (2-byte index)
    ByteGetVar,       // 0xAE
    ByteGetMember,    // 0xAF
    CallFuncPop,      // 0xB0
    CallFuncSetVar,   // 0xB1
    CallMethodPop,    // 0xB2
    CallMethodSetVar, // 0xB3
}

impl DictRefKind {
    fn opcode(self) -> u8 {
        match self {
            DictRefKind::PushByte => 0xA2,
            DictRefKind::PushWord => 0xA3,
            DictRefKind::ByteGetVar => 0xAE,
            DictRefKind::ByteGetMember => 0xAF,
            DictRefKind::CallFuncPop => 0xB0,
            DictRefKind::CallFuncSetVar => 0xB1,
            DictRefKind::CallMethodPop => 0xB2,
            DictRefKind::CallMethodSetVar => 0xB3,
        }
    }

    fn is_word(self) -> bool {
        self == DictRefKind::PushWord
    }
}

impl PushStringKind {
    fn opcode(self) -> u8 {
        match self {
            PushStringKind::Push => 0xA1,
            PushStringKind::GetVar => 0xA4,
            PushStringKind::GetMember => 0xA5,
            PushStringKind::SetVar => 0xA6,
            PushStringKind::SetMember => 0xA7,
        }
    }
}

pub mod opcode {
    pub const END: u8 = 0x00;
    pub const NEXT_FRAME: u8 = 0x04;
    pub const PREV_FRAME: u8 = 0x05;
    pub const PLAY: u8 = 0x06;
    pub const STOP: u8 = 0x07;
    pub const TOGGLE_QUALITY: u8 = 0x08;
    pub const STOP_SOUNDS: u8 = 0x09;
    pub const ADD: u8 = 0x0A;
    pub const SUBTRACT: u8 = 0x0B;
    pub const MULTIPLY: u8 = 0x0C;
    pub const DIVIDE: u8 = 0x0D;
    pub const EQUALS: u8 = 0x0E;
    pub const LESS_THAN: u8 = 0x0F;
    pub const AND: u8 = 0x10;
    pub const OR: u8 = 0x11;
    pub const NOT: u8 = 0x12;
    pub const STRING_EQUALS: u8 = 0x13;
    pub const STRING_LENGTH: u8 = 0x14;
    pub const SUB_STRING: u8 = 0x15;
    pub const POP: u8 = 0x17;
    pub const TO_INTEGER: u8 = 0x18;
    pub const GET_VARIABLE: u8 = 0x1C;
    pub const SET_VARIABLE: u8 = 0x1D;
    pub const SET_TARGET2: u8 = 0x20;
    pub const STRING_ADD: u8 = 0x21;
    pub const GET_PROPERTY: u8 = 0x22;
    pub const SET_PROPERTY: u8 = 0x23;
    pub const CLONE_SPRITE: u8 = 0x24;
    pub const REMOVE_SPRITE: u8 = 0x25;
    pub const TRACE: u8 = 0x26;
    pub const START_DRAG: u8 = 0x27;
    pub const STOP_DRAG: u8 = 0x28;
    pub const STRING_LESS_THAN: u8 = 0x29;
    pub const THROW: u8 = 0x2A;
    pub const CAST_OP: u8 = 0x2B;
    pub const IMPLEMENTS_OP: u8 = 0x2C;
    pub const RANDOM: u8 = 0x30;
    pub const MB_LENGTH: u8 = 0x31;
    pub const CHAR_TO_ASCII: u8 = 0x32;
    pub const ASCII_TO_CHAR: u8 = 0x33;
    pub const GET_TIMER: u8 = 0x34;
    pub const MB_SUB_STRING: u8 = 0x35;
    pub const MB_CHAR_TO_ASCII: u8 = 0x36;
    pub const MB_ASCII_TO_CHAR: u8 = 0x37;
    pub const DELETE: u8 = 0x3A;
    pub const DELETE2: u8 = 0x3B;
    pub const DEFINE_LOCAL: u8 = 0x3C;
    pub const CALL_FUNCTION: u8 = 0x3D;
    pub const RETURN: u8 = 0x3E;
    pub const MODULO: u8 = 0x3F;
    pub const NEW_OBJECT: u8 = 0x40;
    pub const DEFINE_LOCAL2: u8 = 0x41;
    pub const INIT_ARRAY: u8 = 0x42;
    pub const INIT_OBJECT: u8 = 0x43;
    pub const TYPE_OF: u8 = 0x44;
    pub const TARGET_PATH: u8 = 0x45;
    pub const ENUMERATE: u8 = 0x46;
    pub const ADD2: u8 = 0x47;
    pub const LESS_THAN2: u8 = 0x48;
    pub const EQUALS2: u8 = 0x49;
    pub const TO_NUMBER: u8 = 0x4A;
    pub const TO_STRING: u8 = 0x4B;
    pub const PUSH_DUPLICATE: u8 = 0x4C;
    pub const STACK_SWAP: u8 = 0x4D;
    pub const GET_MEMBER: u8 = 0x4E;
    pub const SET_MEMBER: u8 = 0x4F;
    pub const INCREMENT: u8 = 0x50;
    pub const DECREMENT: u8 = 0x51;
    pub const CALL_METHOD: u8 = 0x52;
    pub const NEW_METHOD: u8 = 0x53;
    pub const INSTANCE_OF: u8 = 0x54;
    pub const ENUMERATE2: u8 = 0x55;
    // EA shorthands.
    pub const EA_PUSH_THIS: u8 = 0x56;
    pub const EA_PUSH_GLOBAL: u8 = 0x58;
    pub const EA_PUSH_ZERO: u8 = 0x59;
    pub const EA_PUSH_ONE: u8 = 0x5A;
    pub const EA_CALL_FUNC_POP: u8 = 0x5B;
    pub const EA_CALL_FUNC_SET_VAR: u8 = 0x5C;
    pub const EA_CALL_METHOD_POP: u8 = 0x5D;
    pub const EA_CALL_METHOD_SET_VAR: u8 = 0x5E;
    pub const BIT_AND: u8 = 0x60;
    pub const BIT_OR: u8 = 0x61;
    pub const BIT_XOR: u8 = 0x62;
    pub const BIT_LSHIFT: u8 = 0x63;
    pub const BIT_RSHIFT: u8 = 0x64;
    pub const BIT_URSHIFT: u8 = 0x65;
    pub const STRICT_EQUALS: u8 = 0x66;
    pub const GREATER: u8 = 0x67;
    pub const EXTENDS: u8 = 0x69;
    pub const EA_PUSH_THIS_VAR: u8 = 0x70;
    pub const EA_PUSH_GLOBAL_VAR: u8 = 0x71;
    pub const EA_PUSH_ZERO_SET_VAR: u8 = 0x72;
    pub const EA_PUSH_TRUE: u8 = 0x73;
    pub const EA_PUSH_FALSE: u8 = 0x74;
    pub const EA_PUSH_NULL: u8 = 0x75;
    pub const EA_PUSH_UNDEFINED: u8 = 0x76;
    pub const EA_TRACE_START: u8 = 0x77;
    pub const GOTO_FRAME: u8 = 0x81;
    pub const GET_URL: u8 = 0x83;
    pub const STORE_REGISTER: u8 = 0x87;
    pub const DEFINE_DICTIONARY: u8 = 0x88;
    pub const WAIT_FOR_FRAME: u8 = 0x8A;
    pub const SET_TARGET: u8 = 0x8B;
    pub const GOTO_LABEL: u8 = 0x8C;
    pub const DEFINE_FUNCTION2: u8 = 0x8E;
    pub const TRY: u8 = 0x8F;
    pub const WITH: u8 = 0x94;
    pub const PUSH: u8 = 0x96;
    pub const BRANCH_ALWAYS: u8 = 0x99;
    pub const GET_URL2: u8 = 0x9A;
    pub const DEFINE_FUNCTION: u8 = 0x9B;
    pub const BRANCH_IF_TRUE: u8 = 0x9D;
    pub const CALL_FRAME: u8 = 0x9E;
    pub const GOTO_FRAME2: u8 = 0x9F;
    pub const EA_BRANCH_IF_FALSE: u8 = 0xB8;
    pub const EA_PUSH_REGISTER: u8 = 0xB9;
    pub const BREAKPOINT: u8 = 0xBA;
}

/// Is `op` a valid opcode that carries no payload?
fn is_simple(op: u8) -> bool {
    use opcode::*;
    matches!(
        op,
        NEXT_FRAME
            | PREV_FRAME
            | PLAY
            | STOP
            | TOGGLE_QUALITY
            | STOP_SOUNDS
            | ADD
            | SUBTRACT
            | MULTIPLY
            | DIVIDE
            | EQUALS
            | LESS_THAN
            | AND
            | OR
            | NOT
            | STRING_EQUALS
            | STRING_LENGTH
            | SUB_STRING
            | POP
            | TO_INTEGER
            | GET_VARIABLE
            | SET_VARIABLE
            | SET_TARGET2
            | STRING_ADD
            | GET_PROPERTY
            | SET_PROPERTY
            | CLONE_SPRITE
            | REMOVE_SPRITE
            | TRACE
            | START_DRAG
            | STOP_DRAG
            | STRING_LESS_THAN
            | THROW
            | CAST_OP
            | IMPLEMENTS_OP
            | RANDOM
            | MB_LENGTH
            | CHAR_TO_ASCII
            | ASCII_TO_CHAR
            | GET_TIMER
            | MB_SUB_STRING
            | MB_CHAR_TO_ASCII
            | MB_ASCII_TO_CHAR
            | DELETE
            | DELETE2
            | DEFINE_LOCAL
            | CALL_FUNCTION
            | RETURN
            | MODULO
            | NEW_OBJECT
            | DEFINE_LOCAL2
            | INIT_ARRAY
            | INIT_OBJECT
            | TYPE_OF
            | TARGET_PATH
            | ENUMERATE
            | ADD2
            | LESS_THAN2
            | EQUALS2
            | TO_NUMBER
            | TO_STRING
            | PUSH_DUPLICATE
            | STACK_SWAP
            | GET_MEMBER
            | SET_MEMBER
            | INCREMENT
            | DECREMENT
            | CALL_METHOD
            | NEW_METHOD
            | INSTANCE_OF
            | ENUMERATE2
            | EA_PUSH_THIS
            | EA_PUSH_GLOBAL
            | EA_PUSH_ZERO
            | EA_PUSH_ONE
            | EA_CALL_FUNC_POP
            | EA_CALL_FUNC_SET_VAR
            | EA_CALL_METHOD_POP
            | EA_CALL_METHOD_SET_VAR
            | BIT_AND
            | BIT_OR
            | BIT_XOR
            | BIT_LSHIFT
            | BIT_RSHIFT
            | BIT_URSHIFT
            | STRICT_EQUALS
            | GREATER
            | EXTENDS
            | EA_PUSH_THIS_VAR
            | EA_PUSH_GLOBAL_VAR
            | EA_PUSH_ZERO_SET_VAR
            | EA_PUSH_TRUE
            | EA_PUSH_FALSE
            | EA_PUSH_NULL
            | EA_PUSH_UNDEFINED
            | WAIT_FOR_FRAME
            | GET_URL2
            | CALL_FRAME
            | BREAKPOINT
    )
}

/// Human-readable opcode name (for disassembly / aptinfo).
pub fn opcode_name(op: u8) -> &'static str {
    use opcode::*;
    match op {
        END => "End",
        NEXT_FRAME => "NextFrame",
        PREV_FRAME => "PrevFrame",
        PLAY => "Play",
        STOP => "Stop",
        TOGGLE_QUALITY => "ToggleQuality",
        STOP_SOUNDS => "StopSounds",
        ADD => "Add",
        SUBTRACT => "Subtract",
        MULTIPLY => "Multiply",
        DIVIDE => "Divide",
        EQUALS => "Equals",
        LESS_THAN => "LessThan",
        AND => "And",
        OR => "Or",
        NOT => "Not",
        STRING_EQUALS => "StringEquals",
        STRING_LENGTH => "StringLength",
        SUB_STRING => "SubString",
        POP => "Pop",
        TO_INTEGER => "ToInteger",
        GET_VARIABLE => "GetVariable",
        SET_VARIABLE => "SetVariable",
        SET_TARGET2 => "SetTarget2",
        STRING_ADD => "StringAdd",
        GET_PROPERTY => "GetProperty",
        SET_PROPERTY => "SetProperty",
        CLONE_SPRITE => "CloneSprite",
        REMOVE_SPRITE => "RemoveSprite",
        TRACE => "Trace",
        START_DRAG => "StartDragMovie",
        STOP_DRAG => "StopDragMovie",
        STRING_LESS_THAN => "StringLessThan",
        THROW => "Throw",
        CAST_OP => "CastOp",
        IMPLEMENTS_OP => "ImplementsOp",
        RANDOM => "Random",
        MB_LENGTH => "MBLength",
        CHAR_TO_ASCII => "CharToAscii",
        ASCII_TO_CHAR => "AsciiToChar",
        GET_TIMER => "GetTimer",
        MB_SUB_STRING => "MBSubString",
        MB_CHAR_TO_ASCII => "MBCharToAscii",
        MB_ASCII_TO_CHAR => "MBAsciiToChar",
        DELETE => "Delete",
        DELETE2 => "Delete2",
        DEFINE_LOCAL => "DefineLocal",
        CALL_FUNCTION => "CallFunction",
        RETURN => "Return",
        MODULO => "Modulo",
        NEW_OBJECT => "NewObject",
        DEFINE_LOCAL2 => "DefineLocal2",
        INIT_ARRAY => "InitArray",
        INIT_OBJECT => "InitObject",
        TYPE_OF => "TypeOf",
        TARGET_PATH => "TargetPath",
        ENUMERATE => "Enumerate",
        ADD2 => "Add2",
        LESS_THAN2 => "LessThan2",
        EQUALS2 => "Equals2",
        TO_NUMBER => "ToNumber",
        TO_STRING => "ToString",
        PUSH_DUPLICATE => "PushDuplicate",
        STACK_SWAP => "StackSwap",
        GET_MEMBER => "GetMember",
        SET_MEMBER => "SetMember",
        INCREMENT => "Increment",
        DECREMENT => "Decrement",
        CALL_METHOD => "CallMethod",
        NEW_METHOD => "NewMethod",
        INSTANCE_OF => "InstanceOf",
        ENUMERATE2 => "Enumerate2",
        EA_PUSH_THIS => "EA:PushThis",
        EA_PUSH_GLOBAL => "EA:PushGlobal",
        EA_PUSH_ZERO => "EA:Push0",
        EA_PUSH_ONE => "EA:Push1",
        EA_CALL_FUNC_POP => "EA:CallFuncAndPop",
        EA_CALL_FUNC_SET_VAR => "EA:CallFuncSetVar",
        EA_CALL_METHOD_POP => "EA:CallMethodPop",
        EA_CALL_METHOD_SET_VAR => "EA:CallMethodSetVar",
        BIT_AND => "BitAnd",
        BIT_OR => "BitOr",
        BIT_XOR => "BitXor",
        BIT_LSHIFT => "BitLShift",
        BIT_RSHIFT => "BitRShift",
        BIT_URSHIFT => "BitURShift",
        STRICT_EQUALS => "StrictEquals",
        GREATER => "Greater",
        EXTENDS => "Extends",
        EA_PUSH_THIS_VAR => "EA:PushThisVariable",
        EA_PUSH_GLOBAL_VAR => "EA:PushGlobalVariable",
        EA_PUSH_ZERO_SET_VAR => "EA:PushZeroSetVar",
        EA_PUSH_TRUE => "EA:PushTrue",
        EA_PUSH_FALSE => "EA:PushFalse",
        EA_PUSH_NULL => "EA:PushNull",
        EA_PUSH_UNDEFINED => "EA:PushUndefined",
        EA_TRACE_START => "EA:TraceStart",
        GOTO_FRAME => "GotoFrame",
        GET_URL => "GetUrl",
        STORE_REGISTER => "StoreRegister",
        DEFINE_DICTIONARY => "EA:DefineDictionary",
        WAIT_FOR_FRAME => "WaitForFrame",
        SET_TARGET => "SetTarget",
        GOTO_LABEL => "GotoLabel",
        DEFINE_FUNCTION2 => "DefineFunction2",
        TRY => "Try",
        WITH => "With",
        PUSH => "Push",
        BRANCH_ALWAYS => "BranchAlways",
        GET_URL2 => "GetUrl2",
        DEFINE_FUNCTION => "DefineFunction",
        BRANCH_IF_TRUE => "BranchIfTrue",
        CALL_FRAME => "CallFrame",
        GOTO_FRAME2 => "GotoFrame2",
        0xA1 => "EA:PushString",
        0xA2 => "EA:PushStringDictByte",
        0xA3 => "EA:PushStringDictWord",
        0xA4 => "EA:PushStringGetVar",
        0xA5 => "EA:PushStringGetMember",
        0xA6 => "EA:PushStringSetVar",
        0xA7 => "EA:PushStringSetMember",
        0xAE => "EA:StringDictByteGetVar",
        0xAF => "EA:StringDictByteGetMember",
        0xB0 => "EA:DictCallFuncPop",
        0xB1 => "EA:DictCallFuncSetVar",
        0xB2 => "EA:DictCallMethodPop",
        0xB3 => "EA:DictCallMethodSetVar",
        0xB4 => "EA:PushFloat",
        0xB5 => "EA:PushByte",
        0xB6 => "EA:PushWord",
        0xB7 => "EA:PushDWord",
        EA_BRANCH_IF_FALSE => "EA:BranchIfFalse",
        EA_PUSH_REGISTER => "EA:PushRegister",
        BREAKPOINT => "Breakpoint",
        _ => "<invalid>",
    }
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

/// Byte-offset quantities remembered during the linear pass, resolved to
/// instruction indices once all boundaries are known.
enum RawFix {
    None,
    /// Branch / With: absolute target offset in the blob.
    Target(usize),
    /// Try: absolute end offsets of the try, catch and finally blocks.
    TryEnds(usize, usize, usize),
}

/// Decode the action stream starting at `offset` in the blob.
pub(crate) fn decode_stream(cur: &Cursor, offset: usize, consts: &[Value]) -> Result<ActionStream> {
    let mut c = cur.at(offset);
    let instructions = decode_block(&mut c, None, consts)?;
    Ok(ActionStream { instructions })
}

/// Decode instructions until `End` (when `end` is `None`) or until the byte
/// extent `[.., end)` is consumed (function bodies).
fn decode_block(c: &mut Cursor, end: Option<usize>, consts: &[Value]) -> Result<Vec<Instruction>> {
    let stream_start = c.pos;
    let mut instructions = Vec::new();
    let mut boundaries = Vec::new();
    let mut fixes = Vec::new();

    loop {
        if let Some(e) = end {
            if c.pos >= e {
                break;
            }
        }
        boundaries.push(c.pos);
        let op_offset = c.pos;
        let op = c.u8()?;
        let mut fix = RawFix::None;
        let insn = match op {
            opcode::END => Instruction::End,
            opcode::GOTO_FRAME => {
                c.align_ptr();
                Instruction::GotoFrame(c.i32()?)
            }
            opcode::GET_URL => {
                c.align_ptr();
                let url = c.ptr_string()?;
                let target = c.ptr_string()?;
                Instruction::GetUrl { url, target }
            }
            opcode::STORE_REGISTER => {
                c.align_ptr();
                Instruction::StoreRegister(c.i32()?)
            }
            opcode::SET_TARGET => {
                c.align_ptr();
                Instruction::SetTarget(c.ptr_string()?)
            }
            opcode::GOTO_LABEL => {
                c.align_ptr();
                Instruction::GotoLabel(c.ptr_string()?)
            }
            opcode::GOTO_FRAME2 => {
                c.align_ptr();
                Instruction::GotoFrame2 {
                    play: c.i32()? != 0,
                }
            }
            opcode::PUSH | opcode::DEFINE_DICTIONARY => {
                let items = decode_pool_items(c, consts)?;
                if op == opcode::PUSH {
                    Instruction::Push(items)
                } else {
                    Instruction::DefineDictionary(items)
                }
            }
            opcode::DEFINE_FUNCTION => {
                c.align_ptr();
                let name = c.ptr_string()?;
                c.align(4);
                let n_params = c.i32()?;
                let params_off = c.ptr()?;
                let mut params = Vec::with_capacity(n_params.max(0) as usize);
                if n_params > 0 {
                    let mut pc = c.at(params_off as usize);
                    for _ in 0..n_params {
                        params.push(pc.ptr_string()?);
                    }
                }
                c.align(4);
                let code_size = c.i32()?;
                skip_func_pool(c)?;
                let body_end = c.pos + code_size.max(0) as usize;
                let body = decode_block(c, Some(body_end), consts)?;
                c.pos = body_end;
                Instruction::DefineFunction {
                    name,
                    params,
                    body: ActionStream { instructions: body },
                }
            }
            opcode::DEFINE_FUNCTION2 => {
                c.align_ptr();
                let name = c.ptr_string()?;
                c.align(4);
                let n_params = c.i32()?;
                let register_count = c.i16()?;
                let flags = c.i16()?;
                let params_off = c.ptr()?;
                let mut params = Vec::with_capacity(n_params.max(0) as usize);
                if n_params > 0 {
                    let mut pc = c.at(params_off as usize);
                    for _ in 0..n_params {
                        pc.align_ptr();
                        let register = pc.u32()?;
                        let name = pc.ptr_string()?;
                        params.push((register, name));
                    }
                }
                c.align(4);
                let code_size = c.i32()?;
                skip_func_pool(c)?;
                let body_end = c.pos + code_size.max(0) as usize;
                let body = decode_block(c, Some(body_end), consts)?;
                c.pos = body_end;
                Instruction::DefineFunction2 {
                    name,
                    params,
                    register_count,
                    flags,
                    body: ActionStream { instructions: body },
                }
            }
            opcode::TRY => {
                c.align_ptr();
                let try_size = c.u32()? as usize;
                let catch_size = c.u32()? as usize;
                let finally_size = c.u32()? as usize;
                let flags = c.u8()?;
                c.u8()?;
                c.u8()?;
                let caught_register = c.u8()?;
                let catch_var = if flags & 4 != 0 {
                    c.ptr()?; // raw value, not an offset
                    Some(CatchVar::Register(caught_register))
                } else {
                    let s = c.ptr_string()?;
                    if flags & 1 != 0 {
                        Some(CatchVar::Variable(s))
                    } else {
                        None
                    }
                };
                let after = c.pos;
                fix = RawFix::TryEnds(
                    after + try_size,
                    after + try_size + catch_size,
                    after + try_size + catch_size + finally_size,
                );
                Instruction::Try {
                    try_count: 0,
                    catch_count: 0,
                    finally_count: 0,
                    has_catch: flags & 1 != 0,
                    has_finally: flags & 2 != 0,
                    catch_var,
                }
            }
            opcode::WITH => {
                c.align_ptr();
                let delta = match c.ptr_size {
                    crate::types::PtrSize::Four => c.u32()? as i32 as i64,
                    crate::types::PtrSize::Eight => c.u64()? as i64,
                };
                fix = RawFix::Target((c.pos as i64 + delta) as usize);
                Instruction::With { end_target: 0 }
            }
            opcode::BRANCH_ALWAYS | opcode::BRANCH_IF_TRUE | opcode::EA_BRANCH_IF_FALSE => {
                c.align_ptr();
                let delta = c.i32()? as i64;
                fix = RawFix::Target((c.pos as i64 + delta) as usize);
                match op {
                    opcode::BRANCH_ALWAYS => Instruction::BranchAlways { target: 0 },
                    opcode::BRANCH_IF_TRUE => Instruction::BranchIfTrue { target: 0 },
                    _ => Instruction::BranchIfFalse { target: 0 },
                }
            }
            opcode::EA_TRACE_START => Instruction::TraceStart(unaligned_u32(c)?),
            0xA1 => Instruction::PushString {
                kind: PushStringKind::Push,
                value: aligned_string(c)?,
            },
            0xA4 => Instruction::PushString {
                kind: PushStringKind::GetVar,
                value: aligned_string(c)?,
            },
            0xA5 => Instruction::PushString {
                kind: PushStringKind::GetMember,
                value: aligned_string(c)?,
            },
            0xA6 => Instruction::PushString {
                kind: PushStringKind::SetVar,
                value: aligned_string(c)?,
            },
            0xA7 => Instruction::PushString {
                kind: PushStringKind::SetMember,
                value: aligned_string(c)?,
            },
            0xA2 => Instruction::DictRef {
                kind: DictRefKind::PushByte,
                index: c.u8()? as u16,
            },
            0xA3 => Instruction::DictRef {
                kind: DictRefKind::PushWord,
                index: unaligned_u16(c)?,
            },
            0xAE => Instruction::DictRef {
                kind: DictRefKind::ByteGetVar,
                index: c.u8()? as u16,
            },
            0xAF => Instruction::DictRef {
                kind: DictRefKind::ByteGetMember,
                index: c.u8()? as u16,
            },
            0xB0 => Instruction::DictRef {
                kind: DictRefKind::CallFuncPop,
                index: c.u8()? as u16,
            },
            0xB1 => Instruction::DictRef {
                kind: DictRefKind::CallFuncSetVar,
                index: c.u8()? as u16,
            },
            0xB2 => Instruction::DictRef {
                kind: DictRefKind::CallMethodPop,
                index: c.u8()? as u16,
            },
            0xB3 => Instruction::DictRef {
                kind: DictRefKind::CallMethodSetVar,
                index: c.u8()? as u16,
            },
            0xB4 => Instruction::PushFloat(f32::from_bits(unaligned_u32(c)?)),
            0xB5 => Instruction::PushByte(c.u8()?),
            0xB6 => Instruction::PushWord(unaligned_u16(c)?),
            0xB7 => Instruction::PushDWord(unaligned_u32(c)?),
            opcode::EA_PUSH_REGISTER => Instruction::PushRegister(c.u8()?),
            op if is_simple(op) => Instruction::Simple(op),
            op => {
                return Err(Error::InvalidOpcode {
                    opcode: op,
                    offset: op_offset,
                })
            }
        };
        instructions.push(insn);
        fixes.push(fix);
        if op == opcode::END && end.is_none() {
            break;
        }
    }
    boundaries.push(c.pos);

    // Resolve byte offsets to instruction indices.
    let to_index = |target: usize| -> Result<usize> {
        boundaries
            .binary_search(&target)
            .map_err(|_| Error::BadBranchTarget {
                target,
                stream: stream_start,
            })
    };
    for (i, fix) in fixes.into_iter().enumerate() {
        match fix {
            RawFix::None => {}
            RawFix::Target(t) => {
                let idx = to_index(t)?;
                match &mut instructions[i] {
                    Instruction::With { end_target } => *end_target = idx,
                    Instruction::BranchAlways { target }
                    | Instruction::BranchIfTrue { target }
                    | Instruction::BranchIfFalse { target } => *target = idx,
                    _ => unreachable!(),
                }
            }
            RawFix::TryEnds(t, cch, fin) => {
                let (ti, ci, fi) = (to_index(t)?, to_index(cch)?, to_index(fin)?);
                if let Instruction::Try {
                    try_count,
                    catch_count,
                    finally_count,
                    ..
                } = &mut instructions[i]
                {
                    *try_count = ti - (i + 1);
                    *catch_count = ci - ti;
                    *finally_count = fi - ci;
                }
            }
        }
    }
    Ok(instructions)
}

/// Skip the dead `AptConstantPool` scratch of DefineFunction/2
/// (`{0x98765432, 0x12345678}` magics).
fn skip_func_pool(c: &mut Cursor) -> Result<()> {
    c.align_ptr();
    c.i32()?;
    c.ptr()?;
    Ok(())
}

fn aligned_string(c: &mut Cursor) -> Result<String> {
    c.align_ptr();
    c.ptr_string()
}

fn unaligned_u16(c: &mut Cursor) -> Result<u16> {
    Ok(c.u8()? as u16 | (c.u8()? as u16) << 8)
}

fn unaligned_u32(c: &mut Cursor) -> Result<u32> {
    Ok(c.u8()? as u32 | (c.u8()? as u32) << 8 | (c.u8()? as u32) << 16 | (c.u8()? as u32) << 24)
}

/// `AptConstantPool { int nItems; ptr apItems }` where each item slot holds a
/// constant-table index.
fn decode_pool_items(c: &mut Cursor, consts: &[Value]) -> Result<Vec<Value>> {
    c.align_ptr();
    let n_items = c.i32()?;
    let items_off = c.ptr()?;
    let mut items = Vec::with_capacity(n_items.max(0) as usize);
    if n_items > 0 {
        let mut ic = c.at(items_off as usize);
        for _ in 0..n_items {
            let index = ic.ptr()? as usize;
            let value = consts.get(index).ok_or(Error::BadConstantIndex {
                index,
                count: consts.len(),
            })?;
            items.push(value.clone());
        }
    }
    Ok(items)
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

enum EncodeFix {
    /// (4-byte patch, base offset just past the struct, target instruction index)
    Branch(Patch, usize, usize),
    /// (pointer-width patch, base offset just past the struct, target index)
    With(Patch, usize, usize),
    /// (size patches, base offset, end indices of try/catch/finally)
    Try([Patch; 3], usize, [usize; 3]),
}

/// Encode a stream into the arena; returns its start offset. Push items append
/// to `consts` in encounter order (the engine requires globally sequential
/// constant indices); out-of-line strings/arrays go to `deferred`.
pub(crate) fn encode_stream(
    arena: &mut Arena,
    stream: &ActionStream,
    consts: &mut Vec<Value>,
    deferred: &mut Deferred,
) -> Result<u64> {
    let start = arena.len() as u64;
    encode_block(arena, &stream.instructions, consts, deferred, true)?;
    Ok(start)
}

fn encode_block(
    arena: &mut Arena,
    instructions: &[Instruction],
    consts: &mut Vec<Value>,
    deferred: &mut Deferred,
    implicit_end_if_empty: bool,
) -> Result<()> {
    let mut boundaries = Vec::with_capacity(instructions.len() + 1);
    let mut fixes: Vec<EncodeFix> = Vec::new();

    if instructions.is_empty() && implicit_end_if_empty {
        arena.u8(opcode::END);
        return Ok(());
    }

    for (i, insn) in instructions.iter().enumerate() {
        boundaries.push(arena.len());
        match insn {
            Instruction::End => arena.u8(opcode::END),
            Instruction::Simple(op) => arena.u8(*op),
            Instruction::GotoFrame(frame) => {
                arena.u8(opcode::GOTO_FRAME);
                arena.align_ptr();
                arena.i32(*frame);
            }
            Instruction::GetUrl { url, target } => {
                arena.u8(opcode::GET_URL);
                arena.align_ptr();
                deferred.string(arena, url);
                deferred.string(arena, target);
            }
            Instruction::StoreRegister(register) => {
                arena.u8(opcode::STORE_REGISTER);
                arena.align_ptr();
                arena.i32(*register);
            }
            Instruction::SetTarget(target) => {
                arena.u8(opcode::SET_TARGET);
                arena.align_ptr();
                deferred.string(arena, target);
            }
            Instruction::GotoLabel(label) => {
                arena.u8(opcode::GOTO_LABEL);
                arena.align_ptr();
                deferred.string(arena, label);
            }
            Instruction::GotoFrame2 { play } => {
                arena.u8(opcode::GOTO_FRAME2);
                arena.align_ptr();
                arena.i32(*play as i32);
            }
            Instruction::Push(items) => {
                encode_pool_items(arena, opcode::PUSH, items, consts, deferred)
            }
            Instruction::DefineDictionary(items) => {
                encode_pool_items(arena, opcode::DEFINE_DICTIONARY, items, consts, deferred)
            }
            Instruction::DefineFunction { name, params, body } => {
                arena.u8(opcode::DEFINE_FUNCTION);
                arena.align_ptr();
                deferred.string(arena, name);
                arena.align(4);
                arena.i32(params.len() as i32);
                if params.is_empty() {
                    arena.ptr_value(0);
                } else {
                    deferred.string_array(arena, params.clone());
                }
                arena.align(4);
                let size_patch = arena.i32_patch();
                encode_func_pool(arena);
                let body_start = arena.len();
                encode_block(arena, &body.instructions, consts, deferred, false)?;
                arena.patch_i32(size_patch, (arena.len() - body_start) as i32);
            }
            Instruction::DefineFunction2 {
                name,
                params,
                register_count,
                flags,
                body,
            } => {
                arena.u8(opcode::DEFINE_FUNCTION2);
                arena.align_ptr();
                deferred.string(arena, name);
                arena.align(4);
                arena.i32(params.len() as i32);
                arena.i16(*register_count);
                arena.i16(*flags);
                if params.is_empty() {
                    arena.ptr_value(0);
                } else {
                    deferred.reg_params(arena, params.clone());
                }
                arena.align(4);
                let size_patch = arena.i32_patch();
                encode_func_pool(arena);
                let body_start = arena.len();
                encode_block(arena, &body.instructions, consts, deferred, false)?;
                arena.patch_i32(size_patch, (arena.len() - body_start) as i32);
            }
            Instruction::Try {
                try_count,
                catch_count,
                finally_count,
                has_catch,
                has_finally,
                catch_var,
            } => {
                arena.u8(opcode::TRY);
                arena.align_ptr();
                let try_patch = arena.i32_patch();
                let catch_patch = arena.i32_patch();
                let finally_patch = arena.i32_patch();
                let mut flags = 0u8;
                if *has_catch {
                    flags |= 1;
                }
                if *has_finally {
                    flags |= 2;
                }
                let register = match catch_var {
                    Some(CatchVar::Register(r)) => {
                        flags |= 4;
                        *r
                    }
                    _ => 0,
                };
                arena.u8(flags);
                arena.u8(0);
                arena.u8(0);
                arena.u8(register);
                match catch_var {
                    Some(CatchVar::Register(_)) => arena.ptr_value(0),
                    Some(CatchVar::Variable(name)) => deferred.string(arena, name),
                    None => deferred.string(arena, ""),
                }
                let after = arena.len();
                let t = i + 1 + try_count;
                let cch = t + catch_count;
                let fin = cch + finally_count;
                fixes.push(EncodeFix::Try(
                    [try_patch, catch_patch, finally_patch],
                    after,
                    [t, cch, fin],
                ));
            }
            Instruction::With { end_target } => {
                arena.u8(opcode::WITH);
                arena.align_ptr();
                let patch = arena.ptr_patch();
                fixes.push(EncodeFix::With(patch, arena.len(), *end_target));
            }
            Instruction::BranchAlways { target }
            | Instruction::BranchIfTrue { target }
            | Instruction::BranchIfFalse { target } => {
                arena.u8(match insn {
                    Instruction::BranchAlways { .. } => opcode::BRANCH_ALWAYS,
                    Instruction::BranchIfTrue { .. } => opcode::BRANCH_IF_TRUE,
                    _ => opcode::EA_BRANCH_IF_FALSE,
                });
                arena.align_ptr();
                let patch = arena.i32_patch();
                fixes.push(EncodeFix::Branch(patch, arena.len(), *target));
            }
            Instruction::TraceStart(v) => {
                arena.u8(opcode::EA_TRACE_START);
                arena.u32(*v);
            }
            Instruction::PushString { kind, value } => {
                arena.u8(kind.opcode());
                arena.align_ptr();
                deferred.string(arena, value);
            }
            Instruction::DictRef { kind, index } => {
                arena.u8(kind.opcode());
                if kind.is_word() {
                    arena.u16(*index);
                } else {
                    arena.u8(*index as u8);
                }
            }
            Instruction::PushFloat(v) => {
                arena.u8(0xB4);
                arena.u32(v.to_bits());
            }
            Instruction::PushByte(v) => {
                arena.u8(0xB5);
                arena.u8(*v);
            }
            Instruction::PushWord(v) => {
                arena.u8(0xB6);
                arena.u16(*v);
            }
            Instruction::PushDWord(v) => {
                arena.u8(0xB7);
                arena.u32(*v);
            }
            Instruction::PushRegister(v) => {
                arena.u8(opcode::EA_PUSH_REGISTER);
                arena.u8(*v);
            }
        }
    }
    boundaries.push(arena.len());

    let bound = |idx: usize| -> Result<usize> {
        boundaries.get(idx).copied().ok_or_else(|| {
            Error::Other(format!(
                "branch target index {idx} out of range ({} instructions)",
                instructions.len()
            ))
        })
    };
    for fix in fixes {
        match fix {
            EncodeFix::Branch(patch, base, target) => {
                let delta = bound(target)? as i64 - base as i64;
                arena.patch_i32(patch, delta as i32);
            }
            EncodeFix::With(patch, base, target) => {
                let delta = bound(target)? as i64 - base as i64;
                arena.patch_ptr(patch, delta as u64);
            }
            EncodeFix::Try(patches, base, ends) => {
                let mut prev = base;
                for (patch, end_idx) in patches.into_iter().zip(ends) {
                    let end = bound(end_idx)?;
                    arena.patch_i32(patch, (end - prev) as i32);
                    prev = end;
                }
            }
        }
    }
    Ok(())
}

fn encode_func_pool(arena: &mut Arena) {
    arena.align_ptr();
    arena.i32(FUNC_POOL_ITEMS_MAGIC as i32);
    arena.ptr_value(FUNC_POOL_ARRAY_MAGIC as u64);
}

fn encode_pool_items(
    arena: &mut Arena,
    op: u8,
    items: &[Value],
    consts: &mut Vec<Value>,
    deferred: &mut Deferred,
) {
    arena.u8(op);
    arena.align_ptr();
    arena.i32(items.len() as i32);
    let mut indices = Vec::with_capacity(items.len());
    for item in items {
        indices.push(consts.len() as u64);
        consts.push(item.clone());
    }
    if indices.is_empty() {
        arena.ptr_value(0);
    } else {
        deferred.ptr_array(arena, indices);
    }
}
