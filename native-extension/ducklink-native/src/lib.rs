//! `ducklink-native` — curated bundle of native DuckDB scalars sharing pure
//! logic crates with the ducklink WASM catalog.
//!
//! Companion to the `ducklink` extension. Where `ducklink` embeds wasmtime
//! to run any of 216 datalink components dynamically as WASM, this ships
//! a curated subset of perf-sensitive validators / encoders / transforms
//! compiled directly against duckdb-rs. Same pure-logic crates, native
//! DuckDB speed, no sandbox.
//!
//! Bundled today (15 scalars, 5 -core crates):
//!
//!   aba_*         — US ABA routing-number checks         (aba-core)
//!   iban_*        — International bank account checks    (iban-core)
//!   isbn_*        — ISBN check + normalize               (isbn-core)
//!   luhn_*        — Luhn checksum + check-digit          (luhn-core)
//!   cc_*          — Credit-card validate/brand/mask/...  (creditcard-core)
//!
//! Add a new capability by dropping a new module under `src/scalars/`,
//! adding its `-core` dependency to `Cargo.toml`, and appending
//! `register_*` calls in `loadable::init()`.

pub mod scalars;

#[cfg(feature = "loadable")]
mod loadable {
    use std::error::Error;
    use std::ffi::CString;

    use duckdb::ffi;
    use duckdb::Connection;

    use crate::scalars::helpers::{BoolAdapter, IntAdapter, TextAdapter};
    use crate::scalars::{aba, creditcard, iban, isbn, luhn};

    /// DuckDB C-API entry point. DuckDB derives the symbol name from the
    /// filename: `ducklink_native.duckdb_extension` -> `ducklink_native_init_c_api`.
    ///
    /// # Safety
    /// Called by DuckDB during `LOAD` with a valid `info` / `access` pair.
    #[no_mangle]
    pub unsafe extern "C" fn ducklink_native_init_c_api(
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

        // aba — 3 functions.
        register_bool::<aba::Validate>(&con)?;
        register_int::<aba::FrbDistrict>(&con)?;
        register_text::<aba::FedRegion>(&con)?;

        // iban — 3 functions.
        register_bool::<iban::Validate>(&con)?;
        register_text::<iban::Country>(&con)?;
        register_text::<iban::Bban>(&con)?;

        // isbn — 2 functions.
        register_bool::<isbn::Valid>(&con)?;
        register_text::<isbn::Normalize>(&con)?;

        // luhn — 2 functions.
        register_bool::<luhn::Validate>(&con)?;
        register_int::<luhn::CheckDigit>(&con)?;

        // creditcard — 7 functions.
        register_bool::<creditcard::Validate>(&con)?;
        register_text::<creditcard::Network>(&con)?;
        register_text::<creditcard::Type>(&con)?;
        register_text::<creditcard::Mask>(&con)?;
        register_text::<creditcard::Last4>(&con)?;
        register_text::<creditcard::Bin>(&con)?;
        register_text::<creditcard::Normalize>(&con)?;

        Ok(true)
    }

    /// Wire a `BoolScalar` impl into DuckDB under its declared NAME.
    fn register_bool<T: crate::scalars::helpers::BoolScalar + 'static>(
        con: &Connection,
    ) -> Result<(), Box<dyn Error>> {
        con.register_scalar_function::<BoolAdapter<T>>(T::NAME)
            .map_err(stringify)
    }

    fn register_text<T: crate::scalars::helpers::TextScalar + 'static>(
        con: &Connection,
    ) -> Result<(), Box<dyn Error>> {
        con.register_scalar_function::<TextAdapter<T>>(T::NAME)
            .map_err(stringify)
    }

    fn register_int<T: crate::scalars::helpers::IntScalar + 'static>(
        con: &Connection,
    ) -> Result<(), Box<dyn Error>> {
        con.register_scalar_function::<IntAdapter<T>>(T::NAME)
            .map_err(stringify)
    }

    fn stringify(err: impl std::fmt::Display) -> Box<dyn Error> {
        err.to_string().into()
    }
}
