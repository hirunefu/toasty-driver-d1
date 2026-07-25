//! Conversions between toasty values and the JSON that crosses D1's HTTP API.

use toasty_core::stmt::{self, Value as CoreValue};

use crate::error::D1Error;

/// Largest magnitude D1's JSON transport carries without loss (2^53).
///
/// Beyond this the API answers with a JSON float: `i64::MAX` comes back as
/// 9223372036854776000 and the column's storage class flips to `real`, so the
/// value is silently corrupted. Binding rejects such values instead.
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_992;

fn safe_integer(v: i64) -> Result<serde_json::Value, D1Error> {
    if v.unsigned_abs() > MAX_SAFE_INTEGER as u64 {
        return Err(D1Error::new(format!(
            "integer {v} exceeds ±2^53, the range D1's JSON API carries without \
             loss of precision"
        )));
    }
    Ok(serde_json::json!(v))
}

/// Encodes a bind parameter for the JSON `params` array.
///
/// Blobs cross as JSON arrays of byte values; D1 binds such an array as a
/// real BLOB (`typeof()` reports `blob`) and returns it in the same shape.
pub(crate) fn param_to_json(value: &CoreValue) -> Result<serde_json::Value, D1Error> {
    Ok(match value {
        CoreValue::Null => serde_json::Value::Null,
        CoreValue::Bool(v) => serde_json::json!(if *v { 1 } else { 0 }),
        CoreValue::I8(v) => serde_json::json!(*v),
        CoreValue::I16(v) => serde_json::json!(*v),
        CoreValue::I32(v) => serde_json::json!(*v),
        CoreValue::I64(v) => safe_integer(*v)?,
        CoreValue::U8(v) => serde_json::json!(*v),
        CoreValue::U16(v) => serde_json::json!(*v),
        CoreValue::U32(v) => serde_json::json!(*v),
        // Wraps like the SQLite driver does — SQLite has no unsigned integer
        // type, and decoding casts the bit pattern back.
        CoreValue::U64(v) => safe_integer(*v as i64)?,
        CoreValue::F32(v) => serde_json::json!(*v),
        CoreValue::F64(v) => serde_json::json!(*v),
        CoreValue::String(v) => serde_json::json!(v),
        CoreValue::Bytes(v) => {
            serde_json::Value::Array(v.iter().map(|b| serde_json::json!(*b)).collect())
        }
        CoreValue::Uuid(v) => {
            serde_json::Value::Array(v.as_bytes().iter().map(|b| serde_json::json!(*b)).collect())
        }
        CoreValue::List(_) | CoreValue::Object(_) => {
            let text = toasty_sql::json::to_string(value)
                .map_err(|err| D1Error::new(format!("serialize document value: {err}")))?;
            serde_json::Value::String(text)
        }
        other => {
            return Err(D1Error::new(format!(
                "unsupported bind parameter for D1: {other:?}"
            )));
        }
    })
}

/// Decodes a result cell according to the type the query plan expects,
/// mirroring the sqlite driver's mapping of SQLite storage classes.
pub(crate) fn json_to_value(
    value: &serde_json::Value,
    ty: &stmt::Type,
) -> Result<CoreValue, D1Error> {
    use serde_json::Value as Json;

    Ok(match value {
        Json::Null => CoreValue::Null,
        Json::Number(n) if n.is_i64() || n.is_u64() => {
            let v = n
                .as_i64()
                .ok_or_else(|| D1Error::new(format!("integer out of i64 range: {n}")))?;
            match ty {
                stmt::Type::Bool => CoreValue::Bool(v != 0),
                stmt::Type::I8 => CoreValue::I8(v as i8),
                stmt::Type::I16 => CoreValue::I16(v as i16),
                stmt::Type::I32 => CoreValue::I32(v as i32),
                stmt::Type::I64 => CoreValue::I64(v),
                stmt::Type::U8 => CoreValue::U8(v as u8),
                stmt::Type::U16 => CoreValue::U16(v as u16),
                stmt::Type::U32 => CoreValue::U32(v as u32),
                stmt::Type::U64 => CoreValue::U64(v as u64),
                // SQLite stores floats that happen to be integral as
                // INTEGER; D1 then serializes them as JSON integers.
                stmt::Type::F32 => CoreValue::F32(v as f32),
                stmt::Type::F64 => CoreValue::F64(v as f64),
                _ => {
                    return Err(D1Error::new(format!("cannot decode integer into {ty:?}")));
                }
            }
        }
        Json::Number(n) => {
            let v = n
                .as_f64()
                .ok_or_else(|| D1Error::new(format!("non-finite number: {n}")))?;
            match ty {
                stmt::Type::F32 => CoreValue::F32(v as f32),
                stmt::Type::F64 => CoreValue::F64(v),
                // An integer column answering with a float means the stored
                // value left the ±2^53 range D1's JSON API can represent.
                _ => {
                    return Err(D1Error::new(format!(
                        "cannot decode {n} into {ty:?}: values beyond ±2^53 come \
                         back from D1 as floats and have already lost precision"
                    )));
                }
            }
        }
        Json::String(text) => match ty {
            stmt::Type::Uuid => CoreValue::Uuid(
                text.parse()
                    .map_err(|err| D1Error::new(format!("invalid uuid: {err}")))?,
            ),
            stmt::Type::List(elem) => toasty_sql::json::list_from_str(text, elem)
                .map_err(|err| D1Error::new(format!("decode collection column: {err}")))?,
            stmt::Type::Object => toasty_sql::json::from_str(text, ty)
                .map_err(|err| D1Error::new(format!("decode document column: {err}")))?,
            _ => CoreValue::String(text.clone()),
        },
        Json::Bool(v) => match ty {
            stmt::Type::Bool => CoreValue::Bool(*v),
            _ => return Err(D1Error::new(format!("cannot decode bool into {ty:?}"))),
        },
        // D1 returns BLOB columns as arrays of byte values.
        Json::Array(items) => {
            let bytes = json_array_to_bytes(items)?;
            match ty {
                stmt::Type::Bytes => CoreValue::Bytes(bytes),
                stmt::Type::Uuid => CoreValue::Uuid(
                    uuid::Uuid::from_slice(&bytes)
                        .map_err(|err| D1Error::new(format!("invalid uuid blob: {err}")))?,
                ),
                _ => return Err(D1Error::new(format!("cannot decode blob into {ty:?}"))),
            }
        }
        Json::Object(_) => {
            return Err(D1Error::new(format!(
                "unexpected JSON object in result cell: {value}"
            )));
        }
    })
}

fn json_array_to_bytes(items: &[serde_json::Value]) -> Result<Vec<u8>, D1Error> {
    items
        .iter()
        .map(|item| {
            item.as_u64()
                .and_then(|n| u8::try_from(n).ok())
                .ok_or_else(|| D1Error::new(format!("blob element is not a byte: {item}")))
        })
        .collect()
}

/// Decodes a result cell when the plan did not declare types (`RawSqlRet::Infer`).
pub(crate) fn json_to_value_infer(value: &serde_json::Value) -> CoreValue {
    use serde_json::Value as Json;

    match value {
        Json::Null => CoreValue::Null,
        Json::Number(n) if n.is_i64() => CoreValue::I64(n.as_i64().unwrap()),
        Json::Number(n) => CoreValue::F64(n.as_f64().unwrap_or(f64::NAN)),
        Json::String(text) => CoreValue::String(text.clone()),
        Json::Array(items) => json_array_to_bytes(items)
            .map(CoreValue::Bytes)
            .unwrap_or_else(|_| CoreValue::String(value.to_string())),
        Json::Bool(v) => CoreValue::Bool(*v),
        Json::Object(_) => CoreValue::String(value.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_encode_scalars() {
        assert_eq!(
            param_to_json(&CoreValue::String("hi".into())).unwrap(),
            serde_json::json!("hi")
        );
        assert_eq!(
            param_to_json(&CoreValue::I64(42)).unwrap(),
            serde_json::json!(42)
        );
        assert_eq!(
            param_to_json(&CoreValue::U64(7)).unwrap(),
            serde_json::json!(7)
        );
        assert_eq!(
            param_to_json(&CoreValue::Bool(true)).unwrap(),
            serde_json::json!(1)
        );
        assert_eq!(
            param_to_json(&CoreValue::Bool(false)).unwrap(),
            serde_json::json!(0)
        );
        assert_eq!(
            param_to_json(&CoreValue::Null).unwrap(),
            serde_json::Value::Null
        );
    }

    #[test]
    fn params_reject_integers_beyond_the_safe_range() {
        assert!(param_to_json(&CoreValue::I64(MAX_SAFE_INTEGER)).is_ok());
        assert!(param_to_json(&CoreValue::I64(-MAX_SAFE_INTEGER)).is_ok());

        let err = param_to_json(&CoreValue::I64(i64::MAX)).unwrap_err();
        assert!(err.to_string().contains("2^53"), "message was: {err}");
        assert!(param_to_json(&CoreValue::I64(MAX_SAFE_INTEGER + 1)).is_err());

        // u64 wraps into i64 before the range check, so the top of the range
        // round-trips through the bit pattern while the middle does not.
        assert!(param_to_json(&CoreValue::U64(u64::MAX)).is_ok());
        assert!(param_to_json(&CoreValue::U64(1 << 60)).is_err());
    }

    #[test]
    fn blobs_round_trip_as_byte_arrays() {
        let bytes = CoreValue::Bytes(vec![1, 2, 255]);
        let encoded = param_to_json(&bytes).unwrap();
        assert_eq!(encoded, serde_json::json!([1, 2, 255]));
        assert_eq!(json_to_value(&encoded, &stmt::Type::Bytes).unwrap(), bytes);
    }

    #[test]
    fn uuids_round_trip_as_blobs() {
        let uuid = uuid::Uuid::parse_str("67e55044-10b1-426f-9247-bb680e5fe0c8").unwrap();
        let encoded = param_to_json(&CoreValue::Uuid(uuid)).unwrap();
        assert_eq!(encoded.as_array().unwrap().len(), 16);
        assert_eq!(
            json_to_value(&encoded, &stmt::Type::Uuid).unwrap(),
            CoreValue::Uuid(uuid)
        );
    }

    #[test]
    fn typed_decode_maps_sqlite_storage_classes() {
        assert_eq!(
            json_to_value(&serde_json::json!(1), &stmt::Type::Bool).unwrap(),
            CoreValue::Bool(true)
        );
        assert_eq!(
            json_to_value(&serde_json::json!(0), &stmt::Type::Bool).unwrap(),
            CoreValue::Bool(false)
        );
        assert_eq!(
            json_to_value(&serde_json::json!(42), &stmt::Type::U64).unwrap(),
            CoreValue::U64(42)
        );
        assert_eq!(
            json_to_value(&serde_json::json!("title"), &stmt::Type::String).unwrap(),
            CoreValue::String("title".into())
        );
        assert_eq!(
            json_to_value(&serde_json::Value::Null, &stmt::Type::String).unwrap(),
            CoreValue::Null
        );
        // Integral floats come back as JSON integers.
        assert_eq!(
            json_to_value(&serde_json::json!(3), &stmt::Type::F64).unwrap(),
            CoreValue::F64(3.0)
        );
    }

    #[test]
    fn infer_decode_uses_json_shape() {
        assert_eq!(
            json_to_value_infer(&serde_json::json!(5)),
            CoreValue::I64(5)
        );
        assert_eq!(
            json_to_value_infer(&serde_json::json!(2.5)),
            CoreValue::F64(2.5)
        );
        assert_eq!(
            json_to_value_infer(&serde_json::json!("x")),
            CoreValue::String("x".into())
        );
        assert_eq!(
            json_to_value_infer(&serde_json::Value::Null),
            CoreValue::Null
        );
    }
}
