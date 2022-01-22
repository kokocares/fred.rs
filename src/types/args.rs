use crate::error::{RedisError, RedisErrorKind};
use crate::interfaces::ClientLike;
use crate::protocol::connection::OK;
use crate::protocol::utils as protocol_utils;
use crate::types::{FromRedis, GeoPosition, NIL, QUEUED};
use crate::utils;
use bytes::{Buf, Bytes};
use bytes_utils::Str;
use float_cmp::approx_eq;
use redis_protocol::resp2::types::NULL;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::convert::{TryFrom, TryInto};
use std::hash::{Hash, Hasher};
use std::iter::FromIterator;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::{fmt, mem, str};
use tokio_stream::StreamExt;

static_str!(TRUE_STR, "true");
static_str!(FALSE_STR, "false");

macro_rules! impl_string_or_number(
  ($t:ty) => {
    impl From<$t> for StringOrNumber {
      fn from(val: $t) -> Self {
        StringOrNumber::Number(val as i64)
      }
    }
  }
);

macro_rules! impl_from_str_for_redis_key(
  ($t:ty) => {
    impl From<$t> for RedisKey {
      fn from(val: $t) -> Self {
        RedisKey { key: val.to_string().into() }
      }
    }
  }
);

/// An argument representing a string or number.
#[derive(Clone, Debug)]
pub enum StringOrNumber {
  String(Str),
  Number(i64),
  Double(f64),
}

impl PartialEq for StringOrNumber {
  fn eq(&self, other: &Self) -> bool {
    match *self {
      StringOrNumber::String(ref s) => match *other {
        StringOrNumber::String(ref _s) => s == _s,
        _ => false,
      },
      StringOrNumber::Number(ref i) => match *other {
        StringOrNumber::Number(ref _i) => *i == *_i,
        _ => false,
      },
      StringOrNumber::Double(ref d) => match *other {
        StringOrNumber::Double(ref _d) => utils::f64_eq(*d, *_d),
        _ => false,
      },
    }
  }
}

impl Eq for StringOrNumber {}

impl StringOrNumber {
  /// An optimized way to convert from `&'static str` that avoids copying or moving the underlying bytes.
  pub fn from_static_str(s: &'static str) -> Self {
    StringOrNumber::String(utils::static_str(s))
  }

  pub(crate) fn into_arg(self) -> RedisValue {
    match self {
      StringOrNumber::String(s) => RedisValue::String(s),
      StringOrNumber::Number(n) => RedisValue::Integer(n),
      StringOrNumber::Double(f) => RedisValue::Double(f),
    }
  }
}

impl TryFrom<RedisValue> for StringOrNumber {
  type Error = RedisError;

  fn try_from(value: RedisValue) -> Result<Self, Self::Error> {
    let val = match value {
      RedisValue::String(s) => StringOrNumber::String(s),
      RedisValue::Integer(i) => StringOrNumber::Number(i),
      RedisValue::Double(f) => StringOrNumber::Double(f),
      RedisValue::Bytes(b) => StringOrNumber::String(Str::from_inner(b)?),
      _ => return Err(RedisError::new(RedisErrorKind::InvalidArgument, "")),
    };

    Ok(val)
  }
}

impl<'a> From<&'a str> for StringOrNumber {
  fn from(s: &'a str) -> Self {
    StringOrNumber::String(s.into())
  }
}

impl From<String> for StringOrNumber {
  fn from(s: String) -> Self {
    StringOrNumber::String(s.into())
  }
}

impl From<Str> for StringOrNumber {
  fn from(s: Str) -> Self {
    StringOrNumber::String(s)
  }
}

impl_string_or_number!(i8);
impl_string_or_number!(i16);
impl_string_or_number!(i32);
impl_string_or_number!(i64);
impl_string_or_number!(isize);
impl_string_or_number!(u8);
impl_string_or_number!(u16);
impl_string_or_number!(u32);
impl_string_or_number!(u64);
impl_string_or_number!(usize);

impl From<f32> for StringOrNumber {
  fn from(f: f32) -> Self {
    StringOrNumber::Double(f as f64)
  }
}

impl From<f64> for StringOrNumber {
  fn from(f: f64) -> Self {
    StringOrNumber::Double(f)
  }
}

// TODO rewrite all the to_str() functions on args types to return Str

/// A key in Redis.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct RedisKey {
  key: Bytes,
}

impl RedisKey {
  /// Create a new `RedisKey` from static bytes without copying.
  pub fn from_static(b: &'static &[u8]) -> Self {
    RedisKey {
      key: Bytes::from_static(b),
    }
  }

  /// Read the key as a str slice if it can be parsed as a UTF8 string.
  pub fn as_str(&self) -> Option<&str> {
    str::from_utf8(&self.key).ok()
  }

  /// Read the key as a byte slice.
  pub fn as_bytes(&self) -> &[u8] {
    &self.key
  }

  /// Read the inner `Bytes` struct.
  pub fn inner(&self) -> &Bytes {
    &self.key
  }

  /// Read the key as a lossy UTF8 string with `String::from_utf8_lossy`.
  pub fn as_str_lossy(&self) -> Cow<str> {
    String::from_utf8_lossy(&self.key)
  }

  /// Convert the key to a UTF8 string, if possible.
  pub fn into_string(self) -> Option<String> {
    String::from_utf8(self.key.to_vec()).ok()
  }

  /// Read the inner bytes making up the key.
  pub fn into_bytes(self) -> Bytes {
    self.key
  }

  /// Hash the key to find the associated cluster [hash slot](https://redis.io/topics/cluster-spec#keys-distribution-model).
  pub fn cluster_hash(&self) -> u16 {
    redis_protocol::redis_keyslot(&self.key)
  }

  /// Read the `host:port` of the cluster node that owns the key if the client is clustered and the cluster state is known.
  pub fn cluster_owner<C>(&self, client: &C) -> Option<Arc<String>>
  where
    C: ClientLike,
  {
    if utils::is_clustered(&client.inner().config) {
      let hash_slot = self.cluster_hash();
      client
        .inner()
        .cluster_state
        .read()
        .as_ref()
        .and_then(|state| state.get_server(hash_slot).map(|slot| slot.server.clone()))
    } else {
      None
    }
  }

  /// Replace this key with an empty byte array, returning the bytes from the original key.
  pub fn take(&mut self) -> Bytes {
    self.key.split_to(self.key.len())
  }
}

impl TryFrom<RedisValue> for RedisKey {
  type Error = RedisError;

  fn try_from(value: RedisValue) -> Result<Self, Self::Error> {
    let val = match value {
      RedisValue::String(s) => RedisKey { key: s.into_inner() },
      RedisValue::Integer(i) => RedisKey {
        key: i.to_string().into(),
      },
      RedisValue::Double(f) => RedisKey {
        key: f.to_string().into(),
      },
      RedisValue::Bytes(b) => RedisKey { key: b },
      RedisValue::Boolean(b) => match b {
        true => RedisKey {
          key: TRUE_STR.clone().into_inner(),
        },
        false => RedisKey {
          key: FALSE_STR.clone().into_inner(),
        },
      },
      RedisValue::Queued => utils::static_str(QUEUED),
      _ => {
        return Err(RedisError::new(
          RedisErrorKind::InvalidArgument,
          "Cannot convert to key.",
        ))
      }
    };

    Ok(val)
  }
}

impl From<Bytes> for RedisKey {
  fn from(b: Bytes) -> Self {
    RedisKey { key: b }
  }
}

impl<'a> From<&'a [u8]> for RedisKey {
  fn from(b: &'a [u8]) -> Self {
    RedisKey { key: b.into() }
  }
}

/*
// doing this prevents MultipleKeys from being generic in its `From` implementations since the compiler cant know what to do with `Vec<u8>`.
impl From<Vec<u8>> for RedisKey {
  fn from(b: Vec<u8>) -> Self {
    RedisKey { key: b.into() }
  }
}
*/

impl From<String> for RedisKey {
  fn from(s: String) -> Self {
    RedisKey { key: s.into() }
  }
}

impl<'a> From<&'a str> for RedisKey {
  fn from(s: &'a str) -> Self {
    RedisKey { key: s.into() }
  }
}

impl From<Str> for RedisKey {
  fn from(s: Str) -> Self {
    RedisKey { key: s.into_inner() }
  }
}

impl<'a> From<&'a RedisKey> for RedisKey {
  fn from(k: &'a RedisKey) -> RedisKey {
    k.clone()
  }
}

impl_from_str_for_redis_key!(u8);
impl_from_str_for_redis_key!(u16);
impl_from_str_for_redis_key!(u32);
impl_from_str_for_redis_key!(u64);
impl_from_str_for_redis_key!(u128);
impl_from_str_for_redis_key!(usize);
impl_from_str_for_redis_key!(i8);
impl_from_str_for_redis_key!(i16);
impl_from_str_for_redis_key!(i32);
impl_from_str_for_redis_key!(i64);
impl_from_str_for_redis_key!(i128);
impl_from_str_for_redis_key!(isize);
impl_from_str_for_redis_key!(f32);
impl_from_str_for_redis_key!(f64);

/// A map of `(String, RedisValue)` pairs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RedisMap {
  pub(crate) inner: HashMap<RedisKey, RedisValue>,
}

impl RedisMap {
  /// Create a new empty map.
  pub fn new() -> Self {
    RedisMap { inner: HashMap::new() }
  }

  /// Replace the value an empty map, returning the original value.
  pub fn take(&mut self) -> Self {
    mem::replace(&mut self.inner, HashMap::new()).into()
  }

  /// Read the number of (key, value) pairs in the map.
  pub fn len(&self) -> usize {
    self.inner.len()
  }

  /// Take the inner `HashMap`.
  pub fn inner(self) -> HashMap<RedisKey, RedisValue> {
    self.inner
  }
}

impl Deref for RedisMap {
  type Target = HashMap<RedisKey, RedisValue>;

  fn deref(&self) -> &Self::Target {
    &self.inner
  }
}

impl DerefMut for RedisMap {
  fn deref_mut(&mut self) -> &mut Self::Target {
    &mut self.inner
  }
}

impl<'a> From<&'a RedisMap> for RedisMap {
  fn from(vals: &'a RedisMap) -> Self {
    vals.clone()
  }
}

impl<K, V> TryFrom<HashMap<K, V>> for RedisMap
where
  K: TryInto<RedisKey>,
  K::Error: Into<RedisError>,
  V: TryInto<RedisValue>,
  V::Error: Into<RedisError>,
{
  type Error = RedisError;

  fn try_from(value: HashMap<K, V>) -> Result<Self, Self::Error> {
    Ok(RedisMap {
      inner: value
        .into_iter()
        .map(|(k, v)| Ok((k.try_into()?, v.try_into()?)))
        .collect()?,
    })
  }
}

impl<K, V> TryFrom<BTreeMap<K, V>> for RedisMap
where
  K: TryInto<RedisKey>,
  K::Error: Into<RedisError>,
  V: TryInto<RedisValue>,
  V::Error: Into<RedisError>,
{
  type Error = RedisError;

  fn try_from(value: BTreeMap<K, V>) -> Result<Self, Self::Error> {
    Ok(RedisMap {
      inner: value
        .into_iter()
        .map(|(k, v)| Ok((k.try_into()?, v.try_into()?)))
        .collect()?,
    })
  }
}

impl<K, V> TryFrom<(K, V)> for RedisMap
where
  K: TryInto<RedisKey>,
  K::Error: Into<RedisError>,
  V: TryInto<RedisValue>,
  V::Error: Into<RedisError>,
{
  type Error = RedisError;

  fn try_from((key, value): (K, V)) -> Result<Self, Self::Error> {
    let mut inner = HashMap::with_capacity(1);
    inner.insert(key.try_into()?, value.try_into()?);
    Ok(RedisMap { inner })
  }
}

impl<K, V> TryFrom<Vec<(K, V)>> for RedisMap
where
  K: TryInto<RedisKey>,
  K::Error: Into<RedisError>,
  V: TryInto<RedisValue>,
  V::Error: Into<RedisError>,
{
  type Error = RedisError;

  fn try_from(values: Vec<(K, V)>) -> Result<Self, Self::Error> {
    let mut inner = HashMap::with_capacity(values.len());
    for (key, value) in values.into_iter() {
      inner.insert(key.try_into()?, value.try_into()?);
    }
    Ok(RedisMap { inner })
  }
}

impl<K, V> TryFrom<VecDeque<(K, V)>> for RedisMap
where
  K: TryInto<RedisKey>,
  K::Error: Into<RedisError>,
  V: TryInto<RedisValue>,
  V::Error: Into<RedisError>,
{
  type Error = RedisError;

  fn try_from(values: VecDeque<(K, V)>) -> Result<Self, Self::Error> {
    let mut inner = HashMap::with_capacity(values.len());
    for (key, value) in values.into_iter() {
      inner.insert(key.try_into()?, value.try_into()?);
    }
    Ok(RedisMap { inner })
  }
}

/// The kind of value from Redis.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RedisValueKind {
  Boolean,
  Integer,
  Double,
  String,
  Bytes,
  Null,
  Queued,
  Map,
  Array,
}

impl fmt::Display for RedisValueKind {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    let s = match *self {
      RedisValueKind::Boolean => "Boolean",
      RedisValueKind::Integer => "Integer",
      RedisValueKind::Double => "Double",
      RedisValueKind::String => "String",
      RedisValueKind::Bytes => "Bytes",
      RedisValueKind::Null => "nil",
      RedisValueKind::Queued => "Queued",
      RedisValueKind::Map => "Map",
      RedisValueKind::Array => "Array",
    };

    write!(f, "{}", s)
  }
}

/// A value used in a Redis command.
#[derive(Clone, Debug)]
pub enum RedisValue {
  /// A boolean value.
  Boolean(bool),
  /// An integer value.
  Integer(i64),
  /// A double floating point number.
  Double(f64),
  /// A string value.
  String(Str),
  /// A value to represent non-UTF8 strings or byte arrays.
  Bytes(Bytes),
  /// A `nil` value.
  Null,
  /// A special value used to indicate a MULTI block command was received by the server.
  Queued,
  /// A nested map of key/value pairs.
  Map(RedisMap),
  /// An ordered list of values.
  Array(Vec<RedisValue>),
}

impl PartialEq for RedisValue {
  fn eq(&self, other: &Self) -> bool {
    use RedisValue::*;

    match self {
      Boolean(ref s) => match other {
        Boolean(ref o) => *s == *o,
        _ => false,
      },
      Integer(ref s) => match other {
        Integer(ref o) => *s == *o,
        _ => false,
      },
      Double(ref s) => match other {
        Double(ref o) => approx_eq!(f64, *s, *o, ulps = 2),
        _ => false,
      },
      String(ref s) => match other {
        String(ref o) => s == o,
        _ => false,
      },
      Bytes(ref s) => match other {
        Bytes(ref o) => s == o,
        _ => false,
      },
      Null => match other {
        Null => true,
        _ => false,
      },
      Queued => match other {
        Queued => true,
        _ => false,
      },
      Map(ref s) => match other {
        Map(ref o) => s == o,
        _ => false,
      },
      Array(ref s) => match other {
        Array(ref o) => s == o,
        _ => false,
      },
    }
  }
}

impl Eq for RedisValue {}

impl<'a> RedisValue {
  /// Create a new `RedisValue::Bytes` from a static byte slice without copying.
  pub fn from_static(b: &'static [u8]) -> Self {
    RedisValue::Bytes(Bytes::from_static(b))
  }

  /// Create a new `RedisValue::String` from a static `str` without copying.
  pub fn from_static_str(s: &'static str) -> Self {
    RedisValue::String(utils::static_str(s))
  }

  /// Create a new `RedisValue` with the `OK` status.
  pub fn new_ok() -> Self {
    Self::from_static_str(OK)
  }

  /// Whether or not the value is a simple string OK value.
  pub fn is_ok(&self) -> bool {
    match *self {
      RedisValue::String(ref s) => s == OK,
      _ => false,
    }
  }

  /// Attempt to convert the value into an integer, returning the original string as an error if the parsing fails.
  pub fn into_integer(self) -> Result<RedisValue, RedisValue> {
    match self {
      RedisValue::String(s) => match s.parse::<i64>() {
        Ok(i) => Ok(RedisValue::Integer(i)),
        Err(_) => Err(RedisValue::String(s)),
      },
      RedisValue::Integer(i) => Ok(RedisValue::Integer(i)),
      _ => Err(self),
    }
  }

  /// Read the type of the value without any associated data.
  pub fn kind(&self) -> RedisValueKind {
    match *self {
      RedisValue::Boolean(_) => RedisValueKind::Boolean,
      RedisValue::Integer(_) => RedisValueKind::Integer,
      RedisValue::Double(_) => RedisValueKind::Double,
      RedisValue::String(_) => RedisValueKind::String,
      RedisValue::Bytes(_) => RedisValueKind::Bytes,
      RedisValue::Null => RedisValueKind::Null,
      RedisValue::Queued => RedisValueKind::Queued,
      RedisValue::Map(_) => RedisValueKind::Map,
      RedisValue::Array(_) => RedisValueKind::Array,
    }
  }

  /// Check if the value is null.
  pub fn is_null(&self) -> bool {
    match *self {
      RedisValue::Null => true,
      _ => false,
    }
  }

  /// Check if the value is an integer.
  pub fn is_integer(&self) -> bool {
    match *self {
      RedisValue::Integer(_) => true,
      _ => false,
    }
  }

  /// Check if the value is a string.
  pub fn is_string(&self) -> bool {
    match *self {
      RedisValue::String(_) => true,
      _ => false,
    }
  }

  /// Check if the value is an array of bytes.
  pub fn is_bytes(&self) -> bool {
    match *self {
      RedisValue::Bytes(_) => true,
      _ => false,
    }
  }

  /// Whether or not the value is a boolean value or can be parsed as a boolean value.
  pub fn is_boolean(&self) -> bool {
    match *self {
      RedisValue::Boolean(_) => true,
      RedisValue::Integer(i) => match i {
        0 | 1 => true,
        _ => false,
      },
      RedisValue::String(ref s) => match s.as_ref() {
        "true" | "false" | "t" | "f" | "TRUE" | "FALSE" | "T" | "F" | "1" | "0" => true,
        _ => false,
      },
      _ => false,
    }
  }

  /// Whether or not the inner value is a double or can be parsed as a double.
  pub fn is_double(&self) -> bool {
    match *self {
      RedisValue::Double(_) => true,
      RedisValue::String(ref s) => utils::redis_string_to_f64(s).is_ok(),
      _ => false,
    }
  }

  /// Check if the value is a `QUEUED` response.
  pub fn is_queued(&self) -> bool {
    match *self {
      RedisValue::Queued => true,
      _ => false,
    }
  }

  /// Whether or not the value is an array or map.
  pub fn is_aggregate_type(&self) -> bool {
    match *self {
      RedisValue::Array(_) | RedisValue::Map(_) => true,
      _ => false,
    }
  }

  /// Whether or not the value is a `RedisMap`.
  pub fn is_map(&self) -> bool {
    match *self {
      RedisValue::Map(_) => true,
      _ => false,
    }
  }

  /// Whether or not the value is a `RedisMap` or an array with an even number of elements where each even-numbered element is not an aggregate type.
  ///
  /// RESP2 and RESP3 encode maps differently, and this function can be used to duck-type maps.
  pub fn is_probably_map(&self) -> bool {
    match *self {
      RedisValue::Map(_) => true,
      RedisValue::Array(ref arr) => {
        if arr.len() % 2 == 0 {
          arr.chunks(2).fold(true, |b, chunk| b && !chunk[0].is_aggregate_type())
        } else {
          false
        }
      }
      _ => false,
    }
  }

  /// Whether or not the value is an array.
  pub fn is_array(&self) -> bool {
    match *self {
      RedisValue::Array(_) => true,
      _ => false,
    }
  }

  /// Read and return the inner value as a `u64`, if possible.
  pub fn as_u64(&self) -> Option<u64> {
    match self {
      RedisValue::Integer(ref i) => {
        if *i >= 0 {
          Some(*i as u64)
        } else {
          None
        }
      }
      RedisValue::String(ref s) => s.parse::<u64>().ok(),
      RedisValue::Array(ref inner) => {
        if inner.len() == 1 {
          inner.first().and_then(|v| v.as_u64())
        } else {
          None
        }
      }
      _ => None,
    }
  }

  ///  Read and return the inner value as a `i64`, if possible.
  pub fn as_i64(&self) -> Option<i64> {
    match self {
      RedisValue::Integer(ref i) => Some(*i),
      RedisValue::String(ref s) => s.parse::<i64>().ok(),
      RedisValue::Array(ref inner) => {
        if inner.len() == 1 {
          inner.first().and_then(|v| v.as_i64())
        } else {
          None
        }
      }
      _ => None,
    }
  }

  ///  Read and return the inner value as a `usize`, if possible.
  pub fn as_usize(&self) -> Option<usize> {
    match self {
      RedisValue::Integer(i) => {
        if *i >= 0 {
          Some(*i as usize)
        } else {
          None
        }
      }
      RedisValue::String(ref s) => s.parse::<usize>().ok(),
      RedisValue::Array(ref inner) => {
        if inner.len() == 1 {
          inner.first().and_then(|v| v.as_usize())
        } else {
          None
        }
      }
      _ => None,
    }
  }

  ///  Read and return the inner value as a `f64`, if possible.
  pub fn as_f64(&self) -> Option<f64> {
    match self {
      RedisValue::Double(ref f) => Some(*f),
      RedisValue::String(ref s) => utils::redis_string_to_f64(s).ok(),
      RedisValue::Integer(ref i) => Some(*i as f64),
      RedisValue::Array(ref inner) => {
        if inner.len() == 1 {
          inner.first().and_then(|v| v.as_f64())
        } else {
          None
        }
      }
      _ => None,
    }
  }

  /// Read and return the inner `String` if the value is a string or scalar value.
  pub fn into_string(self) -> Option<String> {
    match self {
      RedisValue::Boolean(b) => Some(b.to_string()),
      RedisValue::Double(f) => Some(f.to_string()),
      RedisValue::String(s) => Some(s.into_string()),
      RedisValue::Bytes(b) => String::from_utf8(b.to_vec()).ok(),
      RedisValue::Integer(i) => Some(i.to_string()),
      RedisValue::Queued => Some(QUEUED.to_owned()),
      RedisValue::Array(mut inner) => {
        if inner.len() == 1 {
          inner.pop().and_then(|v| v.into_string())
        } else {
          None
        }
      }
      _ => None,
    }
  }

  /// Read and return the inner data as a `Str` from the `bytes` crate.
  pub fn into_bytes_str(self) -> Option<Str> {
    match self {
      RedisValue::Boolean(b) => match b {
        true => Some(TRUE_STR.clone()),
        false => Some(FALSE_STR.clone()),
      },
      RedisValue::Double(f) => Some(f.to_string().into()),
      RedisValue::String(s) => Some(s),
      RedisValue::Bytes(b) => Str::from_inner(b).ok(),
      RedisValue::Integer(i) => Some(i.to_string().into()),
      RedisValue::Queued => Some(utils::static_str(QUEUED)),
      RedisValue::Array(mut inner) => {
        if inner.len() == 1 {
          inner.pop().and_then(|v| v.into_bytes_str())
        } else {
          None
        }
      }
      _ => None,
    }
  }

  /// Read and return the inner `String` if the value is a string or scalar value.
  ///
  /// Note: this will cast integers and doubles to strings.
  pub fn as_string(&self) -> Option<String> {
    match self {
      RedisValue::Boolean(ref b) => Some(b.to_string()),
      RedisValue::Double(ref f) => Some(f.to_string()),
      RedisValue::String(ref s) => Some(s.to_string()),
      RedisValue::Bytes(ref b) => str::from_utf8(b).ok().map(|s| s.to_owned()),
      RedisValue::Integer(ref i) => Some(i.to_string()),
      RedisValue::Queued => Some(QUEUED.to_owned()),
      _ => None,
    }
  }

  /// Read the inner value as a string slice.
  ///
  /// Null is returned as "nil" and scalar values are cast to a string.
  pub fn as_str(&'a self) -> Option<Cow<'a, str>> {
    let s = match *self {
      RedisValue::Double(ref f) => Cow::Owned(f.to_string()),
      RedisValue::Boolean(ref b) => Cow::Owned(b.to_string()),
      RedisValue::String(ref s) => Cow::Borrowed(s.as_str()),
      RedisValue::Integer(ref i) => Cow::Owned(i.to_string()),
      RedisValue::Null => Cow::Borrowed(NIL),
      RedisValue::Queued => Cow::Borrowed(QUEUED),
      RedisValue::Bytes(ref b) => return str::from_utf8(b).ok().map(|s| Cow::Borrowed(s)),
      _ => return None,
    };

    Some(s)
  }

  /// Read the inner value as a string, using `String::from_utf8_lossy` on byte slices.
  pub fn as_str_lossy(&self) -> Option<Cow<str>> {
    let s = match *self {
      RedisValue::Boolean(ref b) => Cow::Owned(b.to_string()),
      RedisValue::Double(ref f) => Cow::Owned(f.to_string()),
      RedisValue::String(ref s) => Cow::Borrowed(s.as_str()),
      RedisValue::Integer(ref i) => Cow::Owned(i.to_string()),
      RedisValue::Null => Cow::Borrowed(NIL),
      RedisValue::Queued => Cow::Borrowed(QUEUED),
      RedisValue::Bytes(ref b) => String::from_utf8_lossy(b),
      _ => return None,
    };

    Some(s)
  }

  /// Read the inner value as an array of bytes, if possible.
  pub fn as_bytes(&self) -> Option<&[u8]> {
    match *self {
      RedisValue::String(ref s) => Some(s.as_bytes()),
      RedisValue::Bytes(ref b) => Some(b),
      RedisValue::Queued => Some(QUEUED.as_bytes()),
      _ => None,
    }
  }

  /// Attempt to convert the value to a `bool`.
  pub fn as_bool(&self) -> Option<bool> {
    match *self {
      RedisValue::Boolean(b) => Some(b),
      RedisValue::Integer(ref i) => match *i {
        0 => Some(false),
        1 => Some(true),
        _ => None,
      },
      RedisValue::String(ref s) => match s.as_ref() {
        "true" | "TRUE" | "t" | "T" | "1" => Some(true),
        "false" | "FALSE" | "f" | "F" | "0" => Some(false),
        _ => None,
      },
      RedisValue::Null => Some(false),
      RedisValue::Array(ref inner) => {
        if inner.len() == 1 {
          inner.first().and_then(|v| v.as_bool())
        } else {
          None
        }
      }
      _ => None,
    }
  }

  /// Convert the value to an array of `(value, score)` tuples if the redis value is an array result from a sorted set command with scores.
  pub fn into_zset_result(self) -> Result<Vec<(RedisValue, f64)>, RedisError> {
    protocol_utils::value_to_zset_result(self)
  }

  /// Attempt to convert this value to a Redis map if it's an array with an even number of elements.
  pub fn into_map(self) -> Result<RedisMap, RedisError> {
    if let RedisValue::Map(map) = self {
      return Ok(map);
    }

    if let RedisValue::Array(mut values) = self {
      if values.len() % 2 != 0 {
        return Err(RedisError::new(
          RedisErrorKind::Unknown,
          "Expected an even number of elements.",
        ));
      }
      let mut inner = HashMap::with_capacity(values.len() / 2);
      while values.len() >= 2 {
        let value = values.pop().unwrap();
        let key: RedisKey = values.pop().unwrap().try_into()?;

        inner.insert(key, value);
      }

      Ok(RedisMap { inner })
    } else {
      Err(RedisError::new(RedisErrorKind::Unknown, "Expected array."))
    }
  }

  /// Convert the array value to a set, if possible.
  pub fn into_set(self) -> Result<HashSet<RedisValue>, RedisError> {
    if let RedisValue::Array(values) = self {
      let mut out = HashSet::with_capacity(values.len());

      for value in values.into_iter() {
        out.insert(value);
      }
      Ok(out)
    } else {
      Err(RedisError::new(RedisErrorKind::Unknown, "Expected array."))
    }
  }

  /// Convert this value to an array if it's an array or map.
  ///
  /// If the value is not an array or map this returns a single-element array containing the current value.
  pub fn into_array(self) -> Vec<RedisValue> {
    match self {
      RedisValue::Array(values) => values,
      RedisValue::Map(map) => {
        let mut out = Vec::with_capacity(map.len() * 2);

        for (key, value) in map.inner().into_iter() {
          out.push(key.into());
          out.push(value);
        }
        out
      }
      _ => vec![self],
    }
  }

  /// Convert the value to an array of bytes, if possible.
  pub fn into_owned_bytes(self) -> Option<Vec<u8>> {
    let v = match self {
      RedisValue::String(s) => s.into_bytes(),
      RedisValue::Bytes(b) => b.to_vec(),
      RedisValue::Null => NULL.as_bytes().to_vec(),
      RedisValue::Queued => QUEUED.as_bytes().to_vec(),
      RedisValue::Array(mut inner) => {
        if inner.len() == 1 {
          return inner.pop().and_then(|v| v.into_owned_bytes());
        } else {
          return None;
        }
      }
      RedisValue::Integer(i) => i.to_string().into_bytes(),
      _ => return None,
    };

    Some(v)
  }

  /// Convert the value into a `Bytes` view.
  pub fn into_bytes(self) -> Option<Bytes> {
    let v = match self {
      RedisValue::String(s) => s.inner().clone(),
      RedisValue::Bytes(b) => b,
      RedisValue::Null => Bytes::from_static(NULL.as_bytes()),
      RedisValue::Queued => Bytes::from_static(QUEUED.as_bytes()),
      RedisValue::Array(mut inner) => {
        if inner.len() == 1 {
          return inner.pop().and_then(|v| v.into_bytes());
        } else {
          return None;
        }
      }
      RedisValue::Integer(i) => i.to_string().into(),
      _ => return None,
    };

    Some(v)
  }

  /// Convert the value into a `GeoPosition`, if possible.
  ///
  /// Null values are returned as `None` to work more easily with the result of the `GEOPOS` command.
  pub fn as_geo_position(&self) -> Result<Option<GeoPosition>, RedisError> {
    utils::value_to_geo_pos(self)
  }

  /// Replace this value with `RedisValue::Null`, returning the original value.
  pub fn take(&mut self) -> RedisValue {
    mem::replace(self, RedisValue::Null)
  }

  /// Attempt to convert this value to any value that implements the [FromRedis](crate::types::FromRedis) trait.
  ///
  /// ```rust
  /// # use fred::types::RedisValue;
  /// # use std::collections::HashMap;
  /// let foo: usize = RedisValue::String("123".into()).convert()?;
  /// let foo: i64 = RedisValue::String("123".into()).convert()?;
  /// let foo: String = RedisValue::String("123".into()).convert()?;
  /// let foo: Vec<u8> = RedisValue::Bytes(vec![102, 111, 111].into()).convert()?;
  /// let foo: Vec<u8> = RedisValue::String("foo".into()).convert()?;
  /// let foo: Vec<String> = RedisValue::Array(vec!["a".into(), "b".into()]).convert()?;
  /// let foo: HashMap<String, u16> = RedisValue::Array(vec![
  ///   "a".into(), 1.into(),
  ///   "b".into(), 2.into()
  /// ])
  /// .convert()?;
  /// let foo: (String, i64) = RedisValue::Array(vec!["a".into(), 1.into()]).convert()?;
  /// let foo: Vec<(String, i64)> = RedisValue::Array(vec![
  ///   "a".into(), 1.into(),
  ///   "b".into(), 2.into()
  /// ])
  /// .convert()?;
  /// // ...
  /// ```
  pub fn convert<R>(self) -> Result<R, RedisError>
  where
    R: FromRedis,
  {
    R::from_value(self)
  }

  /// Whether or not the value can be hashed.
  ///
  /// Some use cases require using `RedisValue` types as keys in a `HashMap`, etc. Trying to do so with an aggregate type can panic,
  /// and this function can be used to more gracefully handle this situation.
  pub fn can_hash(&self) -> bool {
    match self.kind() {
      RedisValueKind::String
      | RedisValueKind::Boolean
      | RedisValueKind::Double
      | RedisValueKind::Integer
      | RedisValueKind::Bytes
      | RedisValueKind::Null
      | RedisValueKind::Array
      | RedisValueKind::Queued => true,
      _ => false,
    }
  }
}

impl Hash for RedisValue {
  fn hash<H: Hasher>(&self, state: &mut H) {
    // used to prevent collisions between different types
    let prefix = match self.kind() {
      RedisValueKind::Boolean => b'B',
      RedisValueKind::Double => b'd',
      RedisValueKind::Integer => b'i',
      RedisValueKind::String => b's',
      RedisValueKind::Null => b'n',
      RedisValueKind::Queued => b'q',
      RedisValueKind::Array => b'a',
      RedisValueKind::Map => b'm',
      RedisValueKind::Bytes => b'b',
    };
    prefix.hash(state);

    match *self {
      RedisValue::Boolean(b) => b.hash(state),
      RedisValue::Double(f) => f.to_be_bytes().hash(state),
      RedisValue::Integer(d) => d.hash(state),
      RedisValue::String(ref s) => s.hash(state),
      RedisValue::Bytes(ref b) => b.hash(state),
      RedisValue::Null => NULL.hash(state),
      RedisValue::Queued => QUEUED.hash(state),
      RedisValue::Array(ref arr) => {
        for value in arr.iter() {
          value.hash(state);
        }
      }
      _ => panic!("Cannot hash aggregate value."),
    }
  }
}

impl From<u8> for RedisValue {
  fn from(d: u8) -> Self {
    RedisValue::Integer(d as i64)
  }
}

impl From<u16> for RedisValue {
  fn from(d: u16) -> Self {
    RedisValue::Integer(d as i64)
  }
}

impl From<u32> for RedisValue {
  fn from(d: u32) -> Self {
    RedisValue::Integer(d as i64)
  }
}

impl From<i8> for RedisValue {
  fn from(d: i8) -> Self {
    RedisValue::Integer(d as i64)
  }
}

impl From<i16> for RedisValue {
  fn from(d: i16) -> Self {
    RedisValue::Integer(d as i64)
  }
}

impl From<i32> for RedisValue {
  fn from(d: i32) -> Self {
    RedisValue::Integer(d as i64)
  }
}

impl From<i64> for RedisValue {
  fn from(d: i64) -> Self {
    RedisValue::Integer(d)
  }
}

impl From<f32> for RedisValue {
  fn from(f: f32) -> Self {
    RedisValue::Double(f as f64)
  }
}

impl From<f64> for RedisValue {
  fn from(f: f64) -> Self {
    RedisValue::Double(f)
  }
}

impl TryFrom<u64> for RedisValue {
  type Error = RedisError;

  fn try_from(d: u64) -> Result<Self, Self::Error> {
    if d >= (i64::MAX as u64) {
      return Err(RedisError::new(RedisErrorKind::Unknown, "Unsigned integer too large."));
    }

    Ok((d as i64).into())
  }
}

impl TryFrom<u128> for RedisValue {
  type Error = RedisError;

  fn try_from(d: u128) -> Result<Self, Self::Error> {
    if d >= (i64::MAX as u128) {
      return Err(RedisError::new(RedisErrorKind::Unknown, "Unsigned integer too large."));
    }

    Ok((d as i64).into())
  }
}

impl TryFrom<i128> for RedisValue {
  type Error = RedisError;

  fn try_from(d: i128) -> Result<Self, Self::Error> {
    if d >= (i64::MAX as i128) {
      return Err(RedisError::new(RedisErrorKind::Unknown, "Signed integer too large."));
    }

    Ok((d as i64).into())
  }
}

impl TryFrom<usize> for RedisValue {
  type Error = RedisError;

  fn try_from(d: usize) -> Result<Self, Self::Error> {
    if d >= (i64::MAX as usize) {
      return Err(RedisError::new(RedisErrorKind::Unknown, "Unsigned integer too large."));
    }

    Ok((d as i64).into())
  }
}

impl From<Str> for RedisValue {
  fn from(s: Str) -> Self {
    RedisValue::String(s)
  }
}

impl From<Bytes> for RedisValue {
  fn from(b: Bytes) -> Self {
    RedisValue::Bytes(b)
  }
}

impl From<String> for RedisValue {
  fn from(d: String) -> Self {
    RedisValue::String(Str::from(d))
  }
}

impl<'a> From<&'a str> for RedisValue {
  fn from(d: &'a str) -> Self {
    RedisValue::String(Str::from(d))
  }
}

impl<'a> From<&'a [u8]> for RedisValue {
  fn from(b: &'a [u8]) -> Self {
    RedisValue::Bytes(Bytes::from(b.to_vec()))
  }
}

impl From<bool> for RedisValue {
  fn from(d: bool) -> Self {
    RedisValue::Boolean(d)
  }
}

impl<T> TryFrom<Option<T>> for RedisValue
where
  T: TryInto<RedisValue>,
  T::Error: Into<RedisError>,
{
  type Error = RedisError;

  fn try_from(d: Option<T>) -> Result<Self, Self::Error> {
    match d {
      Some(i) => to!(i),
      None => Ok(RedisValue::Null),
    }
  }
}

impl FromIterator<RedisValue> for RedisValue {
  fn from_iter<I: IntoIterator<Item = RedisValue>>(iter: I) -> Self {
    RedisValue::Array(iter.into_iter().collect())
  }
}

impl<K, V> TryFrom<HashMap<K, V>> for RedisValue
where
  K: TryInto<RedisKey>,
  K::Error: Into<RedisError>,
  V: TryInto<RedisValue>,
  V::Error: Into<RedisError>,
{
  type Error = RedisError;

  fn try_from(d: HashMap<K, V>) -> Result<Self, Self::Error> {
    Ok(RedisValue::Map(
      d.into_iter()
        .map(|(k, v)| Ok((k.try_into()?, v.try_into()?)))
        .collect()?,
    ))
  }
}

impl<K, V> TryFrom<BTreeMap<K, V>> for RedisValue
where
  K: TryInto<RedisKey>,
  K::Error: Into<RedisError>,
  V: TryInto<RedisValue>,
  V::Error: Into<RedisError>,
{
  type Error = RedisError;

  fn try_from(d: BTreeMap<K, V>) -> Result<Self, Self::Error> {
    Ok(RedisValue::Map(
      d.into_iter()
        .map(|(k, v)| Ok((k.try_into()?, v.try_into()?)))
        .collect()?,
    ))
  }
}

impl From<RedisKey> for RedisValue {
  fn from(d: RedisKey) -> Self {
    RedisValue::Bytes(d.key)
  }
}

impl From<RedisMap> for RedisValue {
  fn from(m: RedisMap) -> Self {
    RedisValue::Map(m)
  }
}

impl From<()> for RedisValue {
  fn from(_: ()) -> Self {
    RedisValue::Null
  }
}
