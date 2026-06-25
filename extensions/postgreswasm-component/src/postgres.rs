//! A MINIMAL, hand-rolled PostgreSQL v3 wire-protocol client in pure Rust.
//!
//! Plaintext only (no TLS) -- intended for localhost. Implements just enough of
//! the v3.0 startup handshake (trust / cleartext / MD5 auth) and simple-query
//! result-set parsing to back DuckDB's ATTACH storage interface. Never panics:
//! every failure is returned as `PgError` (mapped to a duckerror by the caller).
//!
//! NOTE: unlike MySQL, PostgreSQL ints on the wire are BIG-ENDIAN (network
//! order). Message framing is: 1 tag byte (except the untagged StartupMessage)
//! + int32 length (BE, INCLUDES the 4 length bytes) + payload.
//!
//! References: PostgreSQL "Frontend/Backend Protocol" (protocol version 3.0).

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};

const PROTOCOL_V3: i32 = 196608; // 3.0 = (3 << 16) | 0

/// A flat error string; the storage layer wraps it into a duckerror.
#[derive(Debug)]
pub struct PgError(pub String);

impl PgError {
    fn new(s: impl Into<String>) -> Self {
        PgError(s.into())
    }
}

impl From<std::io::Error> for PgError {
    fn from(e: std::io::Error) -> Self {
        PgError(format!("io: {e}"))
    }
}

type Result<T> = std::result::Result<T, PgError>;

/// Logical column type, mapped from the PostgreSQL type OID.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColType {
    Int,
    Float,
    Bool,
    Text,
}

#[derive(Clone, Debug)]
pub struct Column {
    pub name: String,
    pub ty: ColType,
}

/// A fully materialized result set. Each cell is `None` for SQL NULL.
#[derive(Debug)]
pub struct ResultSet {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Option<String>>>,
}

/// Map a PostgreSQL type OID to our logical column type.
///   20/21/23  = int8/int2/int4   -> Int
///   700/701/1700 = float4/float8/numeric -> Float
///   16 = bool -> Bool
///   else -> Text
pub fn classify_oid(oid: i32) -> ColType {
    match oid {
        20 | 21 | 23 => ColType::Int,
        700 | 701 | 1700 => ColType::Float,
        16 => ColType::Bool,
        _ => ColType::Text,
    }
}

/// An authenticated PostgreSQL connection.
pub struct PgConn {
    stream: TcpStream,
}

impl PgConn {
    /// Connect to `host:port`, perform the v3 startup handshake and authenticate
    /// (trust / cleartext / md5). SCRAM is explicitly unsupported.
    pub fn connect(
        host: &str,
        port: u16,
        user: &str,
        password: &str,
        database: &str,
    ) -> Result<PgConn> {
        // Dial a literal IP via a SocketAddr directly to skip name resolution:
        // the wasip2 std resolver path can fail with "thread constructor failed:
        // Not supported" in this component runtime. Only a real hostname falls
        // back to lookup.
        let stream = match host.parse::<IpAddr>() {
            Ok(ip) => TcpStream::connect(SocketAddr::new(ip, port)),
            Err(_) => TcpStream::connect((host, port)),
        }
        .map_err(|e| PgError::new(format!("connect {host}:{port}: {e}")))?;
        let mut conn = PgConn { stream };
        conn.startup(user, password, database)?;
        Ok(conn)
    }

    // ---- message framing --------------------------------------------------

    /// Read one tagged backend message: 1 tag byte + int32 length (BE, includes
    /// the 4 length bytes) + (length-4) payload bytes. Returns (tag, payload).
    fn read_message(&mut self) -> Result<(u8, Vec<u8>)> {
        read_message_from(&mut self.stream)
    }

    /// Write a tagged frontend message: tag + int32 length (BE, includes the 4
    /// length bytes) + payload.
    fn write_message(&mut self, tag: u8, payload: &[u8]) -> Result<()> {
        let total = (payload.len() + 4) as i32;
        self.stream.write_all(&[tag])?;
        self.stream.write_all(&total.to_be_bytes())?;
        self.stream.write_all(payload)?;
        self.stream.flush()?;
        Ok(())
    }

    /// Write the untagged StartupMessage: int32 total-length (BE, includes
    /// itself) + int32 protocol + key/value NUL-terminated pairs + trailing NUL.
    fn write_startup(&mut self, body: &[u8]) -> Result<()> {
        // body already contains: int32 protocol + params + trailing NUL.
        let total = (body.len() + 4) as i32;
        self.stream.write_all(&total.to_be_bytes())?;
        self.stream.write_all(body)?;
        self.stream.flush()?;
        Ok(())
    }

    // ---- startup / auth ---------------------------------------------------

    fn startup(&mut self, user: &str, password: &str, database: &str) -> Result<()> {
        let mut body = Vec::with_capacity(64);
        body.extend_from_slice(&PROTOCOL_V3.to_be_bytes());
        push_cstr(&mut body, "user");
        push_cstr(&mut body, user);
        if !database.is_empty() {
            push_cstr(&mut body, "database");
            push_cstr(&mut body, database);
        }
        body.push(0); // trailing NUL terminates the parameter list
        self.write_startup(&body)?;

        // Read the authentication exchange until AuthenticationOk.
        loop {
            let (tag, payload) = self.read_message()?;
            match tag {
                b'R' => {
                    let mut r = Reader::new(&payload);
                    let sub = r.i32()?;
                    match sub {
                        0 => return self.finish_startup(), // AuthenticationOk
                        3 => {
                            // cleartext password
                            let mut p = Vec::new();
                            push_cstr(&mut p, password);
                            self.write_message(b'p', &p)?;
                        }
                        5 => {
                            // MD5 password: payload has a 4-byte salt.
                            let salt = r.bytes(4)?;
                            let token = md5_auth(user, password, salt);
                            let mut p = Vec::new();
                            push_cstr(&mut p, &token);
                            self.write_message(b'p', &p)?;
                        }
                        10 => {
                            return Err(PgError::new(
                                "SCRAM auth not supported; use trust/md5",
                            ));
                        }
                        other => {
                            return Err(PgError::new(format!(
                                "unsupported authentication request {other}"
                            )));
                        }
                    }
                }
                b'E' => return Err(PgError::new(parse_error_response(&payload))),
                other => {
                    return Err(PgError::new(format!(
                        "unexpected message '{}' during auth",
                        other as char
                    )));
                }
            }
        }
    }

    /// After AuthenticationOk, drain ParameterStatus/BackendKeyData/etc. until
    /// the first ReadyForQuery ('Z').
    fn finish_startup(&mut self) -> Result<()> {
        loop {
            let (tag, payload) = self.read_message()?;
            match tag {
                b'Z' => return Ok(()), // ReadyForQuery
                b'S' | b'K' | b'N' => continue, // ParameterStatus/BackendKeyData/Notice
                b'E' => return Err(PgError::new(parse_error_response(&payload))),
                _ => continue, // ignore anything else benign
            }
        }
    }

    // ---- simple query -----------------------------------------------------

    /// Run a SQL string via the simple-query protocol and fully materialize its
    /// result set. A statement with no result set returns an empty `ResultSet`.
    pub fn query(&mut self, sql: &str) -> Result<ResultSet> {
        let mut p = Vec::with_capacity(sql.len() + 1);
        push_cstr(&mut p, sql);
        self.write_message(b'Q', &p)?;
        parse_query_response(&mut self.stream)
    }
}

/// Read one tagged backend message from any reader: 1 tag byte + int32 length
/// (BE, includes the 4 length bytes) + (length-4) payload bytes. Returns
/// (tag, payload). Factored out of `PgConn::read_message` so it can be driven
/// over an in-memory cursor (fuzzing). Never panics; truncation -> io error.
fn read_message_from<R: Read>(r: &mut R) -> Result<(u8, Vec<u8>)> {
    let mut tag = [0u8; 1];
    r.read_exact(&mut tag)?;
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = i32::from_be_bytes(len_buf);
    if len < 4 {
        return Err(PgError::new(format!("bad message length {len}")));
    }
    let payload_len = (len - 4) as usize;
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        r.read_exact(&mut payload)?;
    }
    Ok((tag[0], payload))
}

/// Drive the simple-query backend message loop over any reader, materializing
/// the result set. Factored out of `PgConn::query` so the BE framing +
/// RowDescription/DataRow/ErrorResponse parsing can be fuzzed over an in-memory
/// cursor of untrusted server bytes. Never panics.
pub fn parse_query_response<R: Read>(r: &mut R) -> Result<ResultSet> {
    let mut columns: Vec<Column> = Vec::new();
    let mut rows: Vec<Vec<Option<String>>> = Vec::new();
    let mut pending_err: Option<String> = None;

    loop {
        let (tag, payload) = read_message_from(r)?;
        match tag {
            b'T' => {
                // RowDescription.
                columns = parse_row_description(&payload)?;
            }
            b'D' => {
                // DataRow.
                rows.push(parse_data_row(&payload)?);
            }
            b'C' => {
                // CommandComplete -- a statement finished; keep reading (a
                // multi-statement string could follow), end at 'Z'.
            }
            b'E' => {
                // ErrorResponse: remember it, but keep reading until 'Z' so
                // the connection is left in a clean state.
                pending_err = Some(parse_error_response(&payload));
            }
            b'Z' => {
                // ReadyForQuery -- end of this query cycle.
                if let Some(msg) = pending_err {
                    return Err(PgError::new(msg));
                }
                return Ok(ResultSet { columns, rows });
            }
            b'I' => { /* EmptyQueryResponse */ }
            b'N' | b'S' => { /* NoticeResponse / ParameterStatus */ }
            _ => { /* ignore other benign async messages */ }
        }
    }
}

// ---- MD5 auth -------------------------------------------------------------

/// PostgreSQL MD5 auth token:
///   "md5" + hex(md5( hex(md5(password ++ user)) ++ salt ))
fn md5_auth(user: &str, password: &str, salt: &[u8]) -> String {
    let mut inner = Vec::new();
    inner.extend_from_slice(password.as_bytes());
    inner.extend_from_slice(user.as_bytes());
    let inner_hex = hex(&md5::compute(&inner).0);

    let mut outer = Vec::new();
    outer.extend_from_slice(inner_hex.as_bytes());
    outer.extend_from_slice(salt);
    let outer_hex = hex(&md5::compute(&outer).0);

    format!("md5{outer_hex}")
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

// ---- message helpers ------------------------------------------------------

/// Append a NUL-terminated string to a message buffer.
fn push_cstr(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(s.as_bytes());
    buf.push(0);
}

/// A linear, big-endian byte reader that never panics (out-of-range -> error).
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }
    fn need(&self, n: usize) -> Result<()> {
        // Checked add: `n` derived from an i32 length can be ~2 GiB, and on a
        // 32-bit target `self.pos + n` could wrap and spuriously pass.
        match self.pos.checked_add(n) {
            Some(end) if end <= self.buf.len() => Ok(()),
            _ => Err(PgError::new("truncated message")),
        }
    }
    fn i16(&mut self) -> Result<i16> {
        self.need(2)?;
        let v = i16::from_be_bytes([self.buf[self.pos], self.buf[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }
    fn i32(&mut self) -> Result<i32> {
        self.need(4)?;
        let s = &self.buf[self.pos..self.pos + 4];
        self.pos += 4;
        Ok(i32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        self.need(n)?;
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn null_str(&mut self) -> Result<String> {
        let start = self.pos;
        while self.pos < self.buf.len() && self.buf[self.pos] != 0 {
            self.pos += 1;
        }
        if self.pos >= self.buf.len() {
            return Err(PgError::new("unterminated NUL string"));
        }
        let s = String::from_utf8_lossy(&self.buf[start..self.pos]).into_owned();
        self.pos += 1; // consume NUL
        Ok(s)
    }
}

/// Parse a RowDescription ('T') message into columns.
///   int16 field-count, then per field:
///     name\0, table-oid int32, col-attnum int16, type-oid int32,
///     type-size int16, type-mod int32, format int16
pub fn parse_row_description(payload: &[u8]) -> Result<Vec<Column>> {
    let mut r = Reader::new(payload);
    let n = r.i16()?;
    if n < 0 {
        return Err(PgError::new("negative field count in RowDescription"));
    }
    let mut cols = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let name = r.null_str()?;
        let _table_oid = r.i32()?;
        let _col_attnum = r.i16()?;
        let type_oid = r.i32()?;
        let _type_size = r.i16()?;
        let _type_mod = r.i32()?;
        let _format = r.i16()?;
        cols.push(Column {
            name,
            ty: classify_oid(type_oid),
        });
    }
    Ok(cols)
}

/// Parse a DataRow ('D') message:
///   int16 col-count, then per col: int32 byte-length (-1 = NULL) + that many
///   bytes (the value in TEXT format).
pub fn parse_data_row(payload: &[u8]) -> Result<Vec<Option<String>>> {
    let mut r = Reader::new(payload);
    let n = r.i16()?;
    if n < 0 {
        return Err(PgError::new("negative column count in DataRow"));
    }
    let mut row = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let len = r.i32()?;
        if len < 0 {
            row.push(None);
        } else {
            let bytes = r.bytes(len as usize)?;
            row.push(Some(String::from_utf8_lossy(bytes).into_owned()));
        }
    }
    Ok(row)
}

/// Extract a human-readable message from an ErrorResponse ('E') message.
/// The payload is a sequence of (field-type byte, value\0) pairs terminated by
/// a single 0 byte. Field 'M' is the primary message; 'C' is the SQLSTATE.
pub fn parse_error_response(payload: &[u8]) -> String {
    let mut r = Reader::new(payload);
    let mut message = String::new();
    let mut code = String::new();
    loop {
        let field = match r.bytes(1) {
            Ok(b) if b[0] != 0 => b[0],
            _ => break, // 0 terminator or end
        };
        let val = match r.null_str() {
            Ok(v) => v,
            Err(_) => break,
        };
        match field {
            b'M' => message = val,
            b'C' => code = val,
            _ => {}
        }
    }
    if message.is_empty() {
        message = "unknown error".to_string();
    }
    if code.is_empty() {
        message
    } else {
        format!("[{code}] {message}")
    }
}

// ---------------------------------------------------------------------------
// Native unit tests for the pure protocol logic (no network).
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_oids() {
        assert_eq!(classify_oid(23), ColType::Int); // int4
        assert_eq!(classify_oid(20), ColType::Int); // int8
        assert_eq!(classify_oid(21), ColType::Int); // int2
        assert_eq!(classify_oid(700), ColType::Float); // float4
        assert_eq!(classify_oid(701), ColType::Float); // float8
        assert_eq!(classify_oid(1700), ColType::Float); // numeric
        assert_eq!(classify_oid(16), ColType::Bool); // bool
        assert_eq!(classify_oid(25), ColType::Text); // text
        assert_eq!(classify_oid(1043), ColType::Text); // varchar
    }

    #[test]
    fn hex_lowercase() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff, 0xa5]), "000fffa5");
    }

    #[test]
    fn md5_auth_matches_reference() {
        // md5_auth(user,pw,salt) = "md5" + md5( md5(pw++user) ++ salt ).
        // Cross-check against an independently computed value.
        let salt = [0x01u8, 0x02, 0x03, 0x04];
        let inner = format!("{:x}", md5::compute("secretpostgres"));
        let mut outer = inner.into_bytes();
        outer.extend_from_slice(&salt);
        let expected = format!("md5{:x}", md5::compute(&outer));
        assert_eq!(md5_auth("postgres", "secret", &salt), expected);
        assert!(md5_auth("postgres", "secret", &salt).starts_with("md5"));
        assert_eq!(md5_auth("postgres", "secret", &salt).len(), 35); // "md5" + 32 hex
    }

    #[test]
    fn be_int_helpers() {
        // i32 big-endian round trip via Reader.
        let buf = [0x00, 0x00, 0x01, 0x00, 0xff, 0xfe];
        let mut r = Reader::new(&buf);
        assert_eq!(r.i32().unwrap(), 256);
        assert_eq!(r.i16().unwrap(), -2);
        // truncation is an error, never a panic.
        let mut r2 = Reader::new(&[0x00]);
        assert!(r2.i32().is_err());
    }

    #[test]
    fn null_str_reads_until_nul() {
        let buf = b"name\0rest";
        let mut r = Reader::new(buf);
        assert_eq!(r.null_str().unwrap(), "name");
        // unterminated -> error.
        let buf2 = b"nope";
        let mut r2 = Reader::new(buf2);
        assert!(r2.null_str().is_err());
    }

    #[test]
    fn row_description_parses() {
        // 1 field "a", type-oid 23 (int4).
        let mut p = Vec::new();
        p.extend_from_slice(&1i16.to_be_bytes()); // field count
        push_cstr(&mut p, "a");
        p.extend_from_slice(&0i32.to_be_bytes()); // table oid
        p.extend_from_slice(&0i16.to_be_bytes()); // attnum
        p.extend_from_slice(&23i32.to_be_bytes()); // type oid int4
        p.extend_from_slice(&4i16.to_be_bytes()); // type size
        p.extend_from_slice(&(-1i32).to_be_bytes()); // type mod
        p.extend_from_slice(&0i16.to_be_bytes()); // format text
        let cols = parse_row_description(&p).unwrap();
        assert_eq!(cols.len(), 1);
        assert_eq!(cols[0].name, "a");
        assert_eq!(cols[0].ty, ColType::Int);
    }

    #[test]
    fn data_row_parses_with_null() {
        // 2 cols: "x", NULL.
        let mut p = Vec::new();
        p.extend_from_slice(&2i16.to_be_bytes());
        p.extend_from_slice(&1i32.to_be_bytes()); // len 1
        p.push(b'x');
        p.extend_from_slice(&(-1i32).to_be_bytes()); // NULL
        let row = parse_data_row(&p).unwrap();
        assert_eq!(row, vec![Some("x".to_string()), None]);
    }

    #[test]
    fn error_response_parses_message_and_code() {
        // fields: S='ERROR', C='42P01', M='relation does not exist', then 0.
        let mut p = Vec::new();
        p.push(b'S');
        p.extend_from_slice(b"ERROR\0");
        p.push(b'C');
        p.extend_from_slice(b"42P01\0");
        p.push(b'M');
        p.extend_from_slice(b"relation does not exist\0");
        p.push(0);
        let msg = parse_error_response(&p);
        assert_eq!(msg, "[42P01] relation does not exist");
    }

    // ---- fuzz regressions (cargo-fuzz; fuzz/fuzz_targets/postgres_parse.rs) --

    /// A DataRow claiming a column byte-length near i32::MAX must not let
    /// `Reader::need` overflow `pos + n` (a panic in overflow-checked builds, a
    /// wrapping OOB-pass in release). The checked_add fix returns a truncation
    /// error. (On 32-bit wasm, `len as usize` is ~2 GiB and `pos + n` wraps.)
    #[test]
    fn data_row_huge_length_does_not_overflow() {
        use std::io::Cursor;
        let mut p = Vec::new();
        p.extend_from_slice(&1i16.to_be_bytes()); // 1 column
        p.extend_from_slice(&i32::MAX.to_be_bytes()); // claimed length ~2 GiB
        // no payload follows -> must error, never panic / never allocate 2 GiB.
        assert!(parse_data_row(&p).is_err());

        // Same payload driven through the full BE message loop as a 'D' message.
        let mut msg = Vec::new();
        msg.push(b'D');
        let total = (p.len() + 4) as i32;
        msg.extend_from_slice(&total.to_be_bytes());
        msg.extend_from_slice(&p);
        let mut cur = Cursor::new(&msg[..]);
        assert!(parse_query_response(&mut cur).is_err());
    }

    /// Truncated / negative-length frames are graceful (no panic).
    #[test]
    fn truncated_and_negative_frames_are_graceful() {
        use std::io::Cursor;
        // message length < 4 is rejected, not used to size a vec.
        let mut bad = Vec::new();
        bad.push(b'T');
        bad.extend_from_slice(&1i32.to_be_bytes()); // claims length 1 (< 4)
        let mut c = Cursor::new(&bad[..]);
        assert!(parse_query_response(&mut c).is_err());

        // Empty payloads to every parser: error, never panic.
        assert!(parse_row_description(&[]).is_err());
        assert!(parse_data_row(&[]).is_err());
        let _ = parse_error_response(&[]); // returns "unknown error"
    }
}
