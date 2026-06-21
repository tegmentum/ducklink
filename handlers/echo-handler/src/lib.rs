//! Reference duckdb-wasm-httpd request handler.
//!
//! Implements the `duckdb:handler/request-handler` world: receives the
//! JSON-encoded HTTP request as a single string and returns a response. This
//! one echoes a one-line summary as `text/plain`, small enough to read
//! end-to-end as a contract reference. The native host (`ducklink serve
//! --load echo=echo_handler.wasm`) instantiates it fresh per request and
//! dispatches routes with `kind='wasm'` to it.

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:handler/request-handler",
});

use exports::duckdb::handler::handler::Guest;

struct EchoHandler;

impl Guest for EchoHandler {
    fn handle(request: String) -> Result<String, String> {
        // The request is the JSON blob the dispatcher built. Echo a summary +
        // the raw request back, and set text/plain via the structured-response
        // shape the host understands ({status, ctype, body}).
        let summary = format!("echo: {} bytes\n{}", request.len(), request);
        let mut out = String::from("{\"status\":200,\"ctype\":\"text/plain; charset=utf-8\",\"body\":");
        json_string(&mut out, &summary);
        out.push('}');
        Ok(out)
    }
}

fn json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

export!(EchoHandler);
