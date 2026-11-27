//! Optional `serde` serialization/deserialization support directly to/from [`Value`].

use std::fmt;

use serde::de::{
    self, DeserializeOwned, EnumAccess, IntoDeserializer, MapAccess, SeqAccess, VariantAccess,
    Visitor,
};
use serde::ser::{
    self, Serialize, SerializeMap, SerializeSeq, SerializeStruct, SerializeStructVariant,
    SerializeTuple, SerializeTupleStruct, SerializeTupleVariant, Serializer,
};

use crate::value::{ConversionError, FromValue, IntoValue, Value};

/// Wrapper allowing any `serde::Serialize` / `serde::DeserializeOwned` type to be
/// passed as an RPC parameter or returned as an RPC result.
///
/// Outbound `Client::call`, `Client::notify`, and `provide` handler returns use
/// [`IntoValue::try_into_value`] and propagate serialization failures (such as `i128`/`u128`
/// values outside 64-bit bounds) as [`ConversionError`]. Calling [`IntoValue::into_value`]
/// directly falls back to `Value::Nil` on error; prefer `try_into_value()` or [`to_value`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Serde<T>(pub T);

impl<T: Serialize> IntoValue for Serde<T> {
    fn into_value(self) -> Value {
        to_value(&self.0).unwrap_or(Value::Nil)
    }

    fn try_into_value(self) -> Result<Value, ConversionError> {
        to_value(&self.0)
    }
}

impl<T: DeserializeOwned> FromValue for Serde<T> {
    fn from_value(value: Value) -> Result<Self, ConversionError> {
        from_value(value).map(Serde)
    }
}

/// Convert any `serde::Serialize` value into a [`Value`].
pub fn to_value<T: Serialize>(value: &T) -> Result<Value, ConversionError> {
    value.serialize(ValueSerializer)
}

/// Convert a [`Value`] into any `serde::de::DeserializeOwned` type.
pub fn from_value<T: DeserializeOwned>(value: Value) -> Result<T, ConversionError> {
    T::deserialize(value)
}

impl ser::Error for ConversionError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self::new(msg.to_string())
    }
}

impl de::Error for ConversionError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self::new(msg.to_string())
    }
}

struct ValueSerializer;

impl Serializer for ValueSerializer {
    type Ok = Value;
    type Error = ConversionError;

    type SerializeSeq = SeqSerializer;
    type SerializeTuple = SeqSerializer;
    type SerializeTupleStruct = SeqSerializer;
    type SerializeTupleVariant = TupleVariantSerializer;
    type SerializeMap = MapSerializer;
    type SerializeStruct = StructSerializer;
    type SerializeStructVariant = StructVariantSerializer;

    fn serialize_bool(self, v: bool) -> Result<Value, Self::Error> {
        Ok(Value::Bool(v))
    }

    fn serialize_i8(self, v: i8) -> Result<Value, Self::Error> {
        Ok(Value::Int(i64::from(v)))
    }

    fn serialize_i16(self, v: i16) -> Result<Value, Self::Error> {
        Ok(Value::Int(i64::from(v)))
    }

    fn serialize_i32(self, v: i32) -> Result<Value, Self::Error> {
        Ok(Value::Int(i64::from(v)))
    }

    fn serialize_i64(self, v: i64) -> Result<Value, Self::Error> {
        Ok(Value::Int(v))
    }

    fn serialize_i128(self, v: i128) -> Result<Value, Self::Error> {
        i64::try_from(v)
            .map(Value::Int)
            .map_err(|_| ConversionError::new(format!("i128 value {v} out of i64 range")))
    }

    fn serialize_u8(self, v: u8) -> Result<Value, Self::Error> {
        Ok(Value::Uint(u64::from(v)))
    }

    fn serialize_u16(self, v: u16) -> Result<Value, Self::Error> {
        Ok(Value::Uint(u64::from(v)))
    }

    fn serialize_u32(self, v: u32) -> Result<Value, Self::Error> {
        Ok(Value::Uint(u64::from(v)))
    }

    fn serialize_u64(self, v: u64) -> Result<Value, Self::Error> {
        Ok(Value::Uint(v))
    }

    fn serialize_u128(self, v: u128) -> Result<Value, Self::Error> {
        u64::try_from(v)
            .map(Value::Uint)
            .map_err(|_| ConversionError::new(format!("u128 value {v} out of u64 range")))
    }

    fn serialize_f32(self, v: f32) -> Result<Value, Self::Error> {
        Ok(Value::F32(v))
    }

    fn serialize_f64(self, v: f64) -> Result<Value, Self::Error> {
        Ok(Value::F64(v))
    }

    fn serialize_char(self, v: char) -> Result<Value, Self::Error> {
        Ok(Value::Str(v.to_string()))
    }

    fn serialize_str(self, v: &str) -> Result<Value, Self::Error> {
        Ok(Value::Str(v.to_string()))
    }

    fn serialize_bytes(self, v: &[u8]) -> Result<Value, Self::Error> {
        Ok(Value::Bin(v.to_vec()))
    }

    fn serialize_none(self) -> Result<Value, Self::Error> {
        Ok(Value::Nil)
    }

    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<Value, Self::Error> {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<Value, Self::Error> {
        Ok(Value::Nil)
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<Value, Self::Error> {
        Ok(Value::Nil)
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
    ) -> Result<Value, Self::Error> {
        Ok(Value::Str(variant.to_string()))
    }

    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Value, Self::Error> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Value, Self::Error> {
        Ok(Value::Map(vec![(
            Value::Str(variant.to_string()),
            value.serialize(Self)?,
        )]))
    }

    fn serialize_seq(self, len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        Ok(SeqSerializer {
            items: Vec::with_capacity(len.unwrap_or(0)),
        })
    }

    fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        Ok(SeqSerializer {
            items: Vec::with_capacity(len),
        })
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        Ok(SeqSerializer {
            items: Vec::with_capacity(len),
        })
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        Ok(TupleVariantSerializer {
            variant: variant.to_string(),
            items: Vec::with_capacity(len),
        })
    }

    fn serialize_map(self, len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        Ok(MapSerializer {
            entries: Vec::with_capacity(len.unwrap_or(0)),
            next_key: None,
        })
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        Ok(StructSerializer {
            entries: Vec::with_capacity(len),
        })
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        Ok(StructVariantSerializer {
            variant: variant.to_string(),
            entries: Vec::with_capacity(len),
        })
    }
}

struct SeqSerializer {
    items: Vec<Value>,
}

impl SerializeSeq for SeqSerializer {
    type Ok = Value;
    type Error = ConversionError;

    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Self::Error> {
        self.items.push(to_value(&value)?);
        Ok(())
    }

    fn end(self) -> Result<Value, Self::Error> {
        Ok(Value::Array(self.items))
    }
}

impl SerializeTuple for SeqSerializer {
    type Ok = Value;
    type Error = ConversionError;

    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Self::Error> {
        SerializeSeq::serialize_element(self, value)
    }

    fn end(self) -> Result<Value, Self::Error> {
        Ok(Value::Array(self.items))
    }
}

impl SerializeTupleStruct for SeqSerializer {
    type Ok = Value;
    type Error = ConversionError;

    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Self::Error> {
        SerializeSeq::serialize_element(self, value)
    }

    fn end(self) -> Result<Value, Self::Error> {
        Ok(Value::Array(self.items))
    }
}

struct TupleVariantSerializer {
    variant: String,
    items: Vec<Value>,
}

impl SerializeTupleVariant for TupleVariantSerializer {
    type Ok = Value;
    type Error = ConversionError;

    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Self::Error> {
        self.items.push(to_value(&value)?);
        Ok(())
    }

    fn end(self) -> Result<Value, Self::Error> {
        Ok(Value::Map(vec![(
            Value::Str(self.variant),
            Value::Array(self.items),
        )]))
    }
}

struct MapSerializer {
    entries: Vec<(Value, Value)>,
    next_key: Option<Value>,
}

impl SerializeMap for MapSerializer {
    type Ok = Value;
    type Error = ConversionError;

    fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result<(), Self::Error> {
        self.next_key = Some(to_value(&key)?);
        Ok(())
    }

    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Self::Error> {
        let key = self
            .next_key
            .take()
            .ok_or_else(|| ConversionError::new("serialize_value called before serialize_key"))?;
        self.entries.push((key, to_value(&value)?));
        Ok(())
    }

    fn end(self) -> Result<Value, Self::Error> {
        Ok(Value::Map(self.entries))
    }
}

struct StructSerializer {
    entries: Vec<(Value, Value)>,
}

impl SerializeStruct for StructSerializer {
    type Ok = Value;
    type Error = ConversionError;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Self::Error> {
        self.entries
            .push((Value::Str(key.to_string()), to_value(&value)?));
        Ok(())
    }

    fn end(self) -> Result<Value, Self::Error> {
        Ok(Value::Map(self.entries))
    }
}

struct StructVariantSerializer {
    variant: String,
    entries: Vec<(Value, Value)>,
}

impl SerializeStructVariant for StructVariantSerializer {
    type Ok = Value;
    type Error = ConversionError;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Self::Error> {
        self.entries
            .push((Value::Str(key.to_string()), to_value(&value)?));
        Ok(())
    }

    fn end(self) -> Result<Value, Self::Error> {
        Ok(Value::Map(vec![(
            Value::Str(self.variant),
            Value::Map(self.entries),
        )]))
    }
}

impl<'de> de::Deserializer<'de> for Value {
    type Error = ConversionError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self {
            Self::Nil => visitor.visit_unit(),
            Self::Bool(b) => visitor.visit_bool(b),
            Self::Int(i) => visitor.visit_i64(i),
            Self::Uint(u) => visitor.visit_u64(u),
            Self::F32(f) => visitor.visit_f32(f),
            Self::F64(f) => visitor.visit_f64(f),
            Self::Str(s) => visitor.visit_string(s),
            Self::Bin(b) => visitor.visit_byte_buf(b),
            Self::Array(items) => visitor.visit_seq(ValueSeqAccess::new(items)),
            Self::Map(entries) => visitor.visit_map(ValueMapAccess::new(entries)),
            Self::Ext { data, .. } => visitor.visit_byte_buf(data),
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self {
            Self::Nil => visitor.visit_none(),
            other => visitor.visit_some(other),
        }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self {
            Self::Bin(b) => visitor.visit_byte_buf(b),
            Self::Str(s) => visitor.visit_byte_buf(s.into_bytes()),
            Self::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(u8::from_value(item)?);
                }
                visitor.visit_byte_buf(out)
            }
            other => Err(ConversionError::new(format!(
                "expected bytes, got {}",
                other.type_name()
            ))),
        }
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_bytes(visitor)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        match self {
            Self::Str(variant) => visitor.visit_enum(
                IntoDeserializer::<ConversionError>::into_deserializer(variant),
            ),
            Self::Map(mut entries) if entries.len() == 1 => {
                let (key, val) = entries.remove(0);
                let variant = String::from_value(key)?;
                visitor.visit_enum(ValueEnumAccess { variant, val })
            }
            other => Err(ConversionError::new(format!(
                "expected enum variant string or single-entry map, got {}",
                other.type_name()
            ))),
        }
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        unit unit_struct seq tuple tuple_struct map struct identifier ignored_any
    }
}

struct ValueSeqAccess {
    iter: std::vec::IntoIter<Value>,
}

impl ValueSeqAccess {
    fn new(items: Vec<Value>) -> Self {
        Self {
            iter: items.into_iter(),
        }
    }
}

impl<'de> SeqAccess<'de> for ValueSeqAccess {
    type Error = ConversionError;

    fn next_element_seed<T: de::DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        match self.iter.next() {
            Some(v) => seed.deserialize(v).map(Some),
            None => Ok(None),
        }
    }
}

struct ValueMapAccess {
    iter: std::vec::IntoIter<(Value, Value)>,
    next_val: Option<Value>,
}

impl ValueMapAccess {
    fn new(entries: Vec<(Value, Value)>) -> Self {
        Self {
            iter: entries.into_iter(),
            next_val: None,
        }
    }
}

impl<'de> MapAccess<'de> for ValueMapAccess {
    type Error = ConversionError;

    fn next_key_seed<K: de::DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        match self.iter.next() {
            Some((k, v)) => {
                self.next_val = Some(v);
                seed.deserialize(k).map(Some)
            }
            None => Ok(None),
        }
    }

    fn next_value_seed<V: de::DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        let v = self
            .next_val
            .take()
            .ok_or_else(|| ConversionError::new("next_value_seed called without key"))?;
        seed.deserialize(v)
    }
}

struct ValueEnumAccess {
    variant: String,
    val: Value,
}

impl<'de> EnumAccess<'de> for ValueEnumAccess {
    type Error = ConversionError;
    type Variant = ValueVariantAccess;

    fn variant_seed<V: de::DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, Self::Variant), Self::Error> {
        let v = seed.deserialize(IntoDeserializer::<ConversionError>::into_deserializer(
            self.variant,
        ))?;
        Ok((v, ValueVariantAccess { val: self.val }))
    }
}

struct ValueVariantAccess {
    val: Value,
}

impl<'de> VariantAccess<'de> for ValueVariantAccess {
    type Error = ConversionError;

    fn unit_variant(self) -> Result<(), Self::Error> {
        match self.val {
            Value::Nil => Ok(()),
            other => Err(ConversionError::new(format!(
                "expected nil for unit variant, got {}",
                other.type_name()
            ))),
        }
    }

    fn newtype_variant_seed<T: de::DeserializeSeed<'de>>(
        self,
        seed: T,
    ) -> Result<T::Value, Self::Error> {
        seed.deserialize(self.val)
    }

    fn tuple_variant<V: Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        de::Deserializer::deserialize_seq(self.val, visitor)
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        de::Deserializer::deserialize_map(self.val, visitor)
    }
}

impl Serialize for Value {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Nil => serializer.serialize_none(),
            Self::Bool(b) => serializer.serialize_bool(*b),
            Self::Int(i) => serializer.serialize_i64(*i),
            Self::Uint(u) => serializer.serialize_u64(*u),
            Self::F32(f) => serializer.serialize_f32(*f),
            Self::F64(f) => serializer.serialize_f64(*f),
            Self::Str(s) => serializer.serialize_str(s),
            Self::Bin(b) => serializer.serialize_bytes(b),
            Self::Array(items) => {
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Self::Map(entries) => {
                let mut map = serializer.serialize_map(Some(entries.len()))?;
                for (k, v) in entries {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
            Self::Ext { data, .. } => serializer.serialize_bytes(data),
        }
    }
}

impl<'de> serde::Deserialize<'de> for Value {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ValueVisitor;

        impl<'de> Visitor<'de> for ValueVisitor {
            type Value = Value;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any MessagePack-compatible value")
            }

            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
                Ok(Value::Bool(v))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
                Ok(Value::Int(v))
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
                Ok(Value::Uint(v))
            }

            fn visit_f32<E: de::Error>(self, v: f32) -> Result<Value, E> {
                Ok(Value::F32(v))
            }

            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
                Ok(Value::F64(v))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
                Ok(Value::Str(v.to_string()))
            }

            fn visit_string<E: de::Error>(self, v: String) -> Result<Value, E> {
                Ok(Value::Str(v))
            }

            fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Value, E> {
                Ok(Value::Bin(v.to_vec()))
            }

            fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Value, E> {
                Ok(Value::Bin(v))
            }

            fn visit_none<E: de::Error>(self) -> Result<Value, E> {
                Ok(Value::Nil)
            }

            fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
                Ok(Value::Nil)
            }

            fn visit_some<D: serde::Deserializer<'de>>(
                self,
                deserializer: D,
            ) -> Result<Value, D::Error> {
                serde::Deserialize::deserialize(deserializer)
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
                let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(elem) = seq.next_element()? {
                    items.push(elem);
                }
                Ok(Value::Array(items))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
                let mut entries = Vec::with_capacity(map.size_hint().unwrap_or(0));
                while let Some((k, v)) = map.next_entry()? {
                    entries.push((k, v));
                }
                Ok(Value::Map(entries))
            }
        }

        deserializer.deserialize_any(ValueVisitor)
    }
}
