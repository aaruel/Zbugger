//! Debug Adapter Protocol framing: JSON messages with a `Content-Length` header.
//! <https://microsoft.github.io/debug-adapter-protocol/overview>

use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

/// Read one message. `Ok(None)` at end of stream.
pub fn read_message(reader: &mut impl BufRead) -> io::Result<Option<Value>> {
    let mut length = None;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            if length.is_some() {
                break;
            }
            continue; // stray blank line between messages
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.trim().eq_ignore_ascii_case("Content-Length") {
                length = value.trim().parse::<usize>().ok();
            }
        }
    }
    let mut body = vec![0; length.unwrap_or(0)];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Write one message.
pub fn write_message(writer: &mut impl Write, message: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(message)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()
}

/// A request message.
pub fn request(seq: i64, command: &str, arguments: Value) -> Value {
    json!({ "seq": seq, "type": "request", "command": command, "arguments": arguments })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut bytes = Vec::new();
        write_message(&mut bytes, &request(1, "initialize", json!({ "adapterID": "lldb-dap" }))).unwrap();
        write_message(&mut bytes, &json!({ "type": "event", "event": "initialized" })).unwrap();
        let mut reader = io::Cursor::new(bytes);
        let first = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(first["command"], "initialize");
        assert_eq!(first["arguments"]["adapterID"], "lldb-dap");
        assert_eq!(read_message(&mut reader).unwrap().unwrap()["event"], "initialized");
        assert!(read_message(&mut reader).unwrap().is_none());
    }
}
