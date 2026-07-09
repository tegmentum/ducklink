//! `aba` — native DuckDB extension for US ABA routing-number checks.
//!
//! Three scalar functions bound directly against DuckDB's C Extension API via
//! duckdb-rs, sharing the same `aba-core` logic crate the ducklink WASM
//! `aba-component` uses:
//!
//!   * `aba_validate(text) -> boolean`
//!   * `aba_frb_district(text) -> int64`   (NULL for unrecognized prefix)
//!   * `aba_fed_region(text) -> text`      (NULL for unrecognized prefix)
//!
//! Same functions ducklink offers via `ducklink_load('aba')`; the difference
//! is that this build eliminates the WASM sandbox layer, so per-row cost
//! sits at DuckDB's native vectorized-executor floor (~1 ns/row) instead
//! of the ~40 ns/row WASM ceiling.

#[cfg(feature = "loadable")]
mod loadable {
    use std::error::Error;
    use std::ffi::CString;

    use duckdb::core::{DataChunkHandle, Inserter, LogicalTypeId};
    use duckdb::ffi;
    use duckdb::types::DuckString;
    use duckdb::vscalar::{ScalarFunctionSignature, VScalar};
    use duckdb::vtab::arrow::WritableVector;
    use duckdb::Connection;

    use aba_core::logic;

    /// `aba_validate(VARCHAR) -> BOOLEAN`: 9-digit weighted checksum.
    struct AbaValidate;
    impl VScalar for AbaValidate {
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
                let out_slice = out.as_mut_slice_with_len::<bool>(len);
                let arg_slice = arg.as_slice_with_len::<ffi::duckdb_string_t>(len);
                for i in 0..len {
                    let mut s = arg_slice[i];
                    let text = DuckString::new(&mut s).as_str();
                    out_slice[i] = logic::validate(&text);
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

    /// `aba_frb_district(VARCHAR) -> BIGINT`: FRB district 1..12 (0 for
    /// Treasury), NULL for an unrecognized prefix.
    struct AbaFrbDistrict;
    impl VScalar for AbaFrbDistrict {
        type State = ();

        fn invoke(
            _: &(),
            input: &mut DataChunkHandle,
            output: &mut dyn WritableVector,
        ) -> Result<(), Box<dyn Error>> {
            let len = input.len();
            let arg = input.flat_vector(0);
            let mut out = output.flat_vector();
            // Two-pass write: fill values into the typed slice, collect NULL
            // row indexes, then release the slice borrow before calling
            // `out.set_null` for each. Mirrors the pattern the ducklink WASM
            // sink uses in `write_colvec`.
            let mut nulls: Vec<usize> = Vec::new();
            unsafe {
                let arg_slice = arg.as_slice_with_len::<ffi::duckdb_string_t>(len);
                let out_slice = out.as_mut_slice_with_len::<i64>(len);
                for i in 0..len {
                    let mut s = arg_slice[i];
                    let text = DuckString::new(&mut s).as_str();
                    match logic::frb(&text) {
                        Some(d) => out_slice[i] = d as i64,
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

    /// `aba_fed_region(VARCHAR) -> VARCHAR`: region name, NULL for
    /// unrecognized prefix.
    struct AbaFedRegion;
    impl VScalar for AbaFedRegion {
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
                for i in 0..len {
                    let mut s = arg_slice[i];
                    let text = DuckString::new(&mut s).as_str();
                    match logic::frb(&text) {
                        Some(d) => out.insert(i, logic::fed_region(d)),
                        None => out.set_null(i),
                    }
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

    /// DuckDB C-API extension entry point. DuckDB derives the symbol name
    /// from the filename (`aba.duckdb_extension` → `aba_init_c_api`).
    ///
    /// # Safety
    /// Called by DuckDB during `LOAD` with a valid `info` / `access` pair.
    #[no_mangle]
    pub unsafe extern "C" fn aba_init_c_api(
        info: ffi::duckdb_extension_info,
        access: *const ffi::duckdb_extension_access,
    ) -> bool {
        match init(info, access) {
            Ok(loaded) => loaded,
            Err(e) => {
                if let Some(set_error) = (*access).set_error {
                    if let Ok(c) = CString::new(e.to_string()) {
                        set_error(info, c.as_ptr());
                    }
                }
                false
            }
        }
    }

    unsafe fn init(
        info: ffi::duckdb_extension_info,
        access: *const ffi::duckdb_extension_access,
    ) -> Result<bool, Box<dyn Error>> {
        if !ffi::duckdb_rs_extension_api_init(info, access, "v1.5.4").map_err(stringify)? {
            return Ok(false);
        }
        let get_database = (*access)
            .get_database
            .ok_or_else(|| stringify("get_database is null in duckdb_extension_access"))?;
        let db_ptr = get_database(info);
        if db_ptr.is_null() {
            return Ok(false);
        }
        let db: ffi::duckdb_database = *db_ptr;
        let con = Connection::open_from_raw(db.cast())?;
        con.register_scalar_function::<AbaValidate>("aba_validate")
            .map_err(stringify)?;
        con.register_scalar_function::<AbaFrbDistrict>("aba_frb_district")
            .map_err(stringify)?;
        con.register_scalar_function::<AbaFedRegion>("aba_fed_region")
            .map_err(stringify)?;
        Ok(true)
    }

    fn stringify(err: impl std::fmt::Display) -> Box<dyn Error> {
        err.to_string().into()
    }
}
