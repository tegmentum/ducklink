//! Read .xlsx workbooks as DuckDB table functions over an in-memory BLOB.
//!
//! Reimplements the READ side of DuckDB's official `excel` extension as a
//! loadable ducklink component. A worksheet's schema is dynamic, but the
//! component table-function registry needs a FIXED column list at registration
//! time -- so `read_xlsx` uses the same MELTED shape as `read_parquet` /
//! `sqlite_blob_scan`: one (sheet, row_no, col, val) tuple per non-empty cell.
//!
//!   xlsx_sheets(data BLOB) -> table(
//!       sheet VARCHAR)                       -- one row per worksheet name
//!
//!   read_xlsx(data BLOB) -> table(
//!       sheet  VARCHAR,                       -- worksheet name
//!       row_no BIGINT,                        -- 1-indexed spreadsheet row
//!       col    VARCHAR,                       -- Excel column letter (A, B, ...)
//!       val    VARCHAR)                       -- cell value rendered as text
//!
//!   xlsx_cell(data BLOB, sheet VARCHAR, cell VARCHAR) -> table(
//!       val VARCHAR)                          -- single A1-addressed cell value
//!
//! All functions accept the workbook as a real BLOB or as a hex STRING (the
//! wasm core registers table-function params as VARCHAR, so the SQL entry point
//! passes hex which we decode). A malformed / empty / NULL blob yields ZERO
//! rows -- never a panic and never an error (calamine `open` failures are
//! swallowed).
use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex, OnceLock,
};

use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });

use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};

use calamine::{Data, Reader, Xlsx};

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_all()?;
        Ok(types::Loadresult {
            name: "excelfns".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: Vec::new().into(),
        })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> {
        Ok(false)
    }
    fn shutdown() -> Result<bool, types::Duckerror> {
        Ok(false)
    }
}

impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(
        _h: u32,
        _r: Vec<Vec<types::Duckvalue>>,
        _c: types::Invokeinfo,
    ) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("excelfns: no scalar fns".into()))
    }
    fn call_scalar(
        _h: u32,
        _a: Vec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("excelfns: no scalar fns".into()))
    }

    fn call_table(
        handle: u32,
        args: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        let which = handlers()
            .lock()
            .unwrap()
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown table handle".into()))?;

        let mut it = args.into_iter();

        // First argument: the workbook bytes (BLOB or hex STRING). A NULL /
        // absent / non-decodable first arg yields zero rows (never an error).
        let bytes = match it.next() {
            Some(types::Duckvalue::Blob(b)) => Some(b.into()),
            Some(types::Duckvalue::Text(s)) => hex_decode(&s),
            _ => None,
        };
        let bytes: std::vec::Vec<u8> = match bytes {
            Some(b) => b,
            None => return Ok(Vec::new().into()),
        };

        let rows = match which {
            T::Sheets => sheets_rows(&bytes),
            T::Read => read_melted(&bytes),
            T::Cell => {
                let sheet = text_arg(it.next());
                let cell = text_arg(it.next());
                match (sheet, cell) {
                    (Some(s), Some(c)) => cell_rows(&bytes, &s, &c),
                    _ => std::vec::Vec::new(),
                }
            }
        };
        Ok(rows.into())
    }

    fn call_aggregate(
        _h: u32,
        _r: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("excelfns: no aggs".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("excelfns: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("excelfns: no casts".into()))
    }
}

export!(Extension);

// ---------------------------------------------------------------------------
// Core readers (pure functions over `&[u8]`; unit-tested natively).
// ---------------------------------------------------------------------------

/// Open an xlsx workbook from bytes. Returns None on any malformed input
/// (never panics).
fn open(bytes: &[u8]) -> Option<Xlsx<Cursor<std::vec::Vec<u8>>>> {
    let cursor = Cursor::new(bytes.to_vec());
    calamine::open_workbook_from_rs::<Xlsx<_>, _>(cursor).ok()
}

/// xlsx_sheets: one (sheet) row per worksheet name, in workbook order.
fn sheets_rows(bytes: &[u8]) -> std::vec::Vec<std::vec::Vec<types::Duckvalue>> {
    let wb = match open(bytes) {
        Some(w) => w,
        None => return std::vec::Vec::new(),
    };
    wb.sheet_names()
        .into_iter()
        .map(|name| vec![types::Duckvalue::Text(name.into())])
        .collect()
}

/// read_xlsx (MELTED): one (sheet, row_no, col, val) tuple per non-empty cell,
/// across every worksheet. `row_no` is the 1-indexed spreadsheet row and `col`
/// is the Excel column letter (A, B, ..., Z, AA, ...). Empty cells are skipped.
fn read_melted(bytes: &[u8]) -> std::vec::Vec<std::vec::Vec<types::Duckvalue>> {
    let mut wb = match open(bytes) {
        Some(w) => w,
        None => return std::vec::Vec::new(),
    };

    let mut out = std::vec::Vec::new();
    let names = wb.sheet_names();
    for name in names {
        let range = match wb.worksheet_range(&name) {
            Ok(r) => r,
            Err(_) => continue,
        };
        // used_cells() yields (row, col) RELATIVE to the range's top-left; add
        // the range start to recover absolute spreadsheet coordinates.
        let (start_row, start_col) = range.start().unwrap_or((0, 0));
        for (r, c, data) in range.used_cells() {
            let abs_row = start_row as i64 + r as i64 + 1; // 1-indexed
            let abs_col = start_col as usize + c;
            out.push(vec![
                types::Duckvalue::Text(name.clone().into()),
                types::Duckvalue::Int64(abs_row),
                types::Duckvalue::Text(col_letter(abs_col).into()),
                data_as_text(data),
            ]);
        }
    }
    out
}

/// xlsx_cell: a single (val) row for the A1-addressed `cell` on `sheet`. An
/// unknown sheet, unparsable address, or empty cell yields zero rows.
fn cell_rows(
    bytes: &[u8],
    sheet: &str,
    cell: &str,
) -> std::vec::Vec<std::vec::Vec<types::Duckvalue>> {
    let mut wb = match open(bytes) {
        Some(w) => w,
        None => return std::vec::Vec::new(),
    };
    let range = match wb.worksheet_range(sheet) {
        Ok(r) => r,
        Err(_) => return std::vec::Vec::new(),
    };
    let (row, col) = match parse_a1(cell) {
        Some(rc) => rc,
        None => return std::vec::Vec::new(),
    };
    // get() takes ABSOLUTE (row, col) coordinates on this Range.
    match range.get_value((row, col)) {
        Some(d) if *d != Data::Empty => {
            vec![vec![data_as_text(d)]]
        }
        _ => std::vec::Vec::new(),
    }
}

/// Render a calamine `Data` cell as TEXT for the melted `val` slot. Empty maps
/// to NULL; everything else uses the crate's Display.
fn data_as_text(d: &Data) -> types::Duckvalue {
    match d {
        Data::Empty => types::Duckvalue::Null,
        other => types::Duckvalue::Text(other.to_string().into()),
    }
}

/// Convert a 0-indexed column number to an Excel column letter (0->A, 25->Z,
/// 26->AA, ...).
fn col_letter(mut col: usize) -> std::string::String {
    let mut s = std::vec::Vec::new();
    loop {
        s.push(b'A' + (col % 26) as u8);
        if col < 26 {
            break;
        }
        col = col / 26 - 1;
    }
    s.reverse();
    std::string::String::from_utf8(s).unwrap_or_default()
}

/// Parse an A1-style cell address ("B3", "AA12") into absolute 0-indexed
/// (row, col). None on any malformed input.
fn parse_a1(cell: &str) -> Option<(u32, u32)> {
    let cell = cell.trim();
    let bytes = cell.as_bytes();
    let mut i = 0;
    let mut col: u32 = 0;
    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        col = col
            .checked_mul(26)?
            .checked_add((bytes[i].to_ascii_uppercase() - b'A' + 1) as u32)?;
        i += 1;
    }
    if i == 0 || i == bytes.len() {
        return None; // need both letters and digits
    }
    let mut row: u32 = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if !b.is_ascii_digit() {
            return None;
        }
        row = row.checked_mul(10)?.checked_add((b - b'0') as u32)?;
        i += 1;
    }
    if col == 0 || row == 0 {
        return None;
    }
    Some((row - 1, col - 1)) // 0-indexed
}

/// Decode an ASCII hex string into bytes; None on any invalid char / odd length.
fn hex_decode(s: &str) -> Option<std::vec::Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    let nib = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let b = s.as_bytes();
    let mut out = std::vec::Vec::with_capacity(b.len() / 2);
    let mut i = 0;
    while i < b.len() {
        out.push((nib(b[i])? << 4) | nib(b[i + 1])?);
        i += 2;
    }
    Some(out)
}

/// Coerce an optional duckvalue argument to an owned TEXT string.
fn text_arg(v: Option<types::Duckvalue>) -> Option<std::string::String> {
    match v {
        Some(types::Duckvalue::Text(s)) => Some(s.to_string()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Registration + handle dispatch.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum T {
    Sheets,
    Read,
    Cell,
}
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, T>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, T>> {
    HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_all() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Table)
        .ok_or_else(|| types::Duckerror::Internal("no table capability".into()))?;
    let reg = match cap {
        runtime::Capability::Table(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };

    let data_arg = || runtime::Funcarg {
        name: Some("data".into()),
        logical: types::Logicaltype::Blob,
    };

    // xlsx_sheets --------------------------------------------------------
    {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(h, T::Sheets);
        let args = vec![data_arg()];
        let columns = vec![types::Columndef {
            name: "sheet".into(),
            logical: types::Logicaltype::Text,
        }];
        let opts = runtime::Extopts {
            description: Some("Worksheet names of an .xlsx BLOB: one (sheet) row per sheet".into()),
            tags: vec!["excel".into(), "xlsx".into(), "sheets".into()],
        };
        reg.register("xlsx_sheets", &args, &columns, runtime::TableCallback::new(h), Some(&opts))?;
    }

    // read_xlsx (MELTED) -------------------------------------------------
    {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(h, T::Read);
        let args = vec![data_arg()];
        let columns = vec![
            types::Columndef { name: "sheet".into(), logical: types::Logicaltype::Text },
            types::Columndef { name: "row_no".into(), logical: types::Logicaltype::Int64 },
            types::Columndef { name: "col".into(), logical: types::Logicaltype::Text },
            types::Columndef { name: "val".into(), logical: types::Logicaltype::Text },
        ];
        let opts = runtime::Extopts {
            description: Some(
                "Read an .xlsx BLOB, MELTING each non-empty cell across every sheet into \
                 (sheet, row_no, col, val) tuples (component table fns need fixed columns; \
                 the xlsx schema is dynamic)"
                    .into(),
            ),
            tags: vec!["excel".into(), "xlsx".into(), "read".into(), "melted".into()],
        };
        reg.register("read_xlsx", &args, &columns, runtime::TableCallback::new(h), Some(&opts))?;
    }

    // xlsx_cell ----------------------------------------------------------
    {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(h, T::Cell);
        let args = vec![
            data_arg(),
            runtime::Funcarg { name: Some("sheet".into()), logical: types::Logicaltype::Text },
            runtime::Funcarg { name: Some("cell".into()), logical: types::Logicaltype::Text },
        ];
        let columns = vec![types::Columndef {
            name: "val".into(),
            logical: types::Logicaltype::Text,
        }];
        let opts = runtime::Extopts {
            description: Some(
                "Value of a single A1-addressed cell (e.g. 'B2') on <sheet> of an .xlsx BLOB"
                    .into(),
            ),
            tags: vec!["excel".into(), "xlsx".into(), "cell".into()],
        };
        reg.register("xlsx_cell", &args, &columns, runtime::TableCallback::new(h), Some(&opts))?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Native tests: build a tiny .xlsx in-memory, then drive the readers.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use rust_xlsxwriter::Workbook;

    /// A workbook with one sheet "Sheet1" carrying a 2x2 table:
    ///   A1="a" B1="b"
    ///   A2=1   B2=2
    fn make_xlsx() -> std::vec::Vec<u8> {
        let mut wb = Workbook::new();
        let ws = wb.add_worksheet(); // default name "Sheet1"
        ws.write_string(0, 0, "a").unwrap();
        ws.write_string(0, 1, "b").unwrap();
        ws.write_number(1, 0, 1.0).unwrap();
        ws.write_number(1, 1, 2.0).unwrap();
        wb.save_to_buffer().unwrap()
    }

    fn as_text(v: &types::Duckvalue) -> std::string::String {
        match v {
            types::Duckvalue::Text(s) => s.to_string(),
            types::Duckvalue::Int64(i) => i.to_string(),
            types::Duckvalue::Null => "NULL".to_string(),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn sheets_lists_one_sheet() {
        let bytes = make_xlsx();
        let rows = sheets_rows(&bytes);
        let got: std::vec::Vec<std::string::String> =
            rows.iter().map(|r| as_text(&r[0])).collect();
        assert_eq!(got, vec!["Sheet1".to_string()]);
    }

    #[test]
    fn melted_read_emits_one_tuple_per_cell() {
        let bytes = make_xlsx();
        let rows = read_melted(&bytes);
        // 2x2 table = 4 non-empty cells.
        assert_eq!(rows.len(), 4);
        let quads: std::vec::Vec<(std::string::String, i64, std::string::String, std::string::String)> =
            rows.iter()
                .map(|r| {
                    let rn = match &r[1] {
                        types::Duckvalue::Int64(i) => *i,
                        _ => panic!("row_no not int"),
                    };
                    (as_text(&r[0]), rn, as_text(&r[2]), as_text(&r[3]))
                })
                .collect();
        assert_eq!(quads[0], ("Sheet1".to_string(), 1, "A".to_string(), "a".to_string()));
        assert_eq!(quads[1], ("Sheet1".to_string(), 1, "B".to_string(), "b".to_string()));
        assert_eq!(quads[2], ("Sheet1".to_string(), 2, "A".to_string(), "1".to_string()));
        assert_eq!(quads[3], ("Sheet1".to_string(), 2, "B".to_string(), "2".to_string()));
    }

    #[test]
    fn cell_reads_single_value() {
        let bytes = make_xlsx();
        let rows = cell_rows(&bytes, "Sheet1", "B2");
        assert_eq!(rows.len(), 1);
        assert_eq!(as_text(&rows[0][0]), "2");
        // empty cell -> zero rows
        assert!(cell_rows(&bytes, "Sheet1", "Z99").is_empty());
        // unknown sheet -> zero rows
        assert!(cell_rows(&bytes, "Nope", "A1").is_empty());
        // bad address -> zero rows
        assert!(cell_rows(&bytes, "Sheet1", "not-a-cell").is_empty());
    }

    #[test]
    fn malformed_blob_is_empty_never_panics() {
        assert!(sheets_rows(b"not an xlsx file").is_empty());
        assert!(read_melted(b"not an xlsx file").is_empty());
        assert!(cell_rows(b"not an xlsx file", "Sheet1", "A1").is_empty());
        assert!(sheets_rows(b"").is_empty());
        assert!(read_melted(b"").is_empty());
    }

    #[test]
    fn col_letter_maps_correctly() {
        assert_eq!(col_letter(0), "A");
        assert_eq!(col_letter(25), "Z");
        assert_eq!(col_letter(26), "AA");
        assert_eq!(col_letter(27), "AB");
        assert_eq!(col_letter(701), "ZZ");
        assert_eq!(col_letter(702), "AAA");
    }

    #[test]
    fn parse_a1_roundtrips() {
        assert_eq!(parse_a1("A1"), Some((0, 0)));
        assert_eq!(parse_a1("B3"), Some((2, 1)));
        assert_eq!(parse_a1("AA12"), Some((11, 26)));
        assert_eq!(parse_a1("aa12"), Some((11, 26))); // case-insensitive
        assert_eq!(parse_a1("A0"), None); // row 0 invalid
        assert_eq!(parse_a1("1A"), None); // digits before letters
        assert_eq!(parse_a1("AB"), None); // no row
        assert_eq!(parse_a1("12"), None); // no col
    }

    #[test]
    fn hex_decode_roundtrips() {
        assert_eq!(hex_decode("00ff10").unwrap(), vec![0x00, 0xff, 0x10]);
        assert!(hex_decode("0").is_none()); // odd length
        assert!(hex_decode("zz").is_none()); // bad char
    }

    /// Helper to regenerate the hex blob embedded in smoke.sql. Ignored by
    /// default; run with:
    ///   cargo test --release dump_fixture_hex -- --ignored --nocapture
    #[test]
    #[ignore]
    fn dump_fixture_hex() {
        let bytes = make_xlsx();
        let mut s = std::string::String::with_capacity(bytes.len() * 2);
        for b in &bytes {
            s.push_str(&format!("{b:02x}"));
        }
        println!("XLSX_HEX={}", s);
    }
}
