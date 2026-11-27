//! Protocol error codes, wire RPC errors, and crate error types.

use std::error::Error as StdError;
use std::fmt;

use crate::value::{ConversionError, Value};

/// Standard error codes defined by `arduino-router` and `Arduino_RPClite`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// `0x00` (`0`) — No error (`Arduino_RPClite::NO_ERR`).
    NoError,
    /// `1` — Invalid parameters (`arduino-router::ErrCodeInvalidParams`).
    InvalidParams,
    /// `2` — Requested method is not registered on the router (`arduino-router::ErrCodeMethodNotAvailable`).
    MethodNotAvailable,
    /// `3` — Router failed to forward the request (`arduino-router::ErrCodeFailedToSendRequests`).
    FailedToSendRequest,
    /// `4` — Generic router error (`arduino-router::ErrCodeGenericError`).
    RouterGenericError,
    /// `5` — Method route is already registered by another client (`arduino-router::ErrCodeRouteAlreadyExists`).
    RouteAlreadyExists,
    /// `6` — Message exceeded configured max size (`arduino-router::ErrCodeBufferLimitExceeded`).
    BufferLimitExceeded,
    /// `0xFB` (`251`) — RPC call timed out (`Arduino_RPClite::TIMEOUT_ERR`).
    Timeout,
    /// `0xFC` (`252`) — MessagePack parsing/deserialization failure (`Arduino_RPClite::PARSING_ERR`).
    ParsingError,
    /// `0xFD` (`253`) — Malformed call or argument type/arity mismatch (`Arduino_RPClite::MALFORMED_CALL_ERR`).
    MalformedCall,
    /// `0xFE` (`254`) — Bound method not found on peer (`Arduino_RPClite::FUNCTION_NOT_FOUND_ERR`).
    FunctionNotFound,
    /// `0xFF` (`255`) — Generic peer RPC error (`Arduino_RPClite::GENERIC_ERR`).
    GenericError,
    /// Any other application-defined numeric error code.
    Custom(i32),
}

impl ErrorCode {
    /// Convert a numeric wire error code into [`ErrorCode`].
    #[must_use]
    pub const fn from_i32(code: i32) -> Self {
        match code {
            0x00 => Self::NoError,
            1 => Self::InvalidParams,
            2 => Self::MethodNotAvailable,
            3 => Self::FailedToSendRequest,
            4 => Self::RouterGenericError,
            5 => Self::RouteAlreadyExists,
            6 => Self::BufferLimitExceeded,
            0xFB => Self::Timeout,
            0xFC => Self::ParsingError,
            0xFD => Self::MalformedCall,
            0xFE => Self::FunctionNotFound,
            0xFF => Self::GenericError,
            other => Self::Custom(other),
        }
    }

    /// Return the wire numeric code corresponding to this [`ErrorCode`].
    #[must_use]
    pub const fn as_i32(self) -> i32 {
        match self {
            Self::NoError => 0x00,
            Self::InvalidParams => 1,
            Self::MethodNotAvailable => 2,
            Self::FailedToSendRequest => 3,
            Self::RouterGenericError => 4,
            Self::RouteAlreadyExists => 5,
            Self::BufferLimitExceeded => 6,
            Self::Timeout => 0xFB,
            Self::ParsingError => 0xFC,
            Self::MalformedCall => 0xFD,
            Self::FunctionNotFound => 0xFE,
            Self::GenericError => 0xFF,
            Self::Custom(code) => code,
        }
    }
}

impl From<i32> for ErrorCode {
    fn from(code: i32) -> Self {
        Self::from_i32(code)
    }
}

impl From<ErrorCode> for i32 {
    fn from(code: ErrorCode) -> Self {
        code.as_i32()
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoError => write!(f, "no error (0)"),
            Self::InvalidParams => write!(f, "invalid params (1)"),
            Self::MethodNotAvailable => write!(f, "method not available (2)"),
            Self::FailedToSendRequest => write!(f, "failed to send request (3)"),
            Self::RouterGenericError => write!(f, "router generic error (4)"),
            Self::RouteAlreadyExists => write!(f, "route already exists (5)"),
            Self::BufferLimitExceeded => write!(f, "buffer limit exceeded (6)"),
            Self::Timeout => write!(f, "timeout (251)"),
            Self::ParsingError => write!(f, "parsing error (252)"),
            Self::MalformedCall => write!(f, "malformed call (253)"),
            Self::FunctionNotFound => write!(f, "function not found (254)"),
            Self::GenericError => write!(f, "generic error (255)"),
            Self::Custom(code) => write!(f, "custom error ({code})"),
        }
    }
}

/// Wire RPC error `[code, message]` used by both `arduino-router` and `Arduino_RPClite`.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    /// Structured error code.
    pub code: ErrorCode,
    /// Human-readable error message or traceback.
    pub message: String,
    /// Original raw wire error value when decoded from an incoming response.
    pub raw: Option<Value>,
}

impl RpcError {
    /// Construct a new [`RpcError`].
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            raw: None,
        }
    }

    /// Construct a [`ErrorCode::InvalidParams`] error (`1`).
    #[must_use]
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidParams, message)
    }

    /// Construct a [`ErrorCode::MalformedCall`] error (`0xFD` / `253`).
    #[must_use]
    pub fn malformed_call(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::MalformedCall, message)
    }

    /// Construct a [`ErrorCode::FunctionNotFound`] error (`0xFE` / `254`).
    #[must_use]
    pub fn function_not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::FunctionNotFound, message)
    }

    /// Construct a [`ErrorCode::GenericError`] error (`0xFF` / `255`).
    #[must_use]
    pub fn generic(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::GenericError, message)
    }

    /// Construct a [`ErrorCode::Custom`] error.
    #[must_use]
    pub fn custom(code: i32, message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Custom(code), message)
    }

    /// Encode this error in the canonical `[code, message]` MessagePack array format.
    #[must_use]
    pub fn to_wire(&self) -> Value {
        Value::Array(vec![
            Value::Int(i64::from(self.code.as_i32())),
            Value::Str(self.message.clone()),
        ])
    }

    /// Decode a wire error value from a MessagePack-RPC response.
    ///
    /// Handles canonical `[code, message]` arrays from `arduino-router` and
    /// `Arduino_RPClite`, as well as legacy bare string or integer errors.
    #[must_use]
    pub fn from_wire(value: Value) -> Self {
        match &value {
            Value::Array(items) if items.len() == 2 => {
                if let Some(code_i32) = items[0].as_i64().and_then(|c| i32::try_from(c).ok()) {
                    let message = match &items[1] {
                        Value::Str(s) => s.clone(),
                        Value::Nil => String::new(),
                        other => format!("{other:?}"),
                    };
                    return Self {
                        code: ErrorCode::from_i32(code_i32),
                        message,
                        raw: Some(value),
                    };
                }
            }
            Value::Str(s) => {
                return Self {
                    code: ErrorCode::GenericError,
                    message: s.clone(),
                    raw: Some(value),
                };
            }
            _ => {
                if let Some(code_i32) = value.as_i64().and_then(|c| i32::try_from(c).ok()) {
                    let code = ErrorCode::from_i32(code_i32);
                    return Self {
                        code,
                        message: code.to_string(),
                        raw: Some(value),
                    };
                }
            }
        }

        let message = format!("{value:?}");
        Self {
            code: ErrorCode::GenericError,
            message,
            raw: Some(value),
        }
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.message.is_empty() {
            write!(f, "{}", self.code)
        } else {
            write!(f, "{}: {}", self.code, self.message)
        }
    }
}

impl StdError for RpcError {}

/// Error returned while encoding a MessagePack value or RPC frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// String, binary, array, map, or extension exceeded MessagePack's 32-bit length limit.
    LengthOverflow(&'static str),
    /// Encoded frame exceeded a configured maximum message size.
    FrameTooLarge {
        /// Actual encoded frame length in bytes.
        len: usize,
        /// Configured maximum frame length in bytes.
        max: usize,
    },
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LengthOverflow(reason) => f.write_str(reason),
            Self::FrameTooLarge { len, max } => {
                write!(
                    f,
                    "encoded frame size ({len} bytes) exceeds limit ({max} bytes)"
                )
            }
        }
    }
}

impl StdError for EncodeError {}

/// Error returned while decoding a MessagePack value or RPC frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// Buffer does not yet contain a complete MessagePack frame.
    Incomplete,
    /// Declared payload/container length or frame byte count exceeds `CodecLimits::max_frame_bytes`.
    Oversized,
    /// Wire bytes violate MessagePack or MessagePack-RPC framing rules.
    Malformed(&'static str),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Incomplete => f.write_str("incomplete MessagePack frame"),
            Self::Oversized => f.write_str("MessagePack frame exceeds configured maximum size"),
            Self::Malformed(reason) => write!(f, "malformed MessagePack-RPC frame: {reason}"),
        }
    }
}

impl StdError for DecodeError {}

/// Top-level error type for `arduino-router` operations.
#[derive(Debug)]
pub enum Error {
    /// Underlying socket or stream I/O error.
    Io(std::io::Error),
    /// MessagePack encoding error.
    Encode(EncodeError),
    /// MessagePack decoding error.
    Decode(DecodeError),
    /// Typed parameter or result conversion error.
    Conversion(ConversionError),
    /// Remote RPC error returned by the router or peer.
    Rpc(RpcError),
    /// Outbound RPC call exceeded the configured timeout.
    Timeout,
    /// Underlying client connection or session is closed.
    Closed,
    /// Invalid argument passed to a local API call (for example `max_bytes <= 127`).
    InvalidArgument(String),
}

/// Crate result alias.
pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::Encode(err) => write!(f, "encode error: {err}"),
            Self::Decode(err) => write!(f, "decode error: {err}"),
            Self::Conversion(err) => write!(f, "type conversion error: {err}"),
            Self::Rpc(err) => write!(f, "RPC error: {err}"),
            Self::Timeout => f.write_str("RPC call timed out"),
            Self::Closed => f.write_str("router client connection is closed"),
            Self::InvalidArgument(msg) => write!(f, "invalid argument: {msg}"),
        }
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Encode(err) => Some(err),
            Self::Decode(err) => Some(err),
            Self::Conversion(err) => Some(err),
            Self::Rpc(err) => Some(err),
            Self::Timeout | Self::Closed | Self::InvalidArgument(_) => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<EncodeError> for Error {
    fn from(err: EncodeError) -> Self {
        Self::Encode(err)
    }
}

impl From<DecodeError> for Error {
    fn from(err: DecodeError) -> Self {
        Self::Decode(err)
    }
}

impl From<ConversionError> for Error {
    fn from(err: ConversionError) -> Self {
        Self::Conversion(err)
    }
}

impl From<RpcError> for Error {
    fn from(err: RpcError) -> Self {
        Self::Rpc(err)
    }
}
