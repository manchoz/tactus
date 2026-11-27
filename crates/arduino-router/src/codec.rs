//! Streaming MessagePack-RPC frame codec compatible with `arduino-router` and `Arduino_RPClite`.

use crate::error::{DecodeError, EncodeError};
use crate::value::Value;

/// Default maximum decoded frame size in bytes (64 KiB).
pub const DEFAULT_MAX_FRAME_BYTES: usize = 64 * 1024;
/// Default maximum nested container depth allowed while decoding.
pub const DEFAULT_MAX_DECODE_DEPTH: usize = 32;

/// Resource limits enforced by the streaming MessagePack-RPC decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecLimits {
    /// Maximum allowed byte length of a single incoming MessagePack-RPC frame.
    pub max_frame_bytes: usize,
    /// Maximum nested container (array/map) recursion depth.
    pub max_decode_depth: usize,
}

impl Default for CodecLimits {
    fn default() -> Self {
        Self {
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            max_decode_depth: DEFAULT_MAX_DECODE_DEPTH,
        }
    }
}

/// MessagePack-RPC wire frame (`REQUEST = 0`, `RESPONSE = 1`, `NOTIFICATION = 2`).
#[derive(Debug, Clone, PartialEq)]
pub enum RpcMessage {
    /// `[0, msgid, method, params]`
    Request {
        /// Unique request correlation identifier (`u32`).
        msgid: u32,
        /// Target RPC method name.
        method: String,
        /// Positional MessagePack parameters.
        params: Vec<Value>,
    },
    /// `[1, msgid, error, result]`
    Response {
        /// Correlation identifier matching the originating request.
        msgid: u32,
        /// Wire error payload (`Value::Nil` on success).
        error: Value,
        /// Wire return value (`Value::Nil` on error or void).
        result: Value,
    },
    /// `[2, method, params]`
    Notification {
        /// Target RPC method name.
        method: String,
        /// Positional MessagePack parameters.
        params: Vec<Value>,
    },
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
    max_frame_bytes: usize,
}

impl<'a> Cursor<'a> {
    const fn new(buf: &'a [u8], max_frame_bytes: usize) -> Self {
        Self {
            buf,
            pos: 0,
            max_frame_bytes,
        }
    }

    fn take_u8(&mut self) -> Result<u8, DecodeError> {
        let next_pos = self.pos.checked_add(1).ok_or(DecodeError::Oversized)?;
        if next_pos > self.max_frame_bytes {
            return Err(DecodeError::Oversized);
        }
        if next_pos > self.buf.len() {
            return Err(DecodeError::Incomplete);
        }
        let b = self.buf[self.pos];
        self.pos = next_pos;
        Ok(b)
    }

    fn take_bytes(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        let end = self.pos.checked_add(len).ok_or(DecodeError::Oversized)?;
        if end > self.max_frame_bytes {
            return Err(DecodeError::Oversized);
        }
        if end > self.buf.len() {
            return Err(DecodeError::Incomplete);
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn take_u16_be(&mut self) -> Result<u16, DecodeError> {
        let bytes = self.take_bytes(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn take_u32_be(&mut self) -> Result<u32, DecodeError> {
        let bytes = self.take_bytes(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn take_u64_be(&mut self) -> Result<u64, DecodeError> {
        let bytes = self.take_bytes(8)?;
        Ok(u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }
}

/// Decode one complete MessagePack [`Value`] from the front of `buf`.
///
/// Returns `(value, bytes_consumed)`. Returns [`DecodeError::Incomplete`] when `buf`
/// contains a valid prefix of a value.
pub fn decode_value(buf: &[u8], limits: CodecLimits) -> Result<(Value, usize), DecodeError> {
    let mut cursor = Cursor::new(buf, limits.max_frame_bytes);
    let value = decode_value_inner(&mut cursor, 0, limits)?;
    Ok((value, cursor.pos))
}

/// Decode one complete [`RpcMessage`] frame from the front of `buf`.
///
/// Returns `(message, bytes_consumed)`.
pub fn decode_frame(buf: &[u8], limits: CodecLimits) -> Result<(RpcMessage, usize), DecodeError> {
    let (value, consumed) = decode_value(buf, limits)?;
    let Value::Array(items) = value else {
        return Err(DecodeError::Malformed("RPC frame must be an array"));
    };
    let mut iter = items.into_iter();
    let Some(kind_val) = iter.next() else {
        return Err(DecodeError::Malformed("empty RPC frame"));
    };

    match kind_val.as_i64() {
        Some(0) => {
            let (Some(msgid_val), Some(method_val), Some(params_val), None) =
                (iter.next(), iter.next(), iter.next(), iter.next())
            else {
                return Err(DecodeError::Malformed("invalid RPC request arity"));
            };
            let msgid = msgid_val
                .as_u32()
                .ok_or(DecodeError::Malformed("invalid RPC request msgid"))?;
            let Value::Str(method) = method_val else {
                return Err(DecodeError::Malformed("RPC request method must be string"));
            };
            let Value::Array(params) = params_val else {
                return Err(DecodeError::Malformed("RPC request params must be array"));
            };
            Ok((
                RpcMessage::Request {
                    msgid,
                    method,
                    params,
                },
                consumed,
            ))
        }
        Some(1) => {
            let (Some(msgid_val), Some(error), Some(result), None) =
                (iter.next(), iter.next(), iter.next(), iter.next())
            else {
                return Err(DecodeError::Malformed("invalid RPC response arity"));
            };
            let msgid = msgid_val
                .as_u32()
                .ok_or(DecodeError::Malformed("invalid RPC response msgid"))?;
            Ok((
                RpcMessage::Response {
                    msgid,
                    error,
                    result,
                },
                consumed,
            ))
        }
        Some(2) => {
            let (Some(method_val), Some(params_val), None) =
                (iter.next(), iter.next(), iter.next())
            else {
                return Err(DecodeError::Malformed("invalid RPC notification arity"));
            };
            let Value::Str(method) = method_val else {
                return Err(DecodeError::Malformed(
                    "RPC notification method must be string",
                ));
            };
            let Value::Array(params) = params_val else {
                return Err(DecodeError::Malformed(
                    "RPC notification params must be array",
                ));
            };
            Ok((RpcMessage::Notification { method, params }, consumed))
        }
        _ => Err(DecodeError::Malformed("unknown RPC message type")),
    }
}

fn decode_value_inner(
    cursor: &mut Cursor<'_>,
    depth: usize,
    limits: CodecLimits,
) -> Result<Value, DecodeError> {
    if depth >= limits.max_decode_depth {
        return Err(DecodeError::Malformed("nesting depth exceeded"));
    }

    let tag = cursor.take_u8()?;
    match tag {
        0x00..=0x7f => Ok(Value::Int(i64::from(tag))),
        0x80..=0x8f => decode_map(cursor, usize::from(tag & 0x0f), depth, limits),
        0x90..=0x9f => decode_array(cursor, usize::from(tag & 0x0f), depth, limits),
        0xa0..=0xbf => decode_str(cursor, usize::from(tag & 0x1f), limits),
        0xc0 => Ok(Value::Nil),
        0xc1 => Err(DecodeError::Malformed("reserved tag 0xc1")),
        0xc2 => Ok(Value::Bool(false)),
        0xc3 => Ok(Value::Bool(true)),
        0xc4 => {
            let len = usize::from(cursor.take_u8()?);
            decode_bin(cursor, len, limits)
        }
        0xc5 => {
            let len = usize::from(cursor.take_u16_be()?);
            decode_bin(cursor, len, limits)
        }
        0xc6 => {
            let len = cursor.take_u32_be()? as usize;
            decode_bin(cursor, len, limits)
        }
        0xc7 => {
            let len = usize::from(cursor.take_u8()?);
            decode_ext(cursor, len, limits)
        }
        0xc8 => {
            let len = usize::from(cursor.take_u16_be()?);
            decode_ext(cursor, len, limits)
        }
        0xc9 => {
            let len = cursor.take_u32_be()? as usize;
            decode_ext(cursor, len, limits)
        }
        0xca => {
            let bytes = cursor.take_bytes(4)?;
            let bits = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            Ok(Value::F32(f32::from_bits(bits)))
        }
        0xcb => {
            let bits = cursor.take_u64_be()?;
            Ok(Value::F64(f64::from_bits(bits)))
        }
        0xcc => Ok(Value::Int(i64::from(cursor.take_u8()?))),
        0xcd => Ok(Value::Int(i64::from(cursor.take_u16_be()?))),
        0xce => Ok(Value::Int(i64::from(cursor.take_u32_be()?))),
        0xcf => {
            let val = cursor.take_u64_be()?;
            Ok(match i64::try_from(val) {
                Ok(i) => Value::Int(i),
                Err(_) => Value::Uint(val),
            })
        }
        0xd0 => Ok(Value::Int(i64::from(cursor.take_u8()? as i8))),
        0xd1 => {
            let bytes = cursor.take_bytes(2)?;
            Ok(Value::Int(i64::from(i16::from_be_bytes([
                bytes[0], bytes[1],
            ]))))
        }
        0xd2 => {
            let bytes = cursor.take_bytes(4)?;
            Ok(Value::Int(i64::from(i32::from_be_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3],
            ]))))
        }
        0xd3 => {
            let bytes = cursor.take_bytes(8)?;
            Ok(Value::Int(i64::from_be_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ])))
        }
        0xd4 => decode_ext(cursor, 1, limits),
        0xd5 => decode_ext(cursor, 2, limits),
        0xd6 => decode_ext(cursor, 4, limits),
        0xd7 => decode_ext(cursor, 8, limits),
        0xd8 => decode_ext(cursor, 16, limits),
        0xd9 => {
            let len = usize::from(cursor.take_u8()?);
            decode_str(cursor, len, limits)
        }
        0xda => {
            let len = usize::from(cursor.take_u16_be()?);
            decode_str(cursor, len, limits)
        }
        0xdb => {
            let len = cursor.take_u32_be()? as usize;
            decode_str(cursor, len, limits)
        }
        0xdc => {
            let count = usize::from(cursor.take_u16_be()?);
            decode_array(cursor, count, depth, limits)
        }
        0xdd => {
            let count = cursor.take_u32_be()? as usize;
            decode_array(cursor, count, depth, limits)
        }
        0xde => {
            let count = usize::from(cursor.take_u16_be()?);
            decode_map(cursor, count, depth, limits)
        }
        0xdf => {
            let count = cursor.take_u32_be()? as usize;
            decode_map(cursor, count, depth, limits)
        }
        0xe0..=0xff => Ok(Value::Int(i64::from(tag as i8))),
    }
}

fn decode_str(
    cursor: &mut Cursor<'_>,
    len: usize,
    limits: CodecLimits,
) -> Result<Value, DecodeError> {
    if len > limits.max_frame_bytes {
        return Err(DecodeError::Oversized);
    }
    let bytes = cursor.take_bytes(len)?;
    let s = std::str::from_utf8(bytes)
        .map_err(|_| DecodeError::Malformed("invalid UTF-8 string"))?
        .to_string();
    Ok(Value::Str(s))
}

fn decode_bin(
    cursor: &mut Cursor<'_>,
    len: usize,
    limits: CodecLimits,
) -> Result<Value, DecodeError> {
    if len > limits.max_frame_bytes {
        return Err(DecodeError::Oversized);
    }
    let bytes = cursor.take_bytes(len)?;
    Ok(Value::Bin(bytes.to_vec()))
}

fn decode_ext(
    cursor: &mut Cursor<'_>,
    len: usize,
    limits: CodecLimits,
) -> Result<Value, DecodeError> {
    if len > limits.max_frame_bytes {
        return Err(DecodeError::Oversized);
    }
    let kind = cursor.take_u8()? as i8;
    let data = cursor.take_bytes(len)?;
    Ok(Value::Ext {
        kind,
        data: data.to_vec(),
    })
}

fn decode_array(
    cursor: &mut Cursor<'_>,
    count: usize,
    depth: usize,
    limits: CodecLimits,
) -> Result<Value, DecodeError> {
    if count > limits.max_frame_bytes {
        return Err(DecodeError::Oversized);
    }
    let mut items = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        items.push(decode_value_inner(cursor, depth + 1, limits)?);
    }
    Ok(Value::Array(items))
}

fn decode_map(
    cursor: &mut Cursor<'_>,
    count: usize,
    depth: usize,
    limits: CodecLimits,
) -> Result<Value, DecodeError> {
    if count > limits.max_frame_bytes / 2 {
        return Err(DecodeError::Oversized);
    }
    let mut entries = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        let key = decode_value_inner(cursor, depth + 1, limits)?;
        let val = decode_value_inner(cursor, depth + 1, limits)?;
        entries.push((key, val));
    }
    Ok(Value::Map(entries))
}

/// Encode a single [`RpcMessage`] frame into `out`.
pub fn encode_frame(msg: &RpcMessage, out: &mut Vec<u8>) -> Result<(), EncodeError> {
    match msg {
        RpcMessage::Request {
            msgid,
            method,
            params,
        } => {
            out.push(0x94);
            out.push(0x00);
            encode_uint(u64::from(*msgid), out);
            encode_str(method, out)?;
            encode_array_header(params.len(), out)?;
            for item in params {
                encode_value(item, out)?;
            }
            Ok(())
        }
        RpcMessage::Response {
            msgid,
            error,
            result,
        } => {
            out.push(0x94);
            out.push(0x01);
            encode_uint(u64::from(*msgid), out);
            encode_value(error, out)?;
            encode_value(result, out)?;
            Ok(())
        }
        RpcMessage::Notification { method, params } => {
            out.push(0x93);
            out.push(0x02);
            encode_str(method, out)?;
            encode_array_header(params.len(), out)?;
            for item in params {
                encode_value(item, out)?;
            }
            Ok(())
        }
    }
}

/// Encode an [`RpcMessage`] frame into `out` while enforcing `max_frame_bytes`.
///
/// On [`EncodeError::FrameTooLarge`], `out` is truncated back to its original length.
pub fn encode_frame_checked(
    msg: &RpcMessage,
    out: &mut Vec<u8>,
    max_frame_bytes: usize,
) -> Result<(), EncodeError> {
    let start = out.len();
    if let Err(err) = encode_frame(msg, out) {
        out.truncate(start);
        return Err(err);
    }
    let frame_len = out.len() - start;
    if frame_len > max_frame_bytes {
        out.truncate(start);
        return Err(EncodeError::FrameTooLarge {
            len: frame_len,
            max: max_frame_bytes,
        });
    }
    Ok(())
}

/// Encode a [`Value`] into MessagePack wire bytes using compact integer encoding.
pub fn encode_value(val: &Value, out: &mut Vec<u8>) -> Result<(), EncodeError> {
    match val {
        Value::Nil => out.push(0xc0),
        Value::Bool(b) => out.push(if *b { 0xc3 } else { 0xc2 }),
        Value::Int(i) => encode_int(*i, out),
        Value::Uint(u) => encode_uint(*u, out),
        Value::F32(f) => {
            out.push(0xca);
            out.extend_from_slice(&f.to_be_bytes());
        }
        Value::F64(f) => {
            out.push(0xcb);
            out.extend_from_slice(&f.to_be_bytes());
        }
        Value::Str(s) => encode_str(s, out)?,
        Value::Bin(b) => encode_bin(b, out)?,
        Value::Array(items) => {
            encode_array_header(items.len(), out)?;
            for item in items {
                encode_value(item, out)?;
            }
        }
        Value::Map(entries) => {
            encode_map_header(entries.len(), out)?;
            for (k, v) in entries {
                encode_value(k, out)?;
                encode_value(v, out)?;
            }
        }
        Value::Ext { kind, data } => encode_ext(*kind, data, out)?,
    }
    Ok(())
}

fn encode_uint(v: u64, out: &mut Vec<u8>) {
    if v <= 0x7f {
        out.push(v as u8);
    } else if v <= u64::from(u8::MAX) {
        out.push(0xcc);
        out.push(v as u8);
    } else if v <= u64::from(u16::MAX) {
        out.push(0xcd);
        out.extend_from_slice(&(v as u16).to_be_bytes());
    } else if v <= u64::from(u32::MAX) {
        out.push(0xce);
        out.extend_from_slice(&(v as u32).to_be_bytes());
    } else {
        out.push(0xcf);
        out.extend_from_slice(&v.to_be_bytes());
    }
}

fn encode_int(v: i64, out: &mut Vec<u8>) {
    if v >= 0 {
        encode_uint(v as u64, out);
    } else if v >= -32 {
        out.push((v as i8) as u8);
    } else if v >= i64::from(i8::MIN) {
        out.push(0xd0);
        out.push((v as i8) as u8);
    } else if v >= i64::from(i16::MIN) {
        out.push(0xd1);
        out.extend_from_slice(&(v as i16).to_be_bytes());
    } else if v >= i64::from(i32::MIN) {
        out.push(0xd2);
        out.extend_from_slice(&(v as i32).to_be_bytes());
    } else {
        out.push(0xd3);
        out.extend_from_slice(&v.to_be_bytes());
    }
}

fn encode_str(s: &str, out: &mut Vec<u8>) -> Result<(), EncodeError> {
    let len = s.len();
    if len <= 31 {
        out.push(0xa0 | (len as u8));
    } else if let Ok(l) = u8::try_from(len) {
        out.push(0xd9);
        out.push(l);
    } else if let Ok(l) = u16::try_from(len) {
        out.push(0xda);
        out.extend_from_slice(&l.to_be_bytes());
    } else if let Ok(l) = u32::try_from(len) {
        out.push(0xdb);
        out.extend_from_slice(&l.to_be_bytes());
    } else {
        return Err(EncodeError::LengthOverflow(
            "string exceeds MessagePack max length",
        ));
    }
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

fn encode_bin(bytes: &[u8], out: &mut Vec<u8>) -> Result<(), EncodeError> {
    let len = bytes.len();
    if let Ok(l) = u8::try_from(len) {
        out.push(0xc4);
        out.push(l);
    } else if let Ok(l) = u16::try_from(len) {
        out.push(0xc5);
        out.extend_from_slice(&l.to_be_bytes());
    } else if let Ok(l) = u32::try_from(len) {
        out.push(0xc6);
        out.extend_from_slice(&l.to_be_bytes());
    } else {
        return Err(EncodeError::LengthOverflow(
            "binary exceeds MessagePack max length",
        ));
    }
    out.extend_from_slice(bytes);
    Ok(())
}

fn encode_array_header(len: usize, out: &mut Vec<u8>) -> Result<(), EncodeError> {
    if len <= 15 {
        out.push(0x90 | (len as u8));
    } else if let Ok(l) = u16::try_from(len) {
        out.push(0xdc);
        out.extend_from_slice(&l.to_be_bytes());
    } else if let Ok(l) = u32::try_from(len) {
        out.push(0xdd);
        out.extend_from_slice(&l.to_be_bytes());
    } else {
        return Err(EncodeError::LengthOverflow(
            "array exceeds MessagePack max length",
        ));
    }
    Ok(())
}

fn encode_map_header(len: usize, out: &mut Vec<u8>) -> Result<(), EncodeError> {
    if len <= 15 {
        out.push(0x80 | (len as u8));
    } else if let Ok(l) = u16::try_from(len) {
        out.push(0xde);
        out.extend_from_slice(&l.to_be_bytes());
    } else if let Ok(l) = u32::try_from(len) {
        out.push(0xdf);
        out.extend_from_slice(&l.to_be_bytes());
    } else {
        return Err(EncodeError::LengthOverflow(
            "map exceeds MessagePack max length",
        ));
    }
    Ok(())
}

fn encode_ext(kind: i8, data: &[u8], out: &mut Vec<u8>) -> Result<(), EncodeError> {
    let len = data.len();
    match len {
        1 => out.push(0xd4),
        2 => out.push(0xd5),
        4 => out.push(0xd6),
        8 => out.push(0xd7),
        16 => out.push(0xd8),
        _ => {
            if let Ok(l) = u8::try_from(len) {
                out.push(0xc7);
                out.push(l);
            } else if let Ok(l) = u16::try_from(len) {
                out.push(0xc8);
                out.extend_from_slice(&l.to_be_bytes());
            } else if let Ok(l) = u32::try_from(len) {
                out.push(0xc9);
                out.extend_from_slice(&l.to_be_bytes());
            } else {
                return Err(EncodeError::LengthOverflow(
                    "ext exceeds MessagePack max length",
                ));
            }
        }
    }
    out.push(kind as u8);
    out.extend_from_slice(data);
    Ok(())
}
