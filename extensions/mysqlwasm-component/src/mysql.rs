//! A MINIMAL, hand-rolled MySQL/MariaDB wire-protocol client in pure Rust.
//!
//! Plaintext only (no TLS) -- intended for localhost. Implements just enough of
//! the protocol-41 handshake (mysql_native_password) and COM_QUERY result-set
//! parsing to back DuckDB's ATTACH storage interface. Never panics: every
//! failure is returned as `MyError` (mapped to a duckerror by the caller).
//!
//! References: MySQL "Client/Server Protocol" packet formats.

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};

use sha1::{Digest, Sha1};

// ---- capability flags (subset we advertise) -------------------------------
const CLIENT_LONG_PASSWORD: u32 = 0x0000_0001;
const CLIENT_LONG_FLAG: u32 = 0x0000_0004;
const CLIENT_CONNECT_WITH_DB: u32 = 0x0000_0008;
const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
const CLIENT_PLUGIN_AUTH: u32 = 0x0008_0000;

const COM_QUERY: u8 = 0x03;

/// A flat error string; the storage layer wraps it into a duckerror.
#[derive(Debug)]
pub struct MyError(pub String);

impl MyError {
    fn new(s: impl Into<String>) -> Self {
        MyError(s.into())
    }
}

impl From<std::io::Error> for MyError {
    fn from(e: std::io::Error) -> Self {
        MyError(format!("io: {e}"))
    }
}

type Result<T> = std::result::Result<T, MyError>;

/// Logical column type, mapped from the MySQL field type byte.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColType {
    Int,
    Float,
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

/// MySQL field type bytes (only the ones we classify).
mod field_type {
    pub const DECIMAL: u8 = 0x00;
    pub const TINY: u8 = 0x01;
    pub const SHORT: u8 = 0x02;
    pub const LONG: u8 = 0x03;
    pub const FLOAT: u8 = 0x04;
    pub const DOUBLE: u8 = 0x05;
    pub const LONGLONG: u8 = 0x08;
    pub const INT24: u8 = 0x09;
    pub const YEAR: u8 = 0x0d;
    pub const NEWDECIMAL: u8 = 0xf6;
}

fn classify(type_byte: u8) -> ColType {
    use field_type::*;
    match type_byte {
        TINY | SHORT | LONG | LONGLONG | INT24 | YEAR => ColType::Int,
        FLOAT | DOUBLE | DECIMAL | NEWDECIMAL => ColType::Float,
        _ => ColType::Text,
    }
}

/// An authenticated MySQL connection.
pub struct MyConn {
    stream: TcpStream,
}

impl MyConn {
    /// Connect to `host:port`, perform the protocol-41 handshake with
    /// mysql_native_password and select `database`.
    pub fn connect(
        host: &str,
        port: u16,
        user: &str,
        password: &str,
        database: &str,
    ) -> Result<MyConn> {
        // Connect WITHOUT going through `ToSocketAddrs` for a literal IP: std's
        // `(&str, u16)` impl always calls `lookup_host`, and the wasip2 std
        // resolver path can fail with "thread constructor failed: Not supported"
        // in this component runtime. A literal IP is dialed via a SocketAddr
        // directly; only a real hostname falls back to name resolution.
        let stream = match host.parse::<IpAddr>() {
            Ok(ip) => TcpStream::connect(SocketAddr::new(ip, port)),
            Err(_) => TcpStream::connect((host, port)),
        }
        .map_err(|e| MyError::new(format!("connect {host}:{port}: {e}")))?;
        let mut conn = MyConn { stream };
        conn.handshake(user, password, database)?;
        Ok(conn)
    }

    // ---- packet framing ---------------------------------------------------

    /// Read one logical packet payload (handles the 3-byte length + 1-byte
    /// sequence id header; does NOT reassemble 16MB-spanning packets, which the
    /// tiny result sets here never produce). Returns (payload, seq).
    fn read_packet(&mut self) -> Result<(Vec<u8>, u8)> {
        let mut header = [0u8; 4];
        self.stream.read_exact(&mut header)?;
        let len = (header[0] as usize) | (header[1] as usize) << 8 | (header[2] as usize) << 16;
        let seq = header[3];
        let mut payload = vec![0u8; len];
        if len > 0 {
            self.stream.read_exact(&mut payload)?;
        }
        Ok((payload, seq))
    }

    /// Write one packet payload with the given sequence id.
    fn write_packet(&mut self, seq: u8, payload: &[u8]) -> Result<()> {
        let len = payload.len();
        if len >= 0x00ff_ffff {
            return Err(MyError::new("packet too large"));
        }
        let header = [len as u8, (len >> 8) as u8, (len >> 16) as u8, seq];
        self.stream.write_all(&header)?;
        self.stream.write_all(payload)?;
        self.stream.flush()?;
        Ok(())
    }

    // ---- handshake --------------------------------------------------------

    fn handshake(&mut self, user: &str, password: &str, database: &str) -> Result<()> {
        let (payload, _seq) = self.read_packet()?;
        let mut r = Reader::new(&payload);

        let protocol = r.u8()?;
        if protocol == 0xff {
            return Err(MyError::new(format!(
                "server refused connection: {}",
                parse_err_packet(&payload)
            )));
        }
        if protocol != 10 {
            return Err(MyError::new(format!(
                "unsupported handshake protocol version {protocol}"
            )));
        }
        let _server_version = r.null_str()?; // human-readable version
        let _thread_id = r.u32()?;
        // auth-plugin-data-part-1: 8 bytes of salt.
        let mut salt = Vec::with_capacity(20);
        salt.extend_from_slice(r.bytes(8)?);
        let _filler = r.u8()?; // 0x00
        let cap_lower = r.u16()? as u32;
        // The remainder is optional but always present on modern servers.
        let mut auth_plugin_name = String::from("mysql_native_password");
        if r.remaining() > 0 {
            let _charset = r.u8()?;
            let _status = r.u16()?;
            let cap_upper = r.u16()? as u32;
            let capabilities = cap_lower | (cap_upper << 16);
            let auth_data_len = r.u8()?;
            r.skip(10)?; // reserved
            if capabilities & CLIENT_SECURE_CONNECTION != 0 {
                // auth-plugin-data-part-2: at least 13 bytes; we take 12 (the
                // 13th is a trailing NUL). 8 + 12 = a 20-byte salt.
                let take = (auth_data_len as i32 - 8).max(13) as usize;
                let part2 = r.bytes(take)?;
                salt.extend_from_slice(&part2[..12.min(part2.len())]);
            }
            if capabilities & CLIENT_PLUGIN_AUTH != 0 && r.remaining() > 0 {
                auth_plugin_name = r.null_str()?;
            }
        }

        if auth_plugin_name != "mysql_native_password" {
            return Err(MyError::new(format!(
                "unsupported auth plugin '{auth_plugin_name}' (only mysql_native_password)"
            )));
        }
        if salt.len() < 20 {
            return Err(MyError::new(format!(
                "short auth salt ({} bytes)",
                salt.len()
            )));
        }
        let salt = &salt[..20];

        let auth_response = native_password_auth(password.as_bytes(), salt);
        let resp = self.build_handshake_response(user, &auth_response, database);
        self.write_packet(1, &resp)?;

        let (reply, _seq) = self.read_packet()?;
        match reply.first().copied() {
            Some(0x00) => Ok(()),                        // OK
            Some(0xfe) => Ok(()), // old EOF / OK (no auth-switch handled here)
            Some(0xff) => Err(MyError::new(format!(
                "authentication failed: {}",
                parse_err_packet(&reply)
            ))),
            other => Err(MyError::new(format!(
                "unexpected handshake reply header {other:?}"
            ))),
        }
    }

    fn build_handshake_response(
        &self,
        user: &str,
        auth_response: &[u8],
        database: &str,
    ) -> Vec<u8> {
        let capabilities = CLIENT_PROTOCOL_41
            | CLIENT_SECURE_CONNECTION
            | CLIENT_PLUGIN_AUTH
            | CLIENT_CONNECT_WITH_DB
            | CLIENT_LONG_PASSWORD
            | CLIENT_LONG_FLAG;
        let mut p = Vec::with_capacity(64);
        p.extend_from_slice(&capabilities.to_le_bytes());
        p.extend_from_slice(&(0x0100_0000u32).to_le_bytes()); // max packet 16MB
        p.push(45); // charset utf8mb4_general_ci (any utf8 works)
        p.extend_from_slice(&[0u8; 23]); // reserved
        // username, NUL-terminated.
        p.extend_from_slice(user.as_bytes());
        p.push(0);
        // length-prefixed auth response (secure connection: 1-byte length).
        p.push(auth_response.len() as u8);
        p.extend_from_slice(auth_response);
        // default database, NUL-terminated.
        p.extend_from_slice(database.as_bytes());
        p.push(0);
        // auth plugin name, NUL-terminated.
        p.extend_from_slice(b"mysql_native_password");
        p.push(0);
        p
    }

    // ---- COM_QUERY --------------------------------------------------------

    /// Run a query and fully materialize its result set. A query that yields no
    /// result set (OK packet) returns an empty `ResultSet`.
    pub fn query(&mut self, sql: &str) -> Result<ResultSet> {
        let mut payload = Vec::with_capacity(1 + sql.len());
        payload.push(COM_QUERY);
        payload.extend_from_slice(sql.as_bytes());
        self.write_packet(0, &payload)?;

        let (first, _seq) = self.read_packet()?;
        match first.first().copied() {
            Some(0x00) => {
                // OK packet: no result set (e.g. a DDL/DML statement).
                return Ok(ResultSet {
                    columns: Vec::new(),
                    rows: Vec::new(),
                });
            }
            Some(0xff) => {
                return Err(MyError::new(format!(
                    "query error: {}",
                    parse_err_packet(&first)
                )));
            }
            _ => {}
        }
        // Otherwise `first` is a length-encoded column count.
        let mut r = Reader::new(&first);
        let ncols = r
            .lenenc_int()?
            .ok_or_else(|| MyError::new("expected column count, got NULL"))?
            as usize;

        // Column definition packets.
        let mut columns = Vec::with_capacity(ncols);
        for _ in 0..ncols {
            let (pkt, _s) = self.read_packet()?;
            columns.push(parse_column_def(&pkt)?);
        }

        // Some servers send an EOF after the column defs (when CLIENT_DEPRECATE_EOF
        // is off, which it is for us). Consume it if present.
        let (mut pkt, _s) = self.read_packet()?;
        if !is_eof(&pkt) {
            // No EOF; `pkt` is already the first row. Fall through.
        } else {
            let (p, _s) = self.read_packet()?;
            pkt = p;
        }

        // Rows until EOF / OK.
        let mut rows = Vec::new();
        loop {
            if is_eof(&pkt) {
                break;
            }
            if pkt.first().copied() == Some(0xff) {
                return Err(MyError::new(format!(
                    "error mid result set: {}",
                    parse_err_packet(&pkt)
                )));
            }
            rows.push(parse_text_row(&pkt, ncols)?);
            let (p, _s) = self.read_packet()?;
            pkt = p;
        }

        Ok(ResultSet { columns, rows })
    }
}

// ---- mysql_native_password ------------------------------------------------

/// auth = SHA1(password) XOR SHA1( salt ++ SHA1(SHA1(password)) ).
/// An empty password yields an empty response.
fn native_password_auth(password: &[u8], salt: &[u8]) -> Vec<u8> {
    if password.is_empty() {
        return Vec::new();
    }
    let sha_pw = sha1(password);
    let sha_sha_pw = sha1(&sha_pw);
    let mut concat = Vec::with_capacity(salt.len() + sha_sha_pw.len());
    concat.extend_from_slice(salt);
    concat.extend_from_slice(&sha_sha_pw);
    let token = sha1(&concat);
    sha_pw.iter().zip(token.iter()).map(|(a, b)| a ^ b).collect()
}

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h = Sha1::new();
    h.update(data);
    h.finalize().into()
}

// ---- packet parsing helpers -----------------------------------------------

/// A linear byte reader that never panics (out-of-range -> error).
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }
    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }
    fn need(&self, n: usize) -> Result<()> {
        if self.pos + n > self.buf.len() {
            return Err(MyError::new("truncated packet"));
        }
        Ok(())
    }
    fn u8(&mut self) -> Result<u8> {
        self.need(1)?;
        let v = self.buf[self.pos];
        self.pos += 1;
        Ok(v)
    }
    fn u16(&mut self) -> Result<u16> {
        self.need(2)?;
        let v = self.buf[self.pos] as u16 | (self.buf[self.pos + 1] as u16) << 8;
        self.pos += 2;
        Ok(v)
    }
    fn u32(&mut self) -> Result<u32> {
        self.need(4)?;
        let s = &self.buf[self.pos..self.pos + 4];
        self.pos += 4;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        self.need(n)?;
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn skip(&mut self, n: usize) -> Result<()> {
        self.need(n)?;
        self.pos += n;
        Ok(())
    }
    fn null_str(&mut self) -> Result<String> {
        let start = self.pos;
        while self.pos < self.buf.len() && self.buf[self.pos] != 0 {
            self.pos += 1;
        }
        if self.pos >= self.buf.len() {
            return Err(MyError::new("unterminated NUL string"));
        }
        let s = String::from_utf8_lossy(&self.buf[start..self.pos]).into_owned();
        self.pos += 1; // consume NUL
        Ok(s)
    }
    /// Length-encoded integer: 0xfb=NULL, 0xfc=u16, 0xfd=u24, 0xfe=u64, else the
    /// byte itself. Returns None for NULL (0xfb).
    fn lenenc_int(&mut self) -> Result<Option<u64>> {
        let first = self.u8()?;
        let v = match first {
            0xfb => return Ok(None),
            0xfc => self.u16()? as u64,
            0xfd => {
                let s = self.bytes(3)?;
                s[0] as u64 | (s[1] as u64) << 8 | (s[2] as u64) << 16
            }
            0xfe => {
                let s = self.bytes(8)?;
                let mut v = 0u64;
                for (i, b) in s.iter().enumerate() {
                    v |= (*b as u64) << (8 * i);
                }
                v
            }
            n => n as u64,
        };
        Ok(Some(v))
    }
    /// Length-encoded string. Returns None for SQL NULL.
    fn lenenc_str(&mut self) -> Result<Option<String>> {
        match self.lenenc_int()? {
            None => Ok(None),
            Some(len) => {
                let s = self.bytes(len as usize)?;
                Ok(Some(String::from_utf8_lossy(s).into_owned()))
            }
        }
    }
    /// Skip a length-encoded string (don't decode the bytes).
    fn skip_lenenc_str(&mut self) -> Result<()> {
        match self.lenenc_int()? {
            None => Ok(()),
            Some(len) => self.skip(len as usize),
        }
    }
}

/// A packet is EOF if it starts with 0xfe and is shorter than 9 bytes.
fn is_eof(pkt: &[u8]) -> bool {
    pkt.first().copied() == Some(0xfe) && pkt.len() < 9
}

/// Parse a Column Definition (protocol 41) packet -> (name, type).
fn parse_column_def(pkt: &[u8]) -> Result<Column> {
    let mut r = Reader::new(pkt);
    r.skip_lenenc_str()?; // catalog ("def")
    r.skip_lenenc_str()?; // schema
    r.skip_lenenc_str()?; // table (alias)
    r.skip_lenenc_str()?; // org_table
    let name = r
        .lenenc_str()?
        .ok_or_else(|| MyError::new("column name was NULL"))?;
    r.skip_lenenc_str()?; // org_name
    let _len_of_fixed = r.lenenc_int()?; // 0x0c
    let _charset = r.u16()?;
    let _column_length = r.u32()?;
    let type_byte = r.u8()?;
    let _flags = r.u16()?;
    let _decimals = r.u8()?;
    Ok(Column {
        name,
        ty: classify(type_byte),
    })
}

/// Parse a text-protocol result row: `ncols` length-encoded strings (NULL=0xfb).
fn parse_text_row(pkt: &[u8], ncols: usize) -> Result<Vec<Option<String>>> {
    let mut r = Reader::new(pkt);
    let mut row = Vec::with_capacity(ncols);
    for _ in 0..ncols {
        row.push(r.lenenc_str()?);
    }
    Ok(row)
}

/// Extract the human-readable message from an ERR packet (0xff header).
fn parse_err_packet(pkt: &[u8]) -> String {
    // 0xff, error_code(2), [sql_state_marker '#' + sql_state(5)], message...
    if pkt.len() < 3 {
        return "unknown error".to_string();
    }
    let code = pkt[1] as u16 | (pkt[2] as u16) << 8;
    let mut idx = 3;
    if pkt.len() > 3 && pkt[3] == b'#' {
        idx = 9.min(pkt.len()); // skip '#' + 5-byte sql_state
    }
    let msg = String::from_utf8_lossy(&pkt[idx..]).into_owned();
    format!("[{code}] {msg}")
}
