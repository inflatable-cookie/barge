//! The CLI's one output channel: the JSON document envelope and the error
//! document, both shaped exactly as `docs/knowledge/contracts/consumption.md`
//! says.

use std::io::Write;

use barge::{Error, ErrorKind, Result};
use serde_json::{json, Value};

/// Print one success document: `{"schema": ..., "data": {...}}` on stdout.
pub(crate) fn document(schema: &str, fields: Vec<(&str, Value)>) -> Result<()> {
    let mut data = serde_json::Map::new();
    for (name, value) in fields {
        data.insert(name.to_string(), value);
    }
    let document = json!({ "schema": schema, "data": Value::Object(data) });
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{document}")
        .map_err(|source| Error::new(ErrorKind::Io, format!("cannot write to stdout: {source}")))?;
    Ok(())
}

/// Print one error document on stderr, in JSON or in the human summary. Errors
/// never go to stdout, so a consumer reads exactly one stream per invocation.
pub(crate) fn report_error(error: &Error, json: bool) {
    let mut stderr = std::io::stderr().lock();
    if json {
        let document = json!({
            "schema": "barge.error/1",
            "error": { "kind": error.kind().as_str(), "message": error.message() },
        });
        let _ = writeln!(stderr, "{document}");
    } else {
        let _ = writeln!(stderr, "barge: {}", error.message());
    }
}
