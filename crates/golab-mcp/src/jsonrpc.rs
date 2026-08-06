//! JSON-RPC 2.0 over newline-delimited JSON.
//!
//! Hand-rolled rather than taken from a crate. The wire surface MCP needs is
//! seven methods and an envelope, and the alternative pulls an async runtime, a
//! schema-derivation macro layer and a pre-1.0 API into a workspace whose one
//! non-negotiable property is that `golab lease acquire` keeps building. A
//! breaking minor bump in a protocol crate should never be able to take the
//! lease path down with it.
//!
//! Framing: one JSON object per line, UTF-8, no length headers (that is LSP).
//! `serde_json::to_string` never emits a raw newline, so a line break is an
//! unambiguous message boundary in both directions.

use serde::Deserialize;
use serde_json::{json, Value};

/// A message from the client. `id` absent means notification — per the spec a
/// notification must never be answered, not even with an error.
#[derive(Debug, Clone, Deserialize)]
pub struct Incoming {
    #[serde(default)]
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl Incoming {
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

pub fn ok(id: Value, result: Value) -> String {
    line(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

pub fn err(id: Value, code: i64, message: &str) -> String {
    line(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    }))
}

pub fn notify(method: &str, params: Value) -> String {
    line(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
}

/// Serialize one frame. A frame that cannot be serialized is still a frame —
/// dropping it would hang a client that is waiting on the id, so fall back to
/// an internal error rather than to silence.
fn line(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|e| {
        let id = v.get("id").cloned().unwrap_or(Value::Null);
        format!(
            r#"{{"jsonrpc":"2.0","id":{},"error":{{"code":{INTERNAL_ERROR},"message":"unserializable response: {}"}}}}"#,
            id,
            e.to_string().replace('"', "'")
        )
    })
}

/// Parse one line. `Ok(None)` for a blank line, which is skipped rather than
/// treated as a parse error — some clients pad their output.
pub fn parse(raw: &str) -> Result<Option<Incoming>, String> {
    // A leading BOM is not valid JSON, but plenty of runtimes emit one on the
    // first write to a stream — Windows PowerShell's `StandardInput` does. The
    // resulting failure ("expected value at line 1 column 1", on a line that
    // looks perfectly fine) is baffling enough to be worth one `trim_start`.
    let trimmed = raw.trim_start_matches('\u{feff}').trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    match serde_json::from_str::<Incoming>(trimmed) {
        Ok(msg) => Ok(Some(msg)),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_and_a_notification_are_distinguishable() {
        let req = parse(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
            .unwrap()
            .unwrap();
        assert!(!req.is_notification());

        let note = parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .unwrap()
            .unwrap();
        assert!(
            note.is_notification(),
            "answering a notification is a protocol violation, so this has to be exact"
        );
    }

    #[test]
    fn missing_params_default_to_null_rather_than_failing() {
        let req = parse(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
            .unwrap()
            .unwrap();
        assert!(req.params.is_null());
    }

    #[test]
    fn blank_lines_are_skipped_and_junk_is_reported() {
        assert!(parse("   ").unwrap().is_none());
        assert!(parse("not json").is_err());
    }

    #[test]
    fn a_leading_bom_does_not_break_the_handshake() {
        // Windows PowerShell writes one on the first write to a stream, and
        // the resulting error points at column 1 of a line that reads fine.
        let framed = format!("\u{feff}{}", r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
        let msg = parse(&framed).unwrap().unwrap();
        assert_eq!(msg.method, "ping");
    }

    #[test]
    fn a_string_id_round_trips_unchanged() {
        // Clients are free to use string ids; echoing back a number would
        // leave them waiting forever.
        let req = parse(r#"{"jsonrpc":"2.0","id":"abc","method":"ping"}"#)
            .unwrap()
            .unwrap();
        let frame = ok(req.id.unwrap(), json!({}));
        let back: Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(back["id"], "abc");
    }

    #[test]
    fn every_frame_is_exactly_one_line() {
        // The whole transport rests on this: a newline is a message boundary.
        let frames = [
            ok(json!(1), json!({ "text": "a\nb\nc" })),
            err(json!(2), METHOD_NOT_FOUND, "no such method"),
            notify("notifications/resources/list_changed", json!({})),
        ];
        for f in frames {
            assert!(!f.contains('\n'), "frame carries a raw newline: {f}");
            let v: Value = serde_json::from_str(&f).unwrap();
            assert_eq!(v["jsonrpc"], "2.0");
        }
    }
}
