//! Fuzz the hand-rolled PostgreSQL v3 wire-protocol parser (untrusted SERVER
//! bytes).
//!
//! `postgres.rs` is wit-free (std + md5), so we `#[path]`-include it and drive
//! its parsing surface from the libfuzzer byte buffer:
//!   * `parse_query_response` over an in-memory `Cursor` (the simple-query BE
//!     message loop: RowDescription / DataRow / ErrorResponse / ReadyForQuery
//!     framing), exactly as it would run over a `TcpStream`.
//!   * `parse_row_description`, `parse_data_row`, `parse_error_response` on the
//!     raw payload bytes.
//!
//! Contract under test: NONE of these may panic on adversarial input.
#![no_main]

use libfuzzer_sys::fuzz_target;
use std::io::Cursor;

#[path = "../../extensions/postgreswasm-component/src/postgres.rs"]
mod postgres;

fuzz_target!(|data: &[u8]| {
    // 1. Drive the full simple-query backend message loop over the bytes as if
    //    they were the server's stream (tag + BE length + payload, repeated).
    let mut cur = Cursor::new(data);
    let _ = postgres::parse_query_response(&mut cur);

    // 2. Hit the individual message parsers with the raw buffer as a payload.
    let _ = postgres::parse_row_description(data);
    let _ = postgres::parse_data_row(data);
    let _ = postgres::parse_error_response(data);
});
