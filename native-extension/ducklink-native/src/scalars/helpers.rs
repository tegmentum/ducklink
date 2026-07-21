//! Shared VScalar wrappers for the two dominant shapes in the bundle:
//!
//! * `TEXT -> BOOLEAN` (validators — aba_validate, iban_validate, ...)
//! * `TEXT -> TEXT`    (transformers — iban_country, cc_normalize, ...)
//!
//! Both take a single VARCHAR input, all-valid per-row, no NULL propagation
//! from the input side (a NULL input to the wrapped `fn(&str)` never fires —
//! DuckDB routes NULL rows to NULL output automatically for `text -> text`).
//!
//! Each concrete scalar is a zero-sized type implementing `Bool` or `Text`,
//! wired through `bool_scalar!` / `text_scalar!` macros so the boilerplate
//! is one line per function.

use std::error::Error;

use duckdb::core::{DataChunkHandle, Inserter, LogicalTypeId};
use duckdb::ffi;
use duckdb::types::DuckString;
use duckdb::vscalar::{ScalarFunctionSignature, VScalar};
use duckdb::vtab::arrow::WritableVector;

/// Trait describing a `TEXT -> BOOLEAN` function via a pure-Rust callable.
/// The `Bool` type parameter is a zero-sized marker; the wrapped function is
/// invoked per row inside `VScalar::invoke`.
pub trait BoolScalar {
    /// Function name registered into DuckDB.
    const NAME: &'static str;
    /// Callable: text in, boolean out.
    fn invoke(text: &str) -> bool;
}

/// Trait describing a `TEXT -> TEXT` function via a pure-Rust callable.
/// `None` from the callable becomes SQL NULL.
pub trait TextScalar {
    const NAME: &'static str;
    fn invoke(text: &str) -> Option<String>;
}

/// Trait describing a `TEXT -> BIGINT` function via a pure-Rust callable.
/// `None` from the callable becomes SQL NULL. Used by luhn_check_digit.
pub trait IntScalar {
    const NAME: &'static str;
    fn invoke(text: &str) -> Option<i64>;
}

/// VScalar adapter — one struct per BoolScalar impl. Written as a generic
/// so registration is one line per function.
pub struct BoolAdapter<T: BoolScalar + 'static>(std::marker::PhantomData<T>);

impl<T: BoolScalar + 'static> VScalar for BoolAdapter<T> {
    type State = ();

    fn invoke(
        _: &(),
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> Result<(), Box<dyn Error>> {
        let len = input.len();
        let arg = input.flat_vector(0);
        let mut out = output.flat_vector();
        unsafe {
            let arg_slice = arg.as_slice_with_len::<ffi::duckdb_string_t>(len);
            let out_slice = out.as_mut_slice_with_len::<bool>(len);
            for i in 0..len {
                let mut s = arg_slice[i];
                out_slice[i] = T::invoke(&DuckString::new(&mut s).as_str());
            }
        }
        Ok(())
    }

    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![LogicalTypeId::Varchar.into()],
            LogicalTypeId::Boolean.into(),
        )]
    }
}

/// VScalar adapter — one struct per TextScalar impl.
pub struct TextAdapter<T: TextScalar + 'static>(std::marker::PhantomData<T>);

impl<T: TextScalar + 'static> VScalar for TextAdapter<T> {
    type State = ();

    fn invoke(
        _: &(),
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> Result<(), Box<dyn Error>> {
        let len = input.len();
        let arg = input.flat_vector(0);
        let mut out = output.flat_vector();
        // Two-pass: pre-collect results (so we can release the input borrow
        // before writing back into the output), then write. Cheaper on
        // narrow strings than double-borrow gymnastics.
        let mut results: Vec<Option<String>> = Vec::with_capacity(len);
        unsafe {
            let arg_slice = arg.as_slice_with_len::<ffi::duckdb_string_t>(len);
            for i in 0..len {
                let mut s = arg_slice[i];
                let text = DuckString::new(&mut s).as_str();
                results.push(T::invoke(&text));
            }
        }
        for (i, r) in results.into_iter().enumerate() {
            match r {
                Some(s) => out.insert(i, s.as_str()),
                None => out.set_null(i),
            }
        }
        Ok(())
    }

    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![LogicalTypeId::Varchar.into()],
            LogicalTypeId::Varchar.into(),
        )]
    }
}

/// VScalar adapter — one struct per IntScalar impl.
pub struct IntAdapter<T: IntScalar + 'static>(std::marker::PhantomData<T>);

impl<T: IntScalar + 'static> VScalar for IntAdapter<T> {
    type State = ();

    fn invoke(
        _: &(),
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> Result<(), Box<dyn Error>> {
        let len = input.len();
        let arg = input.flat_vector(0);
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        unsafe {
            let arg_slice = arg.as_slice_with_len::<ffi::duckdb_string_t>(len);
            let out_slice = out.as_mut_slice_with_len::<i64>(len);
            for i in 0..len {
                let mut s = arg_slice[i];
                let text = DuckString::new(&mut s).as_str();
                match T::invoke(&text) {
                    Some(n) => out_slice[i] = n,
                    None => nulls.push(i),
                }
            }
        }
        for i in nulls {
            out.set_null(i);
        }
        Ok(())
    }

    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![LogicalTypeId::Varchar.into()],
            LogicalTypeId::Bigint.into(),
        )]
    }
}
