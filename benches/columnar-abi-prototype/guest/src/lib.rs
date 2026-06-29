#[allow(warnings)]
mod bindings;

use bindings::exports::bench::dispatch::columnar::Guest as ColGuest;
use bindings::exports::bench::dispatch::rowmajor::Guest as RowGuest;

use bindings::bench::dispatch::types::{
    Colvec, Column, Dispatcherror, Duckvalue,
};

struct Component;

// Row-major path: faithful to the current shim — match the variant per cell,
// produce the variant per cell. wit-bindgen lifts list<list<duckvalue>> into
// Vec<Vec<Duckvalue>> (per-cell variant decode) before this even runs, and
// lowers the Vec<Duckvalue> result the same way.
impl RowGuest for Component {
    fn call_scalar_batch(
        rows: Vec<Vec<Duckvalue>>,
    ) -> Result<Vec<Duckvalue>, Dispatcherror> {
        let mut out = Vec::with_capacity(rows.len());
        for args in &rows {
            match args.first() {
                Some(Duckvalue::Int64(n)) => out.push(Duckvalue::Int64(n + 1)),
                Some(Duckvalue::Null) | None => out.push(Duckvalue::Null),
                _ => out.push(Duckvalue::Null),
            }
        }
        Ok(out)
    }
}

// Columnar path: wit-bindgen lifts list<s64> as a single bulk copy; we operate
// on the typed slice directly (SIMD-able), and lower the result list<s64> as a
// single bulk copy. Zero per-cell variant work.
impl ColGuest for Component {
    fn call_scalar_batch_col(args: Vec<Colvec>) -> Result<Colvec, Dispatcherror> {
        let first = args
            .into_iter()
            .next()
            .ok_or_else(|| Dispatcherror::Failed("no args".into()))?;
        let rows = first.rows;
        match first.data {
            Column::Int64(v) => {
                let mut out = v; // reuse the buffer
                for x in out.iter_mut() {
                    *x += 1;
                }
                Ok(Colvec {
                    data: Column::Int64(out),
                    validity: first.validity,
                    rows,
                })
            }
            _ => Err(Dispatcherror::Failed("unsupported column type".into())),
        }
    }
}

bindings::export!(Component with_types_in bindings);
