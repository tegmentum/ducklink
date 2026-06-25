//! Fuzz the hand-rolled MySQL wire-protocol parser (untrusted SERVER bytes).
//!
//! `mysql.rs` is wit-free (std + sha1), so we `#[path]`-include it directly and
//! drive its parsing surface from the libfuzzer byte buffer:
//!   * `parse_result_set` over an in-memory `Cursor` (the COM_QUERY reply: the
//!     column-count packet + column-def packets + rows), exactly as it would run
//!     over a `TcpStream`, but fed adversarial bytes.
//!   * `parse_column_def`, `parse_text_row`, `parse_err_packet` on the raw bytes.
//!
//! Contract under test: NONE of these may panic. Every malformed input must come
//! back as an `Err` (or a benign value), never an abort.
#![no_main]

use libfuzzer_sys::fuzz_target;
use std::io::Cursor;

#[path = "../../extensions/mysqlwasm-component/src/mysql.rs"]
mod mysql;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }

    // 1. Drive the full result-set parser. The first byte selects how we split
    //    `data` into the already-read "first" packet vs the cursor of remaining
    //    packets, so the corpus exercises both the OK/ERR fast paths and the
    //    column-count -> column-def -> row state machine.
    let split = (data[0] as usize) % data.len();
    let (first, rest) = data.split_at(split);
    let mut cur = Cursor::new(rest);
    let _ = mysql::parse_result_set(&mut cur, first);

    // 2. Hit the individual packet parsers directly with the whole buffer.
    let _ = mysql::parse_column_def(data);
    let _ = mysql::parse_text_row(data, (data[0] as usize) & 0xff);
    let _ = mysql::parse_err_packet(data);
});
