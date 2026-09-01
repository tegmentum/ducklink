//! ADR-0029 Phase 6.2.i — shared value ↔ wit-bindgen-type
//! marshalling for the guest-export migration.
//!
//! Every `ExtensionInstance::dispatch_*` / `pub fn xxx` method
//! migrated off wit-bindgen typed dispatchers to
//! `wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export`
//! constructs a `Vec<Value>` from its typed args + decodes the
//! returned `Vec<Value>` back into typed values. This module houses
//! every reusable converter — Duckvalue, Duckerror, common lifters
//! — so each migrated callsite stays a few lines of glue.

use crate::duckdb_extension_bindings::duckdb::extension::types as extension_types;
use wasmos_runtime_api::Value;

// ─── Duckerror ─────────────────────────────────────────────────────

pub(crate) fn duckerror_from_value(v: &Value) -> crate::extension::Duckerror {
    let (disc, payload) = match v {
        Value::Variant { discriminant, payload } => (discriminant, payload),
        other => {
            return crate::extension::Duckerror::Internal(format!(
                "export_marshal: expected Variant for duckerror, got {other:?}"
            ));
        }
    };
    let msg = match payload.as_deref() {
        Some(Value::String(s)) => s.clone(),
        Some(other) => {
            return crate::extension::Duckerror::Internal(format!(
                "export_marshal: expected String payload for duckerror.{disc}, got {other:?}"
            ));
        }
        None => String::new(),
    };
    match disc.as_str() {
        "invalidargument" => crate::extension::Duckerror::Invalidargument(msg),
        "unsupported" => crate::extension::Duckerror::Unsupported(msg),
        "invalidstate" => crate::extension::Duckerror::Invalidstate(msg),
        "io" => crate::extension::Duckerror::Io(msg),
        "internal" => crate::extension::Duckerror::Internal(msg),
        other => crate::extension::Duckerror::Internal(format!(
            "export_marshal: unknown duckerror discriminant {other:?}: {msg}"
        )),
    }
}

pub(crate) fn string_from_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => format!("export_marshal: expected string, got {other:?}"),
    }
}

pub(crate) fn export_result_to_duckerror<T>(
    out: Vec<Value>,
    method: &str,
    lift_ok: impl FnOnce(Option<&Value>) -> Result<T, crate::extension::Duckerror>,
) -> Result<T, crate::extension::Duckerror> {
    if out.len() != 1 {
        return Err(crate::extension::Duckerror::Internal(format!(
            "{method:?} returned {} values, expected 1",
            out.len()
        )));
    }
    match &out[0] {
        Value::Result(Ok(payload)) => lift_ok(payload.as_deref()),
        Value::Result(Err(payload)) => {
            let e = payload.as_deref().map_or_else(
                || crate::extension::Duckerror::Internal(format!("{method}: Err(None)")),
                duckerror_from_value,
            );
            Err(e)
        }
        other => Err(crate::extension::Duckerror::Internal(format!(
            "{method:?}: expected Result, got {other:?}"
        ))),
    }
}

pub(crate) fn export_result_to_string<T>(
    out: Vec<Value>,
    method: &str,
    lift_ok: impl FnOnce(Option<&Value>) -> Result<T, String>,
) -> Result<T, crate::extension::Duckerror> {
    if out.len() != 1 {
        return Err(crate::extension::Duckerror::Internal(format!(
            "{method:?} returned {} values, expected 1",
            out.len()
        )));
    }
    match &out[0] {
        Value::Result(Ok(payload)) => {
            lift_ok(payload.as_deref()).map_err(crate::extension::Duckerror::Io)
        }
        Value::Result(Err(payload)) => {
            let s = payload.as_deref().map_or_else(String::new, string_from_value);
            Err(crate::extension::Duckerror::Io(s))
        }
        other => Err(crate::extension::Duckerror::Internal(format!(
            "{method:?}: expected Result, got {other:?}"
        ))),
    }
}

// ─── Common lifters ────────────────────────────────────────────────

pub(crate) fn lift_bool(payload: Option<&Value>) -> Result<bool, crate::extension::Duckerror> {
    match payload {
        Some(Value::Bool(b)) => Ok(*b),
        Some(other) => Err(crate::extension::Duckerror::Internal(format!(
            "expected Ok(Bool), got {other:?}"
        ))),
        None => Err(crate::extension::Duckerror::Internal("expected Ok(Bool), got None".into())),
    }
}

pub(crate) fn lift_u32(payload: Option<&Value>) -> Result<u32, crate::extension::Duckerror> {
    match payload {
        Some(Value::U32(n)) => Ok(*n),
        Some(other) => Err(crate::extension::Duckerror::Internal(format!(
            "expected Ok(U32), got {other:?}"
        ))),
        None => Err(crate::extension::Duckerror::Internal("expected Ok(U32), got None".into())),
    }
}

pub(crate) fn lift_u64(payload: Option<&Value>) -> Result<u64, crate::extension::Duckerror> {
    match payload {
        Some(Value::U64(n)) => Ok(*n),
        Some(other) => Err(crate::extension::Duckerror::Internal(format!(
            "expected Ok(U64), got {other:?}"
        ))),
        None => Err(crate::extension::Duckerror::Internal("expected Ok(U64), got None".into())),
    }
}

pub(crate) fn lift_bytes(payload: Option<&Value>) -> Result<Vec<u8>, crate::extension::Duckerror> {
    match payload {
        Some(Value::Bytes(b)) => Ok(b.to_vec()),
        Some(Value::List(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                match item {
                    Value::U8(b) => out.push(*b),
                    other => {
                        return Err(crate::extension::Duckerror::Internal(format!(
                            "list<u8>[{i}]: expected U8, got {other:?}"
                        )));
                    }
                }
            }
            Ok(out)
        }
        Some(other) => Err(crate::extension::Duckerror::Internal(format!(
            "expected Ok(list<u8>), got {other:?}"
        ))),
        None => Err(crate::extension::Duckerror::Internal("expected Ok(list<u8>), got None".into())),
    }
}

pub(crate) fn lift_string_list(
    payload: Option<&Value>,
) -> Result<Vec<String>, crate::extension::Duckerror> {
    match payload {
        Some(Value::List(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                match item {
                    Value::String(s) => out.push(s.clone()),
                    other => {
                        return Err(crate::extension::Duckerror::Internal(format!(
                            "list<string>[{i}]: expected String, got {other:?}"
                        )));
                    }
                }
            }
            Ok(out)
        }
        Some(other) => Err(crate::extension::Duckerror::Internal(format!(
            "expected Ok(list<string>), got {other:?}"
        ))),
        None => Err(crate::extension::Duckerror::Internal(
            "expected Ok(list<string>), got None".into(),
        )),
    }
}

pub(crate) fn lift_s64_list(
    payload: Option<&Value>,
) -> Result<Vec<i64>, crate::extension::Duckerror> {
    match payload {
        Some(Value::List(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                match item {
                    Value::S64(n) => out.push(*n),
                    other => {
                        return Err(crate::extension::Duckerror::Internal(format!(
                            "list<s64>[{i}]: expected S64, got {other:?}"
                        )));
                    }
                }
            }
            Ok(out)
        }
        Some(other) => Err(crate::extension::Duckerror::Internal(format!(
            "expected Ok(list<s64>), got {other:?}"
        ))),
        None => Err(crate::extension::Duckerror::Internal("expected Ok(list<s64>), got None".into())),
    }
}

// ─── Lowerers ──────────────────────────────────────────────────────

/// list<u8> — lower via Value::List of Value::U8 (avoids needing
/// `bytes` crate dep on the consumer side; Value::Bytes is a
/// fast-path alias that the bridge marshaller flattens the same way).
pub(crate) fn bytes_to_value(b: &[u8]) -> Value {
    Value::List(b.iter().map(|byte| Value::U8(*byte)).collect())
}

pub(crate) fn s64_list_to_value(xs: &[i64]) -> Value {
    Value::List(xs.iter().map(|n| Value::S64(*n)).collect())
}

pub(crate) fn f32_matrix_to_value(m: &[Vec<f32>]) -> Value {
    Value::List(
        m.iter()
            .map(|row| Value::List(row.iter().map(|f| Value::F32(*f)).collect()))
            .collect(),
    )
}

pub(crate) fn f32_list_to_value(xs: &[f32]) -> Value {
    Value::List(xs.iter().map(|f| Value::F32(*f)).collect())
}

// ─── Duckvalue: 25 arms ────────────────────────────────────────────

pub(crate) fn duckvalue_to_value(v: &crate::extension::Duckvalue) -> Value {
    use crate::extension::Duckvalue as D;
    let (disc, payload): (&str, Option<Value>) = match v {
        D::Null => ("null", None),
        D::Boolean(b) => ("boolean", Some(Value::Bool(*b))),
        D::Int64(n) => ("int64", Some(Value::S64(*n))),
        D::Uint64(n) => ("uint64", Some(Value::U64(*n))),
        D::Float64(f) => ("float64", Some(Value::F64(*f))),
        D::Text(s) => ("text", Some(Value::String(s.clone()))),
        D::Blob(b) => ("blob", Some(bytes_to_value(b))),
        D::Int32(n) => ("int32", Some(Value::S32(*n))),
        D::Timestamp(n) => ("timestamp", Some(Value::S64(*n))),
        D::Int8(n) => ("int8", Some(Value::S8(*n))),
        D::Int16(n) => ("int16", Some(Value::S16(*n))),
        D::Uint8(n) => ("uint8", Some(Value::U8(*n))),
        D::Uint16(n) => ("uint16", Some(Value::U16(*n))),
        D::Uint32(n) => ("uint32", Some(Value::U32(*n))),
        D::Float32(f) => ("float32", Some(Value::F32(*f))),
        D::Date(n) => ("date", Some(Value::S32(*n))),
        D::Time(n) => ("time", Some(Value::S64(*n))),
        D::Timestamptz(n) => ("timestamptz", Some(Value::S64(*n))),
        D::Decimal(d) => (
            "decimal",
            Some(Value::Record(vec![
                ("lower".into(), Value::U64(d.lower)),
                ("upper".into(), Value::U64(d.upper)),
                ("width".into(), Value::U8(d.width)),
                ("scale".into(), Value::U8(d.scale)),
            ])),
        ),
        D::Interval(i) => (
            "interval",
            Some(Value::Record(vec![
                ("months".into(), Value::S32(i.months)),
                ("days".into(), Value::S32(i.days)),
                ("micros".into(), Value::S64(i.micros)),
            ])),
        ),
        D::Uuid(u) => (
            "uuid",
            Some(Value::Record(vec![
                ("hi".into(), Value::U64(u.hi)),
                ("lo".into(), Value::U64(u.lo)),
            ])),
        ),
        D::Hugeint(h) => (
            "hugeint",
            Some(Value::Record(vec![
                ("lower".into(), Value::U64(h.lower)),
                ("upper".into(), Value::S64(h.upper)),
            ])),
        ),
        D::Uhugeint(h) => (
            "uhugeint",
            Some(Value::Record(vec![
                ("lower".into(), Value::U64(h.lower)),
                ("upper".into(), Value::U64(h.upper)),
            ])),
        ),
        D::Complex(c) => (
            "complex",
            Some(Value::Record(vec![
                ("type-expr".into(), Value::String(c.type_expr.clone())),
                ("json".into(), Value::String(c.json.clone())),
            ])),
        ),
    };
    Value::Variant { discriminant: disc.into(), payload: payload.map(Box::new) }
}

pub(crate) fn value_to_duckvalue(
    v: &Value,
) -> Result<crate::extension::Duckvalue, crate::extension::Duckerror> {
    use crate::extension::Duckvalue as D;
    let (disc, payload) = match v {
        Value::Variant { discriminant, payload } => (discriminant, payload),
        other => {
            return Err(crate::extension::Duckerror::Internal(format!(
                "value_to_duckvalue: expected Variant, got {other:?}"
            )));
        }
    };
    let need = |want: &str| -> Result<&Value, crate::extension::Duckerror> {
        payload.as_deref().ok_or_else(|| {
            crate::extension::Duckerror::Internal(format!(
                "duckvalue.{disc}: expected {want}, got None"
            ))
        })
    };
    Ok(match disc.as_str() {
        "null" => D::Null,
        "boolean" => match need("Bool")? {
            Value::Bool(b) => D::Boolean(*b),
            o => return Err(shape_err(disc, "Bool", o)),
        },
        "int64" => match need("S64")? {
            Value::S64(n) => D::Int64(*n),
            o => return Err(shape_err(disc, "S64", o)),
        },
        "uint64" => match need("U64")? {
            Value::U64(n) => D::Uint64(*n),
            o => return Err(shape_err(disc, "U64", o)),
        },
        "float64" => match need("F64")? {
            Value::F64(f) => D::Float64(*f),
            o => return Err(shape_err(disc, "F64", o)),
        },
        "text" => match need("String")? {
            Value::String(s) => D::Text(s.clone()),
            o => return Err(shape_err(disc, "String", o)),
        },
        "blob" => match need("Bytes")? {
            Value::Bytes(b) => D::Blob(b.to_vec()),
            Value::List(items) => {
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    match it {
                        Value::U8(b) => out.push(*b),
                        o => return Err(shape_err(disc, "U8 in blob list", o)),
                    }
                }
                D::Blob(out)
            }
            o => return Err(shape_err(disc, "Bytes or List<U8>", o)),
        },
        "int32" => match need("S32")? {
            Value::S32(n) => D::Int32(*n),
            o => return Err(shape_err(disc, "S32", o)),
        },
        "timestamp" => match need("S64")? {
            Value::S64(n) => D::Timestamp(*n),
            o => return Err(shape_err(disc, "S64", o)),
        },
        "int8" => match need("S8")? {
            Value::S8(n) => D::Int8(*n),
            o => return Err(shape_err(disc, "S8", o)),
        },
        "int16" => match need("S16")? {
            Value::S16(n) => D::Int16(*n),
            o => return Err(shape_err(disc, "S16", o)),
        },
        "uint8" => match need("U8")? {
            Value::U8(n) => D::Uint8(*n),
            o => return Err(shape_err(disc, "U8", o)),
        },
        "uint16" => match need("U16")? {
            Value::U16(n) => D::Uint16(*n),
            o => return Err(shape_err(disc, "U16", o)),
        },
        "uint32" => match need("U32")? {
            Value::U32(n) => D::Uint32(*n),
            o => return Err(shape_err(disc, "U32", o)),
        },
        "float32" => match need("F32")? {
            Value::F32(f) => D::Float32(*f),
            o => return Err(shape_err(disc, "F32", o)),
        },
        "date" => match need("S32")? {
            Value::S32(n) => D::Date(*n),
            o => return Err(shape_err(disc, "S32", o)),
        },
        "time" => match need("S64")? {
            Value::S64(n) => D::Time(*n),
            o => return Err(shape_err(disc, "S64", o)),
        },
        "timestamptz" => match need("S64")? {
            Value::S64(n) => D::Timestamptz(*n),
            o => return Err(shape_err(disc, "S64", o)),
        },
        "decimal" => {
            let rec = need("Record")?;
            D::Decimal(crate::extension::Decimalvalue {
                lower: u64_field(rec, "lower")?,
                upper: u64_field(rec, "upper")?,
                width: u8_field(rec, "width")?,
                scale: u8_field(rec, "scale")?,
            })
        }
        "interval" => {
            let rec = need("Record")?;
            D::Interval(crate::extension::Intervalvalue {
                months: s32_field(rec, "months")?,
                days: s32_field(rec, "days")?,
                micros: s64_field(rec, "micros")?,
            })
        }
        "uuid" => {
            let rec = need("Record")?;
            D::Uuid(crate::extension::Uuidvalue {
                hi: u64_field(rec, "hi")?,
                lo: u64_field(rec, "lo")?,
            })
        }
        "hugeint" => {
            let rec = need("Record")?;
            D::Hugeint(crate::extension::Hugeintvalue {
                lower: u64_field(rec, "lower")?,
                upper: s64_field(rec, "upper")?,
            })
        }
        "uhugeint" => {
            let rec = need("Record")?;
            D::Uhugeint(crate::extension::Uhugeintvalue {
                lower: u64_field(rec, "lower")?,
                upper: u64_field(rec, "upper")?,
            })
        }
        "complex" => {
            let rec = need("Record")?;
            D::Complex(crate::extension::Complexvalue {
                type_expr: string_field(rec, "type-expr")?,
                json: string_field(rec, "json")?,
            })
        }
        other => {
            return Err(crate::extension::Duckerror::Internal(format!(
                "value_to_duckvalue: unknown discriminant {other:?}"
            )));
        }
    })
}

fn shape_err(disc: &str, want: &str, got: &Value) -> crate::extension::Duckerror {
    crate::extension::Duckerror::Internal(format!(
        "duckvalue.{disc}: expected {want}, got {got:?}"
    ))
}

// ─── Duckvalue collections ─────────────────────────────────────────

pub(crate) fn duckvalue_list_to_value(xs: &[crate::extension::Duckvalue]) -> Value {
    Value::List(xs.iter().map(duckvalue_to_value).collect())
}

pub(crate) fn value_to_duckvalue_list(
    v: &Value,
) -> Result<Vec<crate::extension::Duckvalue>, crate::extension::Duckerror> {
    match v {
        Value::List(items) => items.iter().map(value_to_duckvalue).collect(),
        other => Err(crate::extension::Duckerror::Internal(format!(
            "expected list<duckvalue>, got {other:?}"
        ))),
    }
}

pub(crate) fn value_to_resultset(
    v: &Value,
) -> Result<Vec<Vec<crate::extension::Duckvalue>>, crate::extension::Duckerror> {
    match v {
        Value::List(rows) => rows.iter().map(value_to_duckvalue_list).collect(),
        other => Err(crate::extension::Duckerror::Internal(format!(
            "expected resultset (list<list<duckvalue>>), got {other:?}"
        ))),
    }
}

pub(crate) fn value_to_optional_duckvalue(
    v: &Value,
) -> Result<Option<crate::extension::Duckvalue>, crate::extension::Duckerror> {
    match v {
        Value::Option(None) => Ok(None),
        Value::Option(Some(inner)) => Ok(Some(value_to_duckvalue(inner)?)),
        other => Err(crate::extension::Duckerror::Internal(format!(
            "expected option<duckvalue>, got {other:?}"
        ))),
    }
}

// ─── Record field extractors ───────────────────────────────────────

pub(crate) fn record_field<'a>(
    v: &'a Value,
    name: &str,
) -> Result<&'a Value, crate::extension::Duckerror> {
    match v {
        Value::Record(fields) => fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, val)| val)
            .ok_or_else(|| {
                crate::extension::Duckerror::Internal(format!("record missing field {name:?}"))
            }),
        other => Err(crate::extension::Duckerror::Internal(format!(
            "expected Record, got {other:?}"
        ))),
    }
}

pub(crate) fn u32_field(rec: &Value, name: &str) -> Result<u32, crate::extension::Duckerror> {
    match record_field(rec, name)? {
        Value::U32(n) => Ok(*n),
        o => Err(crate::extension::Duckerror::Internal(format!(
            "field {name:?}: expected U32, got {o:?}"
        ))),
    }
}
pub(crate) fn u64_field(rec: &Value, name: &str) -> Result<u64, crate::extension::Duckerror> {
    match record_field(rec, name)? {
        Value::U64(n) => Ok(*n),
        o => Err(crate::extension::Duckerror::Internal(format!(
            "field {name:?}: expected U64, got {o:?}"
        ))),
    }
}
pub(crate) fn s32_field(rec: &Value, name: &str) -> Result<i32, crate::extension::Duckerror> {
    match record_field(rec, name)? {
        Value::S32(n) => Ok(*n),
        o => Err(crate::extension::Duckerror::Internal(format!(
            "field {name:?}: expected S32, got {o:?}"
        ))),
    }
}
pub(crate) fn s64_field(rec: &Value, name: &str) -> Result<i64, crate::extension::Duckerror> {
    match record_field(rec, name)? {
        Value::S64(n) => Ok(*n),
        o => Err(crate::extension::Duckerror::Internal(format!(
            "field {name:?}: expected S64, got {o:?}"
        ))),
    }
}
pub(crate) fn u8_field(rec: &Value, name: &str) -> Result<u8, crate::extension::Duckerror> {
    match record_field(rec, name)? {
        Value::U8(n) => Ok(*n),
        o => Err(crate::extension::Duckerror::Internal(format!(
            "field {name:?}: expected U8, got {o:?}"
        ))),
    }
}
pub(crate) fn bool_field(rec: &Value, name: &str) -> Result<bool, crate::extension::Duckerror> {
    match record_field(rec, name)? {
        Value::Bool(b) => Ok(*b),
        o => Err(crate::extension::Duckerror::Internal(format!(
            "field {name:?}: expected Bool, got {o:?}"
        ))),
    }
}
pub(crate) fn string_field(rec: &Value, name: &str) -> Result<String, crate::extension::Duckerror> {
    match record_field(rec, name)? {
        Value::String(s) => Ok(s.clone()),
        o => Err(crate::extension::Duckerror::Internal(format!(
            "field {name:?}: expected String, got {o:?}"
        ))),
    }
}

// ─── Logicaltype (25 arms) + columndef ─────────────────────────────

pub(crate) fn logicaltype_to_value(v: &extension_types::Logicaltype) -> Value {
    use extension_types::Logicaltype as L;
    let (disc, payload): (&str, Option<Value>) = match v {
        L::Boolean => ("boolean", None),
        L::Int64 => ("int64", None),
        L::Uint64 => ("uint64", None),
        L::Float64 => ("float64", None),
        L::Text => ("text", None),
        L::Blob => ("blob", None),
        L::Int32 => ("int32", None),
        L::Timestamp => ("timestamp", None),
        L::Int8 => ("int8", None),
        L::Int16 => ("int16", None),
        L::Uint8 => ("uint8", None),
        L::Uint16 => ("uint16", None),
        L::Uint32 => ("uint32", None),
        L::Float32 => ("float32", None),
        L::Date => ("date", None),
        L::Time => ("time", None),
        L::Timestamptz => ("timestamptz", None),
        L::Decimal(d) => (
            "decimal",
            Some(Value::Record(vec![
                ("width".into(), Value::U8(d.width)),
                ("scale".into(), Value::U8(d.scale)),
            ])),
        ),
        L::Interval => ("interval", None),
        L::Uuid => ("uuid", None),
        L::Hugeint => ("hugeint", None),
        L::Uhugeint => ("uhugeint", None),
        L::Complex(s) => ("complex", Some(Value::String(s.clone()))),
    };
    Value::Variant { discriminant: disc.into(), payload: payload.map(Box::new) }
}

pub(crate) fn value_to_logicaltype(
    v: &Value,
) -> Result<extension_types::Logicaltype, crate::extension::Duckerror> {
    use extension_types::Logicaltype as L;
    let (disc, payload) = match v {
        Value::Variant { discriminant, payload } => (discriminant, payload),
        other => {
            return Err(crate::extension::Duckerror::Internal(format!(
                "value_to_logicaltype: expected Variant, got {other:?}"
            )));
        }
    };
    Ok(match disc.as_str() {
        "boolean" => L::Boolean,
        "int64" => L::Int64,
        "uint64" => L::Uint64,
        "float64" => L::Float64,
        "text" => L::Text,
        "blob" => L::Blob,
        "int32" => L::Int32,
        "timestamp" => L::Timestamp,
        "int8" => L::Int8,
        "int16" => L::Int16,
        "uint8" => L::Uint8,
        "uint16" => L::Uint16,
        "uint32" => L::Uint32,
        "float32" => L::Float32,
        "date" => L::Date,
        "time" => L::Time,
        "timestamptz" => L::Timestamptz,
        "decimal" => {
            let rec = payload.as_deref().ok_or_else(|| {
                crate::extension::Duckerror::Internal("logicaltype.decimal: missing payload".into())
            })?;
            L::Decimal(extension_types::Decimalshape {
                width: u8_field(rec, "width")?,
                scale: u8_field(rec, "scale")?,
            })
        }
        "interval" => L::Interval,
        "uuid" => L::Uuid,
        "hugeint" => L::Hugeint,
        "uhugeint" => L::Uhugeint,
        "complex" => match payload.as_deref() {
            Some(Value::String(s)) => L::Complex(s.clone()),
            other => return Err(crate::extension::Duckerror::Internal(format!(
                "logicaltype.complex: expected String, got {other:?}"
            ))),
        },
        other => {
            return Err(crate::extension::Duckerror::Internal(format!(
                "value_to_logicaltype: unknown discriminant {other:?}"
            )));
        }
    })
}

pub(crate) fn columndef_to_value(c: &extension_types::Columndef) -> Value {
    Value::Record(vec![
        ("name".into(), Value::String(c.name.clone())),
        ("logical".into(), logicaltype_to_value(&c.logical)),
    ])
}

pub(crate) fn value_to_columndef(
    v: &Value,
) -> Result<extension_types::Columndef, crate::extension::Duckerror> {
    let name = string_field(v, "name")?;
    let logical = value_to_logicaltype(record_field(v, "logical")?)?;
    Ok(extension_types::Columndef { name, logical })
}

pub(crate) fn columndef_list_to_value(cs: &[extension_types::Columndef]) -> Value {
    Value::List(cs.iter().map(columndef_to_value).collect())
}

pub(crate) fn value_to_columndef_list(
    v: &Value,
) -> Result<Vec<extension_types::Columndef>, crate::extension::Duckerror> {
    match v {
        Value::List(items) => items.iter().map(value_to_columndef).collect(),
        other => Err(crate::extension::Duckerror::Internal(format!(
            "expected list<columndef>, got {other:?}"
        ))),
    }
}

// ─── scan-request / scan-filter / compare-op ───────────────────────
//
// Phase 6.2.j — the top-level `scan_request_to_value` /
// `scan_filter_to_value` / `compare_op_to_value` helpers were removed
// as dead: the sole caller (`storage_scan_open`) marshals inline
// against the bindings-per-module `storage_scan::ScanRequest` (a
// distinct Rust type from the main `extension_storage::ScanRequest`),
// so a shared helper here would still need cross-type conversion at
// the callsite. Restore them if a second caller ever needs the main-
// bindings shape.

// ─── Colvec (Column has 27 arms) ──────────────────────────────────

use crate::duckdb_extension_bindings::duckdb::extension::column_types as extension_column_types;

pub(crate) fn colvec_to_value(cv: &extension_column_types::Colvec) -> Value {
    Value::Record(vec![
        ("data".into(), column_to_value(&cv.data)),
        ("validity".into(), bytes_to_value(&cv.validity)),
        ("rows".into(), Value::U32(cv.rows)),
    ])
}

pub(crate) fn value_to_colvec(
    v: &Value,
) -> Result<extension_column_types::Colvec, crate::extension::Duckerror> {
    let data = value_to_column(record_field(v, "data")?)?;
    let validity = match record_field(v, "validity")? {
        Value::Bytes(b) => b.to_vec(),
        Value::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                match it {
                    Value::U8(b) => out.push(*b),
                    o => return Err(crate::extension::Duckerror::Internal(format!(
                        "colvec.validity: expected U8, got {o:?}"))),
                }
            }
            out
        }
        o => return Err(crate::extension::Duckerror::Internal(format!(
            "colvec.validity: expected Bytes or List<U8>, got {o:?}"))),
    };
    let rows = u32_field(v, "rows")?;
    Ok(extension_column_types::Colvec { data, validity, rows })
}

pub(crate) fn colvec_list_to_value(cvs: &[extension_column_types::Colvec]) -> Value {
    Value::List(cvs.iter().map(colvec_to_value).collect())
}

fn primitive_list_val<T: Copy>(items: &[T], f: impl Fn(T) -> Value) -> Value {
    Value::List(items.iter().map(|x| f(*x)).collect())
}

pub(crate) fn column_to_value(c: &extension_column_types::Column) -> Value {
    use extension_column_types::Column as C;
    let (disc, payload) = match c {
        C::Boolean(xs) => ("boolean", primitive_list_val(xs, Value::Bool)),
        C::Int64(xs) => ("int64", primitive_list_val(xs, Value::S64)),
        C::Uint64(xs) => ("uint64", primitive_list_val(xs, Value::U64)),
        C::Float64(xs) => ("float64", primitive_list_val(xs, Value::F64)),
        C::Int32(xs) => ("int32", primitive_list_val(xs, Value::S32)),
        C::Timestamp(xs) => ("timestamp", primitive_list_val(xs, Value::S64)),
        C::Int8(xs) => ("int8", primitive_list_val(xs, Value::S8)),
        C::Int16(xs) => ("int16", primitive_list_val(xs, Value::S16)),
        C::Uint8(xs) => ("uint8", primitive_list_val(xs, Value::U8)),
        C::Uint16(xs) => ("uint16", primitive_list_val(xs, Value::U16)),
        C::Uint32(xs) => ("uint32", primitive_list_val(xs, Value::U32)),
        C::Float32(xs) => ("float32", primitive_list_val(xs, Value::F32)),
        C::Date(xs) => ("date", primitive_list_val(xs, Value::S32)),
        C::Time(xs) => ("time", primitive_list_val(xs, Value::S64)),
        C::Timestamptz(xs) => ("timestamptz", primitive_list_val(xs, Value::S64)),
        C::Decimal(xs) => ("decimal", Value::List(xs.iter().map(|d| Value::Record(vec![
            ("lower".into(), Value::U64(d.lower)),
            ("upper".into(), Value::U64(d.upper)),
            ("width".into(), Value::U8(d.width)),
            ("scale".into(), Value::U8(d.scale)),
        ])).collect())),
        C::Interval(xs) => ("interval", Value::List(xs.iter().map(|i| Value::Record(vec![
            ("months".into(), Value::S32(i.months)),
            ("days".into(), Value::S32(i.days)),
            ("micros".into(), Value::S64(i.micros)),
        ])).collect())),
        C::Uuid(xs) => ("uuid", Value::List(xs.iter().map(|u| Value::Record(vec![
            ("hi".into(), Value::U64(u.hi)),
            ("lo".into(), Value::U64(u.lo)),
        ])).collect())),
        C::Text(xs) => ("text", Value::List(xs.iter().map(|s| Value::String(s.clone())).collect())),
        C::Blob(xs) => ("blob", Value::List(xs.iter().map(|b| bytes_to_value(b)).collect())),
        C::Hugeint(xs) => ("hugeint", Value::List(xs.iter().map(|h| Value::Record(vec![
            ("lower".into(), Value::U64(h.lower)),
            ("upper".into(), Value::S64(h.upper)),
        ])).collect())),
        C::Uhugeint(xs) => ("uhugeint", Value::List(xs.iter().map(|h| Value::Record(vec![
            ("lower".into(), Value::U64(h.lower)),
            ("upper".into(), Value::U64(h.upper)),
        ])).collect())),
        C::ListCol(n) => ("list-col", Value::Record(vec![
            ("encoded".into(), bytes_to_value(&n.encoded)),
        ])),
        C::StructCol(n) => ("struct-col", Value::Record(vec![
            ("encoded".into(), bytes_to_value(&n.encoded)),
        ])),
        C::MapCol(m) => ("map-col", Value::Record(vec![
            ("keys-encoded".into(), bytes_to_value(&m.keys_encoded)),
            ("vals-encoded".into(), bytes_to_value(&m.vals_encoded)),
        ])),
        C::ArrayCol(a) => ("array-col", Value::Record(vec![
            ("size".into(), Value::U32(a.size)),
            ("encoded".into(), bytes_to_value(&a.encoded)),
        ])),
        C::Complex(xs) => ("complex", Value::List(xs.iter().map(|c| Value::Record(vec![
            ("type-expr".into(), Value::String(c.type_expr.clone())),
            ("json".into(), Value::String(c.json.clone())),
        ])).collect())),
    };
    Value::Variant { discriminant: disc.into(), payload: Some(Box::new(payload)) }
}

/// Lift a Value::List of a specific primitive kind. Uses a closure
/// to extract the inner primitive with a shape-mismatch error.
fn lift_prim_list<T>(
    v: &Value,
    name: &str,
    extract: impl Fn(&Value) -> Option<T>,
) -> Result<Vec<T>, crate::extension::Duckerror> {
    let items = match v {
        Value::List(items) => items,
        o => return Err(crate::extension::Duckerror::Internal(format!(
            "expected List for column.{name}, got {o:?}"))),
    };
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        out.push(extract(item).ok_or_else(|| crate::extension::Duckerror::Internal(format!(
            "column.{name}[{i}]: shape mismatch, got {item:?}")))?);
    }
    Ok(out)
}

pub(crate) fn value_to_column(
    v: &Value,
) -> Result<extension_column_types::Column, crate::extension::Duckerror> {
    use extension_column_types::Column as C;
    let (disc, payload) = match v {
        Value::Variant { discriminant, payload } => (discriminant, payload),
        o => return Err(crate::extension::Duckerror::Internal(format!(
            "value_to_column: expected Variant, got {o:?}"))),
    };
    let p = payload.as_deref().ok_or_else(|| {
        crate::extension::Duckerror::Internal(format!("column.{disc}: missing payload"))
    })?;
    Ok(match disc.as_str() {
        "boolean" => C::Boolean(lift_prim_list(p, "boolean", |v| if let Value::Bool(b) = v { Some(*b) } else { None })?),
        "int64" => C::Int64(lift_prim_list(p, "int64", |v| if let Value::S64(n) = v { Some(*n) } else { None })?),
        "uint64" => C::Uint64(lift_prim_list(p, "uint64", |v| if let Value::U64(n) = v { Some(*n) } else { None })?),
        "float64" => C::Float64(lift_prim_list(p, "float64", |v| if let Value::F64(f) = v { Some(*f) } else { None })?),
        "int32" => C::Int32(lift_prim_list(p, "int32", |v| if let Value::S32(n) = v { Some(*n) } else { None })?),
        "timestamp" => C::Timestamp(lift_prim_list(p, "timestamp", |v| if let Value::S64(n) = v { Some(*n) } else { None })?),
        "int8" => C::Int8(lift_prim_list(p, "int8", |v| if let Value::S8(n) = v { Some(*n) } else { None })?),
        "int16" => C::Int16(lift_prim_list(p, "int16", |v| if let Value::S16(n) = v { Some(*n) } else { None })?),
        "uint8" => C::Uint8(lift_prim_list(p, "uint8", |v| if let Value::U8(n) = v { Some(*n) } else { None })?),
        "uint16" => C::Uint16(lift_prim_list(p, "uint16", |v| if let Value::U16(n) = v { Some(*n) } else { None })?),
        "uint32" => C::Uint32(lift_prim_list(p, "uint32", |v| if let Value::U32(n) = v { Some(*n) } else { None })?),
        "float32" => C::Float32(lift_prim_list(p, "float32", |v| if let Value::F32(f) = v { Some(*f) } else { None })?),
        "date" => C::Date(lift_prim_list(p, "date", |v| if let Value::S32(n) = v { Some(*n) } else { None })?),
        "time" => C::Time(lift_prim_list(p, "time", |v| if let Value::S64(n) = v { Some(*n) } else { None })?),
        "timestamptz" => C::Timestamptz(lift_prim_list(p, "timestamptz", |v| if let Value::S64(n) = v { Some(*n) } else { None })?),
        "text" => C::Text(lift_prim_list(p, "text", |v| if let Value::String(s) = v { Some(s.clone()) } else { None })?),
        "decimal" => {
            let items = match p {
                Value::List(items) => items,
                o => return Err(crate::extension::Duckerror::Internal(format!("column.decimal: expected List, got {o:?}"))),
            };
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                out.push(extension_column_types::Decimalvalue {
                    lower: u64_field(it, "lower")?,
                    upper: u64_field(it, "upper")?,
                    width: u8_field(it, "width")?,
                    scale: u8_field(it, "scale")?,
                });
            }
            C::Decimal(out)
        }
        "interval" => {
            let items = match p {
                Value::List(items) => items,
                o => return Err(crate::extension::Duckerror::Internal(format!("column.interval: expected List, got {o:?}"))),
            };
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                out.push(extension_column_types::Intervalvalue {
                    months: s32_field(it, "months")?,
                    days: s32_field(it, "days")?,
                    micros: s64_field(it, "micros")?,
                });
            }
            C::Interval(out)
        }
        "uuid" => {
            let items = match p {
                Value::List(items) => items,
                o => return Err(crate::extension::Duckerror::Internal(format!("column.uuid: expected List, got {o:?}"))),
            };
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                out.push(extension_column_types::Uuidvalue {
                    hi: u64_field(it, "hi")?,
                    lo: u64_field(it, "lo")?,
                });
            }
            C::Uuid(out)
        }
        "blob" => {
            let items = match p {
                Value::List(items) => items,
                o => return Err(crate::extension::Duckerror::Internal(format!("column.blob: expected List, got {o:?}"))),
            };
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                match it {
                    Value::Bytes(b) => out.push(b.to_vec()),
                    Value::List(bs) => {
                        let mut v = Vec::with_capacity(bs.len());
                        for b in bs {
                            match b {
                                Value::U8(x) => v.push(*x),
                                o => return Err(crate::extension::Duckerror::Internal(format!("column.blob element: expected U8, got {o:?}"))),
                            }
                        }
                        out.push(v);
                    }
                    o => return Err(crate::extension::Duckerror::Internal(format!("column.blob element: expected Bytes/List, got {o:?}"))),
                }
            }
            C::Blob(out)
        }
        "hugeint" => {
            let items = match p {
                Value::List(items) => items,
                o => return Err(crate::extension::Duckerror::Internal(format!("column.hugeint: expected List, got {o:?}"))),
            };
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                out.push(extension_column_types::DuckInt128 {
                    lower: u64_field(it, "lower")?,
                    upper: s64_field(it, "upper")?,
                });
            }
            C::Hugeint(out)
        }
        "uhugeint" => {
            let items = match p {
                Value::List(items) => items,
                o => return Err(crate::extension::Duckerror::Internal(format!("column.uhugeint: expected List, got {o:?}"))),
            };
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                out.push(extension_column_types::DuckUint128 {
                    lower: u64_field(it, "lower")?,
                    upper: u64_field(it, "upper")?,
                });
            }
            C::Uhugeint(out)
        }
        "list-col" => C::ListCol(extension_column_types::NestedColumn {
            encoded: bytes_field(p, "encoded")?,
        }),
        "struct-col" => C::StructCol(extension_column_types::NestedColumn {
            encoded: bytes_field(p, "encoded")?,
        }),
        "map-col" => C::MapCol(extension_column_types::MapColumn {
            keys_encoded: bytes_field(p, "keys-encoded")?,
            vals_encoded: bytes_field(p, "vals-encoded")?,
        }),
        "array-col" => C::ArrayCol(extension_column_types::ArrayColumn {
            size: u32_field(p, "size")?,
            encoded: bytes_field(p, "encoded")?,
        }),
        "complex" => {
            let items = match p {
                Value::List(items) => items,
                o => return Err(crate::extension::Duckerror::Internal(format!("column.complex: expected List, got {o:?}"))),
            };
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                out.push(extension_column_types::Complexvalue {
                    type_expr: string_field(it, "type-expr")?,
                    json: string_field(it, "json")?,
                });
            }
            C::Complex(out)
        }
        other => return Err(crate::extension::Duckerror::Internal(format!(
            "value_to_column: unknown discriminant {other:?}"))),
    })
}

fn bytes_field(rec: &Value, name: &str) -> Result<Vec<u8>, crate::extension::Duckerror> {
    match record_field(rec, name)? {
        Value::Bytes(b) => Ok(b.to_vec()),
        Value::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                match it {
                    Value::U8(b) => out.push(*b),
                    o => return Err(crate::extension::Duckerror::Internal(format!(
                        "field {name:?} element: expected U8, got {o:?}"))),
                }
            }
            Ok(out)
        }
        o => Err(crate::extension::Duckerror::Internal(format!(
            "field {name:?}: expected Bytes or List<U8>, got {o:?}"))),
    }
}

