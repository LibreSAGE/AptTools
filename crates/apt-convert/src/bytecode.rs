//! Translation between APT action streams and standard SWF AVM1 bytecode.
//!
//! APT keeps the SWF opcode numbers for standard actions but re-encodes their
//! operands (pointer-aligned inline structs, constant-table indices) and adds
//! EA shorthand opcodes. Going APT -> SWF we *expand* the shorthands into plain
//! AVM1 actions and re-encode operands into SWF's variable-length form; going
//! SWF -> APT we parse plain AVM1 into the APT instruction model.
//!
//! SWF AVM1 encoding: each action is an opcode byte; opcodes >= 0x80 are
//! followed by a little-endian u16 length and that many payload bytes. The
//! stream ends at an `End` (0x00) or the end of the buffer.

use apt::actions::{ActionStream, DictRefKind, Instruction, PushStringKind};
use apt::Value;

use crate::{Error, Result};

// Standard SWF AVM1 opcodes we emit/parse (subset; values match Flash).
mod swf_op {
    pub const END: u8 = 0x00;
    pub const POP: u8 = 0x17;
    pub const GET_VARIABLE: u8 = 0x1C;
    pub const SET_VARIABLE: u8 = 0x1D;
    pub const GET_MEMBER: u8 = 0x4E;
    pub const SET_MEMBER: u8 = 0x4F;
    pub const NOT: u8 = 0x12;
    pub const CALL_FUNCTION: u8 = 0x3D;
    pub const CALL_METHOD: u8 = 0x52;
    pub const GOTO_FRAME: u8 = 0x81;
    pub const GET_URL: u8 = 0x83;
    pub const STORE_REGISTER: u8 = 0x87;
    pub const CONSTANT_POOL: u8 = 0x88;
    pub const SET_TARGET: u8 = 0x8B;
    pub const GOTO_LABEL: u8 = 0x8C;
    pub const DEFINE_FUNCTION2: u8 = 0x8E;
    pub const WITH: u8 = 0x94;
    pub const PUSH: u8 = 0x96;
    pub const JUMP: u8 = 0x99;
    pub const DEFINE_FUNCTION: u8 = 0x9B;
    pub const IF: u8 = 0x9D;
    pub const GOTO_FRAME2: u8 = 0x9F;
    pub const WAIT_FOR_FRAME: u8 = 0x8A;
    pub const GET_URL2: u8 = 0x9A;
}

/// EA opcodes with no SWF equivalent, expanded during lowering.
fn is_ea_shorthand(op: u8) -> bool {
    use apt::actions::opcode as apt_op;
    matches!(
        op,
        apt_op::EA_PUSH_THIS
            | apt_op::EA_PUSH_GLOBAL
            | apt_op::EA_PUSH_ZERO
            | apt_op::EA_PUSH_ONE
            | apt_op::EA_CALL_FUNC_POP
            | apt_op::EA_CALL_FUNC_SET_VAR
            | apt_op::EA_CALL_METHOD_POP
            | apt_op::EA_CALL_METHOD_SET_VAR
            | apt_op::EA_PUSH_THIS_VAR
            | apt_op::EA_PUSH_GLOBAL_VAR
            | apt_op::EA_PUSH_ZERO_SET_VAR
            | apt_op::EA_PUSH_TRUE
            | apt_op::EA_PUSH_FALSE
            | apt_op::EA_PUSH_NULL
            | apt_op::EA_PUSH_UNDEFINED
            | apt_op::BREAKPOINT
    )
}

// SWF ActionPush value type tags.
mod push_type {
    pub const STRING: u8 = 0;
    pub const FLOAT: u8 = 1;
    pub const NULL: u8 = 2;
    pub const UNDEFINED: u8 = 3;
    pub const REGISTER: u8 = 4;
    pub const BOOLEAN: u8 = 5;
    pub const DOUBLE: u8 = 6;
    pub const INTEGER: u8 = 7;
    pub const CONSTANT8: u8 = 8;
    pub const CONSTANT16: u8 = 9;
}

// ---------------------------------------------------------------------------
// APT -> SWF (lowering + assembly)
// ---------------------------------------------------------------------------

/// A single lowered SWF action, prior to byte assembly. Branch/block targets
/// are APT instruction indices, resolved to byte offsets during assembly.
enum Low {
    /// Opcode with a fully-encoded payload (opcode < 0x80 => empty payload).
    Op(u8, Vec<u8>),
    /// ActionJump to an APT instruction index.
    Jump(usize),
    /// ActionIf to an APT instruction index.
    If(usize),
    /// ActionWith spanning up to an APT instruction index (block size u16).
    With(usize),
    /// Nested function: opcode (DefineFunction/2), header bytes, and body stream.
    Func(u8, Vec<u8>, ActionStream),
}

/// Lower one APT instruction into zero or more SWF actions. `boundary` receives
/// the index of this instruction so branch targets can be resolved.
fn lower_instruction(idx: usize, insn: &Instruction, out: &mut Vec<(usize, Low)>) -> Result<()> {
    let mut push = |op: Low| out.push((idx, op));

    match insn {
        Instruction::End => push(Low::Op(swf_op::END, vec![])),
        // Most no-payload APT opcodes pass straight through, but a couple of
        // them are payload-less only in APT: SWF still requires a body, and a
        // player rejects the action without it.
        Instruction::Simple(apt::actions::opcode::GET_URL2) => {
            // APT takes the url and target off the stack and always behaves as
            // "no send-vars method, no target, no variables" (see the engine's
            // _FunctionAptActionGetUrl2), which is a zero flags byte.
            push(Low::Op(swf_op::GET_URL2, vec![0]));
        }
        Instruction::Simple(apt::actions::opcode::WAIT_FOR_FRAME) => {
            // frame u16 + skip-count u8. APT keeps neither, and everything is
            // already loaded, so skipping nothing is the faithful no-op.
            push(Low::Op(swf_op::WAIT_FOR_FRAME, vec![0, 0, 0]));
        }
        // EA shorthand opcodes don't exist in SWF; a player aborts the whole
        // script on the first one it sees. Each expands per the engine's
        // interpreter (AptActionInterpreter.cpp), which implements them as
        // compositions of the standard actions.
        Instruction::Simple(op) if is_ea_shorthand(*op) => {
            use apt::actions::opcode as apt_op;
            match *op {
                apt_op::EA_PUSH_THIS => push(Low::Op(
                    swf_op::PUSH,
                    encode_push(&[Value::String("this".into())])?,
                )),
                apt_op::EA_PUSH_GLOBAL => push(Low::Op(
                    swf_op::PUSH,
                    encode_push(&[Value::String("_global".into())])?,
                )),
                apt_op::EA_PUSH_ZERO => {
                    push(Low::Op(swf_op::PUSH, encode_push(&[Value::Integer(0)])?))
                }
                apt_op::EA_PUSH_ONE => {
                    push(Low::Op(swf_op::PUSH, encode_push(&[Value::Integer(1)])?))
                }
                apt_op::EA_CALL_FUNC_POP => {
                    push(Low::Op(swf_op::CALL_FUNCTION, vec![]));
                    push(Low::Op(swf_op::POP, vec![]));
                }
                apt_op::EA_CALL_FUNC_SET_VAR => {
                    push(Low::Op(swf_op::CALL_FUNCTION, vec![]));
                    push(Low::Op(swf_op::SET_VARIABLE, vec![]));
                }
                apt_op::EA_CALL_METHOD_POP => {
                    push(Low::Op(swf_op::CALL_METHOD, vec![]));
                    push(Low::Op(swf_op::POP, vec![]));
                }
                apt_op::EA_CALL_METHOD_SET_VAR => {
                    push(Low::Op(swf_op::CALL_METHOD, vec![]));
                    push(Low::Op(swf_op::SET_VARIABLE, vec![]));
                }
                apt_op::EA_PUSH_THIS_VAR => {
                    push(Low::Op(
                        swf_op::PUSH,
                        encode_push(&[Value::String("this".into())])?,
                    ));
                    push(Low::Op(swf_op::GET_VARIABLE, vec![]));
                }
                apt_op::EA_PUSH_GLOBAL_VAR => {
                    push(Low::Op(
                        swf_op::PUSH,
                        encode_push(&[Value::String("_global".into())])?,
                    ));
                    push(Low::Op(swf_op::GET_VARIABLE, vec![]));
                }
                apt_op::EA_PUSH_ZERO_SET_VAR => {
                    push(Low::Op(swf_op::PUSH, encode_push(&[Value::Integer(0)])?));
                    push(Low::Op(swf_op::SET_VARIABLE, vec![]));
                }
                apt_op::EA_PUSH_TRUE => {
                    push(Low::Op(swf_op::PUSH, encode_push(&[Value::Boolean(true)])?))
                }
                apt_op::EA_PUSH_FALSE => push(Low::Op(
                    swf_op::PUSH,
                    encode_push(&[Value::Boolean(false)])?,
                )),
                apt_op::EA_PUSH_NULL => push(Low::Op(swf_op::PUSH, vec![push_type::NULL])),
                apt_op::EA_PUSH_UNDEFINED => {
                    push(Low::Op(swf_op::PUSH, vec![push_type::UNDEFINED]))
                }
                // Debugger breakpoint: nothing to do in a normal player.
                apt_op::BREAKPOINT => {}
                _ => unreachable!(),
            }
        }
        Instruction::Simple(op) => push(Low::Op(*op, vec![])),
        Instruction::GotoFrame(f) => push(Low::Op(
            swf_op::GOTO_FRAME,
            (*f as u16).to_le_bytes().to_vec(),
        )),
        Instruction::GetUrl { url, target } => {
            let mut p = string_bytes(url)?;
            p.extend(string_bytes(target)?);
            push(Low::Op(swf_op::GET_URL, p));
        }
        Instruction::StoreRegister(r) => push(Low::Op(swf_op::STORE_REGISTER, vec![*r as u8])),
        Instruction::SetTarget(t) => push(Low::Op(swf_op::SET_TARGET, string_bytes(t)?)),
        Instruction::GotoLabel(l) => push(Low::Op(swf_op::GOTO_LABEL, string_bytes(l)?)),
        Instruction::GotoFrame2 { play } => push(Low::Op(swf_op::GOTO_FRAME2, vec![*play as u8])),
        Instruction::Push(items) => push(Low::Op(swf_op::PUSH, encode_push(items)?)),
        Instruction::DefineDictionary(items) => {
            push(Low::Op(swf_op::CONSTANT_POOL, encode_constant_pool(items)?))
        }
        Instruction::DefineFunction { name, params, body } => {
            let mut hdr = string_bytes(name)?;
            hdr.extend((params.len() as u16).to_le_bytes());
            for p in params {
                hdr.extend(string_bytes(p)?);
            }
            push(Low::Func(swf_op::DEFINE_FUNCTION, hdr, body.clone()));
        }
        Instruction::DefineFunction2 {
            name,
            params,
            register_count,
            flags,
            body,
        } => {
            // SWF layout: name, NumParams u16, RegisterCount u8, Flags u16,
            // then {Register u8, ParamName} per param, then CodeSize u16.
            let mut hdr = string_bytes(name)?;
            hdr.extend((params.len() as u16).to_le_bytes());
            hdr.push(*register_count as u8);
            hdr.extend((*flags as u16).to_le_bytes());
            for (reg, pname) in params {
                hdr.push(*reg as u8);
                hdr.extend(string_bytes(pname)?);
            }
            push(Low::Func(swf_op::DEFINE_FUNCTION2, hdr, body.clone()));
        }
        Instruction::With { end_target } => push(Low::With(*end_target)),
        Instruction::BranchAlways { target } => push(Low::Jump(*target)),
        Instruction::BranchIfTrue { target } => push(Low::If(*target)),
        Instruction::BranchIfFalse { target } => {
            // No SWF branch-if-false: negate then branch-if-true.
            push(Low::Op(swf_op::NOT, vec![]));
            push(Low::If(*target));
        }
        Instruction::Try { .. } => {
            return Err(Error::Unsupported(
                "Try/Catch action translation to SWF".into(),
            ))
        }
        Instruction::TraceStart(_) => {} // debug-only; drop
        // EA shorthand expansions.
        Instruction::PushString { kind, value } => {
            push(Low::Op(
                swf_op::PUSH,
                encode_push(&[Value::String(value.clone())])?,
            ));
            match kind {
                PushStringKind::Push => {}
                PushStringKind::GetVar => push(Low::Op(swf_op::GET_VARIABLE, vec![])),
                PushStringKind::GetMember => push(Low::Op(swf_op::GET_MEMBER, vec![])),
                PushStringKind::SetVar => push(Low::Op(swf_op::SET_VARIABLE, vec![])),
                PushStringKind::SetMember => push(Low::Op(swf_op::SET_MEMBER, vec![])),
            }
        }
        Instruction::DictRef { kind, index } => {
            push(Low::Op(swf_op::PUSH, encode_constant_ref(*index)));
            match kind {
                DictRefKind::PushByte | DictRefKind::PushWord => {}
                DictRefKind::ByteGetVar => push(Low::Op(swf_op::GET_VARIABLE, vec![])),
                DictRefKind::ByteGetMember => push(Low::Op(swf_op::GET_MEMBER, vec![])),
                DictRefKind::CallFuncPop => {
                    push(Low::Op(swf_op::CALL_FUNCTION, vec![]));
                    push(Low::Op(swf_op::POP, vec![]));
                }
                DictRefKind::CallFuncSetVar => {
                    push(Low::Op(swf_op::CALL_FUNCTION, vec![]));
                    push(Low::Op(swf_op::SET_VARIABLE, vec![]));
                }
                DictRefKind::CallMethodPop => {
                    push(Low::Op(swf_op::CALL_METHOD, vec![]));
                    push(Low::Op(swf_op::POP, vec![]));
                }
                DictRefKind::CallMethodSetVar => {
                    push(Low::Op(swf_op::CALL_METHOD, vec![]));
                    push(Low::Op(swf_op::SET_VARIABLE, vec![]));
                }
            }
        }
        Instruction::PushFloat(v) => push(Low::Op(swf_op::PUSH, encode_push(&[Value::Float(*v)])?)),
        Instruction::PushByte(v) => push(Low::Op(
            swf_op::PUSH,
            encode_push(&[Value::Integer(*v as i32)])?,
        )),
        Instruction::PushWord(v) => push(Low::Op(
            swf_op::PUSH,
            encode_push(&[Value::Integer(*v as i32)])?,
        )),
        Instruction::PushDWord(v) => push(Low::Op(
            swf_op::PUSH,
            encode_push(&[Value::Integer(*v as i32)])?,
        )),
        Instruction::PushRegister(v) => push(Low::Op(
            swf_op::PUSH,
            encode_push(&[Value::Register(*v as i32)])?,
        )),
    }
    Ok(())
}

/// Encode an APT action stream as standard SWF AVM1 bytecode.
pub fn apt_to_swf_actions(stream: &ActionStream) -> Result<Vec<u8>> {
    let mut lowered = Vec::new();
    for (idx, insn) in stream.instructions.iter().enumerate() {
        lower_instruction(idx, insn, &mut lowered)?;
    }
    assemble(&lowered, stream.instructions.len())
}

/// Assemble lowered actions into bytes, resolving branch/with targets.
fn assemble(lowered: &[(usize, Low)], n_instructions: usize) -> Result<Vec<u8>> {
    // First pass: byte offset where each lowered item starts, and the byte
    // offset where each APT instruction index begins.
    let mut sizes = Vec::with_capacity(lowered.len());
    for (_, low) in lowered {
        sizes.push(lowered_size(low)?);
    }
    // Instruction-index -> byte offset (start of its first lowered op).
    let mut index_offset = vec![None; n_instructions + 1];
    let mut off = 0usize;
    for ((idx, _), size) in lowered.iter().zip(&sizes) {
        if index_offset[*idx].is_none() {
            index_offset[*idx] = Some(off);
        }
        off += size;
    }
    index_offset[n_instructions] = Some(off); // "past end"
    let total = off;
    let resolve = |idx: usize| -> Result<usize> {
        // Fall back to end-of-stream for out-of-range/forward targets.
        (idx..=n_instructions)
            .find_map(|i| index_offset.get(i).copied().flatten())
            .ok_or_else(|| Error::Other(format!("unresolved branch target {idx}")))
    };

    // Second pass: emit.
    let mut buf = Vec::with_capacity(total);
    for (i, (_, low)) in lowered.iter().enumerate() {
        let after = buf.len() + sizes[i]; // byte offset just past this action
        match low {
            Low::Op(op, payload) => emit_op(&mut buf, *op, payload),
            Low::Jump(target) => {
                let delta = resolve(*target)? as isize - after as isize;
                emit_op(&mut buf, swf_op::JUMP, &(delta as i16).to_le_bytes());
            }
            Low::If(target) => {
                let delta = resolve(*target)? as isize - after as isize;
                emit_op(&mut buf, swf_op::IF, &(delta as i16).to_le_bytes());
            }
            Low::With(end) => {
                let size = resolve(*end)?.saturating_sub(after);
                emit_op(&mut buf, swf_op::WITH, &(size as u16).to_le_bytes());
            }
            Low::Func(op, hdr, body) => {
                let body_bytes = apt_to_swf_actions(body)?;
                let mut payload = hdr.clone();
                payload.extend((body_bytes.len() as u16).to_le_bytes());
                emit_op(&mut buf, *op, &payload);
                buf.extend_from_slice(&body_bytes);
            }
        }
    }
    Ok(buf)
}

fn lowered_size(low: &Low) -> Result<usize> {
    Ok(match low {
        Low::Op(op, payload) => op_size(*op, payload.len()),
        Low::Jump(_) | Low::If(_) => 3 + 2, // opcode + u16 len + s16
        Low::With(_) => 3 + 2,
        Low::Func(op, hdr, body) => {
            let body_len = apt_to_swf_actions(body)?.len();
            op_size(*op, hdr.len() + 2) + body_len
        }
    })
}

fn op_size(op: u8, payload_len: usize) -> usize {
    if op >= 0x80 {
        1 + 2 + payload_len
    } else {
        1
    }
}

fn emit_op(buf: &mut Vec<u8>, op: u8, payload: &[u8]) {
    buf.push(op);
    if op >= 0x80 {
        buf.extend((payload.len() as u16).to_le_bytes());
        buf.extend_from_slice(payload);
    }
}

fn string_bytes(s: &str) -> Result<Vec<u8>> {
    let mut v = latin1(s)?;
    v.push(0);
    Ok(v)
}

fn latin1(s: &str) -> Result<Vec<u8>> {
    s.chars()
        .map(|c| {
            let v = c as u32;
            if v <= 0xFF {
                Ok(v as u8)
            } else {
                Err(Error::Other(format!("non-latin1 char in {s:?}")))
            }
        })
        .collect()
}

fn encode_push(items: &[Value]) -> Result<Vec<u8>> {
    let mut p = Vec::new();
    for v in items {
        match v {
            Value::String(s) => {
                p.push(push_type::STRING);
                p.extend(string_bytes(s)?);
            }
            Value::Float(f) => {
                p.push(push_type::FLOAT);
                p.extend(f.to_le_bytes());
            }
            Value::Undefined => p.push(push_type::UNDEFINED),
            Value::Register(r) => {
                p.push(push_type::REGISTER);
                p.push(*r as u8);
            }
            Value::Boolean(b) => {
                p.push(push_type::BOOLEAN);
                p.push(*b as u8);
            }
            Value::Integer(i) => {
                p.push(push_type::INTEGER);
                p.extend(i.to_le_bytes());
            }
            Value::Lookup(idx) => {
                if *idx <= 0xFF {
                    p.push(push_type::CONSTANT8);
                    p.push(*idx as u8);
                } else {
                    p.push(push_type::CONSTANT16);
                    p.extend((*idx as u16).to_le_bytes());
                }
            }
        }
    }
    Ok(p)
}

fn encode_constant_ref(index: u16) -> Vec<u8> {
    if index <= 0xFF {
        vec![push_type::CONSTANT8, index as u8]
    } else {
        let mut p = vec![push_type::CONSTANT16];
        p.extend(index.to_le_bytes());
        p
    }
}

/// SWF ActionConstantPool: u16 count, then that many NUL-terminated strings.
fn encode_constant_pool(items: &[Value]) -> Result<Vec<u8>> {
    let mut p = (items.len() as u16).to_le_bytes().to_vec();
    for v in items {
        match v {
            Value::String(s) => p.extend(string_bytes(s)?),
            other => {
                // ConstantPool holds strings; stringify other values.
                p.extend(string_bytes(&value_to_string(other))?)
            }
        }
    }
    Ok(p)
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Integer(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Register(r) => format!("r{r}"),
        Value::Lookup(i) => format!("c{i}"),
        Value::Undefined => "undefined".into(),
    }
}

// ---------------------------------------------------------------------------
// SWF -> APT (parsing standard AVM1)
// ---------------------------------------------------------------------------

/// Parse standard SWF AVM1 bytecode into an APT action stream.
pub fn swf_to_apt_actions(data: &[u8]) -> Result<ActionStream> {
    let instructions = parse_block(data)?;
    Ok(ActionStream { instructions })
}

fn parse_block(data: &[u8]) -> Result<Vec<Instruction>> {
    // First decode into (byte_offset, Raw) so branch offsets can be mapped to
    // instruction indices afterward.
    struct Raw {
        offset: usize,
        insn: Instruction,
        /// Branch delta relative to the byte just past this action.
        branch: Option<(BranchKind, i16, usize)>,
        /// With: absolute byte offset where the block ends.
        with_end: Option<usize>,
    }
    enum BranchKind {
        Jump,
        If,
    }

    let mut raws: Vec<Raw> = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let offset = pos;
        let op = data[pos];
        pos += 1;
        let payload = if op >= 0x80 {
            if pos + 2 > data.len() {
                return Err(Error::SwfRead("truncated action length".into()));
            }
            let len = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2;
            if pos + len > data.len() {
                return Err(Error::SwfRead("truncated action payload".into()));
            }
            let p = &data[pos..pos + len];
            pos += len;
            p
        } else {
            &[]
        };
        let after = pos;

        let mut branch = None;
        let insn = match op {
            swf_op::END => Instruction::End,
            swf_op::GOTO_FRAME => {
                Instruction::GotoFrame(u16::from_le_bytes([payload[0], payload[1]]) as i32)
            }
            swf_op::GET_URL => {
                let (url, rest) = read_string(payload)?;
                let (target, _) = read_string(rest)?;
                Instruction::GetUrl { url, target }
            }
            swf_op::STORE_REGISTER => {
                Instruction::StoreRegister(payload.first().copied().unwrap_or(0) as i32)
            }
            swf_op::CONSTANT_POOL => Instruction::DefineDictionary(parse_constant_pool(payload)?),
            swf_op::SET_TARGET => Instruction::SetTarget(read_string(payload)?.0),
            swf_op::GOTO_LABEL => Instruction::GotoLabel(read_string(payload)?.0),
            swf_op::GOTO_FRAME2 => Instruction::GotoFrame2 {
                play: payload.first().map(|&b| b & 1 != 0).unwrap_or(false),
            },
            swf_op::PUSH => Instruction::Push(parse_push(payload)?),
            swf_op::JUMP => {
                let d = i16::from_le_bytes([payload[0], payload[1]]);
                branch = Some((BranchKind::Jump, d, after));
                Instruction::BranchAlways { target: 0 }
            }
            swf_op::IF => {
                let d = i16::from_le_bytes([payload[0], payload[1]]);
                branch = Some((BranchKind::If, d, after));
                Instruction::BranchIfTrue { target: 0 }
            }
            swf_op::WITH => {
                // The block body is the next `size` bytes of inline actions;
                // record where it ends and keep parsing normally.
                let size = u16::from_le_bytes([payload[0], payload[1]]) as usize;
                raws.push(Raw {
                    offset,
                    insn: Instruction::With { end_target: 0 },
                    branch: None,
                    with_end: Some(after + size),
                });
                continue;
            }
            swf_op::DEFINE_FUNCTION => {
                let (name, rest) = read_string(payload)?;
                let n_params = u16::from_le_bytes([rest[0], rest[1]]) as usize;
                let mut r = &rest[2..];
                let mut params = Vec::with_capacity(n_params);
                for _ in 0..n_params {
                    let (p, rr) = read_string(r)?;
                    params.push(p);
                    r = rr;
                }
                let code_size = u16::from_le_bytes([r[0], r[1]]) as usize;
                let body = &data[pos..(pos + code_size).min(data.len())];
                pos += code_size;
                Instruction::DefineFunction {
                    name,
                    params,
                    body: ActionStream {
                        instructions: parse_block(body)?,
                    },
                }
            }
            swf_op::DEFINE_FUNCTION2 => {
                let (name, rest) = read_string(payload)?;
                let n_params = u16::from_le_bytes([rest[0], rest[1]]) as usize;
                let register_count = rest[2] as i16;
                let flags = i16::from_le_bytes([rest[3], rest[4]]);
                let mut r = &rest[5..];
                let mut params = Vec::with_capacity(n_params);
                for _ in 0..n_params {
                    let reg = r[0] as u32;
                    let (p, rr) = read_string(&r[1..])?;
                    params.push((reg, p));
                    r = rr;
                }
                let code_size = u16::from_le_bytes([r[0], r[1]]) as usize;
                let body = &data[pos..(pos + code_size).min(data.len())];
                pos += code_size;
                Instruction::DefineFunction2 {
                    name,
                    params,
                    register_count,
                    flags,
                    body: ActionStream {
                        instructions: parse_block(body)?,
                    },
                }
            }
            other => Instruction::Simple(other),
        };
        raws.push(Raw {
            offset,
            insn,
            branch,
            with_end: None,
        });
    }

    let index_of = |byte: usize| -> usize {
        raws.iter()
            .position(|r| r.offset == byte)
            .unwrap_or(raws.len())
    };
    let mut instructions = Vec::with_capacity(raws.len());
    for raw in &raws {
        let mut insn = raw.insn.clone();
        if let Some((kind, delta, after)) = &raw.branch {
            let target = (*after as isize + *delta as isize) as usize;
            let idx = index_of(target);
            match (&kind, &mut insn) {
                (BranchKind::Jump, Instruction::BranchAlways { target }) => *target = idx,
                (BranchKind::If, Instruction::BranchIfTrue { target }) => *target = idx,
                _ => {}
            }
        }
        if let (Some(end), Instruction::With { end_target }) = (&raw.with_end, &mut insn) {
            *end_target = index_of(*end);
        }
        instructions.push(insn);
    }
    Ok(instructions)
}

fn read_string(data: &[u8]) -> Result<(String, &[u8])> {
    let end = data
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| Error::SwfRead("unterminated string".into()))?;
    let s = data[..end].iter().map(|&b| b as char).collect();
    Ok((s, &data[end + 1..]))
}

fn parse_constant_pool(data: &[u8]) -> Result<Vec<Value>> {
    if data.len() < 2 {
        return Ok(vec![]);
    }
    let count = u16::from_le_bytes([data[0], data[1]]) as usize;
    let mut rest = &data[2..];
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let (s, r) = read_string(rest)?;
        out.push(Value::String(s));
        rest = r;
    }
    Ok(out)
}

fn parse_push(mut data: &[u8]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    while !data.is_empty() {
        let ty = data[0];
        data = &data[1..];
        let v = match ty {
            push_type::STRING => {
                let (s, r) = read_string(data)?;
                data = r;
                Value::String(s)
            }
            push_type::FLOAT => {
                let v = f32::from_le_bytes(data[..4].try_into().unwrap());
                data = &data[4..];
                Value::Float(v)
            }
            push_type::NULL => Value::Undefined,
            push_type::UNDEFINED => Value::Undefined,
            push_type::REGISTER => {
                let r = data[0] as i32;
                data = &data[1..];
                Value::Register(r)
            }
            push_type::BOOLEAN => {
                let b = data[0] != 0;
                data = &data[1..];
                Value::Boolean(b)
            }
            push_type::DOUBLE => {
                // SWF stores doubles as two little-endian u32 halves swapped.
                let hi = u32::from_le_bytes(data[..4].try_into().unwrap());
                let lo = u32::from_le_bytes(data[4..8].try_into().unwrap());
                data = &data[8..];
                let bits = ((hi as u64) << 32) | lo as u64;
                Value::Float(f64::from_bits(bits) as f32)
            }
            push_type::INTEGER => {
                let v = i32::from_le_bytes(data[..4].try_into().unwrap());
                data = &data[4..];
                Value::Integer(v)
            }
            push_type::CONSTANT8 => {
                let v = data[0] as u32;
                data = &data[1..];
                Value::Lookup(v)
            }
            push_type::CONSTANT16 => {
                let v = u16::from_le_bytes(data[..2].try_into().unwrap()) as u32;
                data = &data[2..];
                Value::Lookup(v)
            }
            other => return Err(Error::SwfRead(format!("unknown push type {other}"))),
        };
        out.push(v);
    }
    Ok(out)
}
