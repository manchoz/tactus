//! Dynamic MessagePack value representation and typed parameter/return conversions.

use std::collections::{BTreeMap, HashMap};
use std::error::Error as StdError;
use std::fmt;
use std::hash::{BuildHasher, Hash};
use std::ops::Deref;

/// A dynamically typed MessagePack value.
///
/// Note on integer decoding: MessagePack wire positive fixints (`0x00..=0x7f`) and
/// `uint8`/`uint16`/`uint32` values decode to [`Value::Int`] when they fit in `i64`,
/// and [`Value::Uint`] for `uint64` values exceeding `i64::MAX`. Both variants encode
/// identically as compact MessagePack integers, and [`Value::as_i64`] / [`Value::as_u64`]
/// accept both seamlessly.
///
/// `f32` (`0xca`) and `f64` (`0xcb`) are preserved distinctly via [`Value::F32`] and
/// [`Value::F64`] for exact C++ `Arduino_RPClite` type-checking interoperability.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Value {
    /// MessagePack `nil` (`0xc0`).
    #[default]
    Nil,
    /// MessagePack boolean (`0xc2` / `0xc3`).
    Bool(bool),
    /// Signed integer (including non-negative integers `<= i64::MAX`).
    Int(i64),
    /// Unsigned integer (used for `u64` values or explicitly constructed unsigned values).
    Uint(u64),
    /// IEEE-754 single-precision float (`0xca`).
    F32(f32),
    /// IEEE-754 double-precision float (`0xcb`).
    F64(f64),
    /// UTF-8 string.
    Str(String),
    /// Raw binary byte buffer (`bin8`/`bin16`/`bin32`).
    Bin(Vec<u8>),
    /// Sequence of MessagePack values.
    Array(Vec<Value>),
    /// Ordered key-value pairs.
    Map(Vec<(Value, Value)>),
    /// MessagePack extension type (`ext` / `fixext`).
    Ext {
        /// Application-defined MessagePack extension type tag (`-128..=127`).
        kind: i8,
        /// Raw extension payload bytes.
        data: Vec<u8>,
    },
}

impl Value {
    /// Returns `true` if this value is [`Value::Nil`].
    #[must_use]
    pub const fn is_nil(&self) -> bool {
        matches!(self, Self::Nil)
    }

    /// Returns the boolean value if this is [`Value::Bool`].
    #[must_use]
    pub const fn as_bool(&self) -> Option<bool> {
        match *self {
            Self::Bool(b) => Some(b),
            _ => None,
        }
    }

    /// Returns the value as `i64` if it is an integer within `i64` bounds.
    #[must_use]
    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Self::Int(i) => Some(i),
            Self::Uint(u) => i64::try_from(u).ok(),
            _ => None,
        }
    }

    /// Returns the value as `u64` if it is a non-negative integer.
    #[must_use]
    pub fn as_u64(&self) -> Option<u64> {
        match *self {
            Self::Uint(u) => Some(u),
            Self::Int(i) => u64::try_from(i).ok(),
            _ => None,
        }
    }

    /// Returns the value as `u32` if it is a non-negative integer within `u32` bounds.
    #[must_use]
    pub fn as_u32(&self) -> Option<u32> {
        self.as_u64().and_then(|u| u32::try_from(u).ok())
    }

    /// Returns the value as `f32` if it is a float or integer.
    #[must_use]
    pub fn as_f32(&self) -> Option<f32> {
        match *self {
            Self::F32(f) => Some(f),
            Self::F64(f) => Some(f as f32),
            Self::Int(i) => Some(i as f32),
            Self::Uint(u) => Some(u as f32),
            _ => None,
        }
    }

    /// Returns the value as `f64` if it is a float or integer.
    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            Self::F64(f) => Some(f),
            Self::F32(f) => Some(f64::from(f)),
            Self::Int(i) => Some(i as f64),
            Self::Uint(u) => Some(u as f64),
            _ => None,
        }
    }

    /// Returns the string slice if this is [`Value::Str`].
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Returns the byte slice if this is [`Value::Bin`].
    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bin(b) => Some(b.as_slice()),
            _ => None,
        }
    }

    /// Returns the element slice if this is [`Value::Array`].
    #[must_use]
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Self::Array(items) => Some(items.as_slice()),
            _ => None,
        }
    }

    /// Returns the entry slice if this is [`Value::Map`].
    #[must_use]
    pub fn as_map(&self) -> Option<&[(Value, Value)]> {
        match self {
            Self::Map(entries) => Some(entries.as_slice()),
            _ => None,
        }
    }

    /// Returns a human-readable type name for diagnostics.
    #[must_use]
    pub const fn type_name(&self) -> &'static str {
        match self {
            Self::Nil => "nil",
            Self::Bool(_) => "bool",
            Self::Int(_) => "int",
            Self::Uint(_) => "uint",
            Self::F32(_) => "f32",
            Self::F64(_) => "f64",
            Self::Str(_) => "string",
            Self::Bin(_) => "bin",
            Self::Array(_) => "array",
            Self::Map(_) => "map",
            Self::Ext { .. } => "ext",
        }
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

impl From<i32> for Value {
    fn from(v: i32) -> Self {
        Self::Int(i64::from(v))
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Self::Int(v)
    }
}

impl From<u32> for Value {
    fn from(v: u32) -> Self {
        Self::Uint(u64::from(v))
    }
}

impl From<u64> for Value {
    fn from(v: u64) -> Self {
        Self::Uint(v)
    }
}

impl From<f32> for Value {
    fn from(v: f32) -> Self {
        Self::F32(v)
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Self::F64(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Self::Str(v.to_string())
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Self::Str(v)
    }
}

impl From<Vec<Value>> for Value {
    fn from(v: Vec<Value>) -> Self {
        Self::Array(v)
    }
}

/// Wrapper for explicit MessagePack binary (`bin`) encoding/decoding.
///
/// Unlike `Vec<u8>` (which encodes as a MessagePack `Array` of `u8` for
/// `Arduino_RPClite` compatibility), `Bytes` encodes as a MessagePack `Bin`
/// payload. When decoding, [`Bytes::from_value`] accepts both `Value::Bin` and
/// `Value::Array` of `u8`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Bytes(pub Vec<u8>);

impl Bytes {
    /// Unwrap into the underlying `Vec<u8>`.
    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(v: Vec<u8>) -> Self {
        Self(v)
    }
}

impl From<&[u8]> for Bytes {
    fn from(v: &[u8]) -> Self {
        Self(v.to_vec())
    }
}

impl From<Bytes> for Vec<u8> {
    fn from(v: Bytes) -> Self {
        v.0
    }
}

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl Deref for Bytes {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<Bytes> for Value {
    fn from(v: Bytes) -> Self {
        Self::Bin(v.0)
    }
}

/// Error returned when converting between [`Value`] and strongly typed Rust values or RPC params.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversionError {
    /// Human-readable description of the conversion or arity mismatch.
    pub message: String,
}

impl ConversionError {
    /// Create a new conversion error with the given message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ConversionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl StdError for ConversionError {}

/// Convert a Rust value into a MessagePack [`Value`].
pub trait IntoValue: Sized {
    /// Convert `self` into a [`Value`].
    fn into_value(self) -> Value;

    /// Fallibly convert `self` into a [`Value`].
    fn try_into_value(self) -> Result<Value, ConversionError> {
        Ok(self.into_value())
    }
}

/// Convert a MessagePack [`Value`] into a typed Rust value.
pub trait FromValue: Sized {
    /// Attempt to convert `value` into `Self`.
    fn from_value(value: Value) -> Result<Self, ConversionError>;
}

/// Convert typed RPC arguments into a positional parameter array.
pub trait IntoParams {
    /// Convert `self` into positional MessagePack parameters.
    fn into_params(self) -> Result<Vec<Value>, ConversionError>;
}

/// Extract typed RPC arguments from a positional parameter array.
pub trait FromParams: Sized {
    /// Validate arity and extract typed parameters from `params`.
    fn from_params(params: Vec<Value>) -> Result<Self, ConversionError>;
}

impl IntoValue for Value {
    fn into_value(self) -> Value {
        self
    }
}

impl FromValue for Value {
    fn from_value(value: Value) -> Result<Self, ConversionError> {
        Ok(value)
    }
}

impl IntoValue for () {
    fn into_value(self) -> Value {
        Value::Nil
    }
}

impl FromValue for () {
    fn from_value(value: Value) -> Result<Self, ConversionError> {
        match value {
            Value::Nil => Ok(()),
            other => Err(ConversionError::new(format!(
                "expected nil, got {}",
                other.type_name()
            ))),
        }
    }
}

impl IntoValue for bool {
    fn into_value(self) -> Value {
        Value::Bool(self)
    }
}

impl FromValue for bool {
    fn from_value(value: Value) -> Result<Self, ConversionError> {
        match value {
            Value::Bool(b) => Ok(b),
            other => Err(ConversionError::new(format!(
                "expected bool, got {}",
                other.type_name()
            ))),
        }
    }
}

macro_rules! impl_signed_int_value {
    ($($ty:ty),* $(,)?) => {
        $(
            impl IntoValue for $ty {
                fn into_value(self) -> Value {
                    Value::Int(self as i64)
                }
            }

            impl FromValue for $ty {
                fn from_value(value: Value) -> Result<Self, ConversionError> {
                    let Some(n) = value.as_i64() else {
                        return Err(ConversionError::new(format!(
                            "expected {}, got {}",
                            stringify!($ty),
                            value.type_name()
                        )));
                    };
                    <$ty>::try_from(n).map_err(|_| {
                        ConversionError::new(format!(
                            "integer {n} out of range for {}",
                            stringify!($ty)
                        ))
                    })
                }
            }
        )*
    };
}

macro_rules! impl_unsigned_int_value {
    ($($ty:ty),* $(,)?) => {
        $(
            impl IntoValue for $ty {
                fn into_value(self) -> Value {
                    Value::Uint(self as u64)
                }
            }

            impl FromValue for $ty {
                fn from_value(value: Value) -> Result<Self, ConversionError> {
                    let Some(n) = value.as_u64() else {
                        return Err(ConversionError::new(format!(
                            "expected {}, got {}",
                            stringify!($ty),
                            value.type_name()
                        )));
                    };
                    <$ty>::try_from(n).map_err(|_| {
                        ConversionError::new(format!(
                            "integer {n} out of range for {}",
                            stringify!($ty)
                        ))
                    })
                }
            }
        )*
    };
}

impl_signed_int_value!(i8, i16, i32, i64, isize);
impl_unsigned_int_value!(u8, u16, u32, u64, usize);

impl IntoValue for f32 {
    fn into_value(self) -> Value {
        Value::F32(self)
    }
}

impl FromValue for f32 {
    fn from_value(value: Value) -> Result<Self, ConversionError> {
        value
            .as_f32()
            .ok_or_else(|| ConversionError::new(format!("expected f32, got {}", value.type_name())))
    }
}

impl IntoValue for f64 {
    fn into_value(self) -> Value {
        Value::F64(self)
    }
}

impl FromValue for f64 {
    fn from_value(value: Value) -> Result<Self, ConversionError> {
        value
            .as_f64()
            .ok_or_else(|| ConversionError::new(format!("expected f64, got {}", value.type_name())))
    }
}

impl IntoValue for String {
    fn into_value(self) -> Value {
        Value::Str(self)
    }
}

impl IntoValue for &str {
    fn into_value(self) -> Value {
        Value::Str(self.to_string())
    }
}

impl FromValue for String {
    fn from_value(value: Value) -> Result<Self, ConversionError> {
        match value {
            Value::Str(s) => Ok(s),
            other => Err(ConversionError::new(format!(
                "expected string, got {}",
                other.type_name()
            ))),
        }
    }
}

impl IntoValue for Bytes {
    fn into_value(self) -> Value {
        Value::Bin(self.0)
    }
}

impl FromValue for Bytes {
    fn from_value(value: Value) -> Result<Self, ConversionError> {
        match value {
            Value::Bin(b) => Ok(Self(b)),
            Value::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for (idx, item) in items.into_iter().enumerate() {
                    let byte = u8::from_value(item).map_err(|err| {
                        ConversionError::new(format!("invalid byte at index {idx}: {err}"))
                    })?;
                    out.push(byte);
                }
                Ok(Self(out))
            }
            other => Err(ConversionError::new(format!(
                "expected bin or byte array, got {}",
                other.type_name()
            ))),
        }
    }
}

impl<T: IntoValue> IntoValue for Option<T> {
    fn into_value(self) -> Value {
        match self {
            Some(v) => v.into_value(),
            None => Value::Nil,
        }
    }

    fn try_into_value(self) -> Result<Value, ConversionError> {
        match self {
            Some(v) => v.try_into_value(),
            None => Ok(Value::Nil),
        }
    }
}

impl<T: FromValue> FromValue for Option<T> {
    fn from_value(value: Value) -> Result<Self, ConversionError> {
        match value {
            Value::Nil => Ok(None),
            other => T::from_value(other).map(Some),
        }
    }
}

impl<T: IntoValue> IntoValue for Vec<T> {
    fn into_value(self) -> Value {
        Value::Array(self.into_iter().map(IntoValue::into_value).collect())
    }

    fn try_into_value(self) -> Result<Value, ConversionError> {
        self.into_iter()
            .map(IntoValue::try_into_value)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array)
    }
}

impl<T: FromValue> FromValue for Vec<T> {
    fn from_value(value: Value) -> Result<Self, ConversionError> {
        match value {
            Value::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for (idx, item) in items.into_iter().enumerate() {
                    out.push(T::from_value(item).map_err(|err| {
                        ConversionError::new(format!("invalid array element at index {idx}: {err}"))
                    })?);
                }
                Ok(out)
            }
            other => Err(ConversionError::new(format!(
                "expected array, got {}",
                other.type_name()
            ))),
        }
    }
}

impl<K: IntoValue, V: IntoValue> IntoValue for BTreeMap<K, V> {
    fn into_value(self) -> Value {
        Value::Map(
            self.into_iter()
                .map(|(k, v)| (k.into_value(), v.into_value()))
                .collect(),
        )
    }

    fn try_into_value(self) -> Result<Value, ConversionError> {
        self.into_iter()
            .map(|(k, v)| Ok((k.try_into_value()?, v.try_into_value()?)))
            .collect::<Result<Vec<_>, ConversionError>>()
            .map(Value::Map)
    }
}

impl<K: Ord + FromValue, V: FromValue> FromValue for BTreeMap<K, V> {
    fn from_value(value: Value) -> Result<Self, ConversionError> {
        match value {
            Value::Map(entries) => {
                let mut map = BTreeMap::new();
                for (k, v) in entries {
                    map.insert(K::from_value(k)?, V::from_value(v)?);
                }
                Ok(map)
            }
            other => Err(ConversionError::new(format!(
                "expected map, got {}",
                other.type_name()
            ))),
        }
    }
}

impl<K: IntoValue, V: IntoValue, S> IntoValue for HashMap<K, V, S> {
    fn into_value(self) -> Value {
        Value::Map(
            self.into_iter()
                .map(|(k, v)| (k.into_value(), v.into_value()))
                .collect(),
        )
    }

    fn try_into_value(self) -> Result<Value, ConversionError> {
        self.into_iter()
            .map(|(k, v)| Ok((k.try_into_value()?, v.try_into_value()?)))
            .collect::<Result<Vec<_>, ConversionError>>()
            .map(Value::Map)
    }
}

impl<K: Eq + Hash + FromValue, V: FromValue, S: BuildHasher + Default> FromValue
    for HashMap<K, V, S>
{
    fn from_value(value: Value) -> Result<Self, ConversionError> {
        match value {
            Value::Map(entries) => {
                let mut map = HashMap::with_capacity_and_hasher(entries.len(), S::default());
                for (k, v) in entries {
                    map.insert(K::from_value(k)?, V::from_value(v)?);
                }
                Ok(map)
            }
            other => Err(ConversionError::new(format!(
                "expected map, got {}",
                other.type_name()
            ))),
        }
    }
}

impl IntoParams for () {
    fn into_params(self) -> Result<Vec<Value>, ConversionError> {
        Ok(Vec::new())
    }
}

impl FromParams for () {
    fn from_params(params: Vec<Value>) -> Result<Self, ConversionError> {
        if params.is_empty() {
            Ok(())
        } else {
            Err(ConversionError::new(format!(
                "too many parameters: expected 0, got {}",
                params.len()
            )))
        }
    }
}

impl IntoParams for Vec<Value> {
    fn into_params(self) -> Result<Vec<Value>, ConversionError> {
        Ok(self)
    }
}

impl FromParams for Vec<Value> {
    fn from_params(params: Vec<Value>) -> Result<Self, ConversionError> {
        Ok(params)
    }
}

macro_rules! count_idents {
    () => { 0usize };
    ($head:ident $(, $tail:ident)*) => { 1usize + count_idents!($($tail),*) };
}

macro_rules! impl_tuple_params {
    ($($name:ident : $idx:tt),+ $(,)?) => {
        impl<$($name: IntoValue),+> IntoParams for ($($name,)+) {
            fn into_params(self) -> Result<Vec<Value>, ConversionError> {
                Ok(vec![$(self.$idx.try_into_value()?),+])
            }
        }

        impl<$($name: FromValue),+> FromParams for ($($name,)+) {
            fn from_params(params: Vec<Value>) -> Result<Self, ConversionError> {
                const EXPECTED: usize = count_idents!($($name),+);
                if params.len() < EXPECTED {
                    return Err(ConversionError::new(format!(
                        "missing call parameters: expected {EXPECTED}, got {}",
                        params.len()
                    )));
                }
                if params.len() > EXPECTED {
                    return Err(ConversionError::new(format!(
                        "too many parameters: expected {EXPECTED}, got {}",
                        params.len()
                    )));
                }
                let mut iter = params.into_iter();
                Ok(($(
                    <$name as FromValue>::from_value(
                        iter.next().expect("length checked above"),
                    )
                    .map_err(|err| {
                        ConversionError::new(format!(
                            "wrong type parameter in position {}: {err}",
                            $idx
                        ))
                    })?,
                )+))
            }
        }
    };
}

impl_tuple_params!(T0: 0);
impl_tuple_params!(T0: 0, T1: 1);
impl_tuple_params!(T0: 0, T1: 1, T2: 2);
impl_tuple_params!(T0: 0, T1: 1, T2: 2, T3: 3);
impl_tuple_params!(T0: 0, T1: 1, T2: 2, T3: 3, T4: 4);
impl_tuple_params!(T0: 0, T1: 1, T2: 2, T3: 3, T4: 4, T5: 5);
impl_tuple_params!(T0: 0, T1: 1, T2: 2, T3: 3, T4: 4, T5: 5, T6: 6);
impl_tuple_params!(T0: 0, T1: 1, T2: 2, T3: 3, T4: 4, T5: 5, T6: 6, T7: 7);
