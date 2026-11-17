use std::time::Duration;

const MCP_DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

pub enum McpReply {
    Json(String),
    Accepted,
}

pub trait McpTools: Send + Sync {
    fn server_name(&self) -> &str;
    fn definitions(&self) -> serde_json::Value;
    fn call(
        &self,
        call_key: &str,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, String>;
    fn cancel(&self, call_key: &str);
}

fn mcp_error_reply(id: serde_json::Value, code: i64, message: &str) -> McpReply {
    McpReply::Json(
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message}
        })
        .to_string(),
    )
}

fn mcp_result_reply(id: serde_json::Value, result: serde_json::Value) -> McpReply {
    McpReply::Json(serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string())
}

fn mcp_request_id_text(id: &serde_json::Value) -> String {
    match id {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

pub fn required_str<'a>(arguments: &'a serde_json::Value, key: &str) -> Result<&'a str, String> {
    arguments
        .get(key)
        .and_then(|value| value.as_str())
        .ok_or_else(|| format!("missing or invalid argument: {key}"))
}

pub fn optional_str<'a>(arguments: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    arguments.get(key).and_then(|value| value.as_str())
}

pub fn optional_u64(arguments: &serde_json::Value, key: &str) -> Option<u64> {
    arguments.get(key).and_then(|value| value.as_u64())
}

pub fn flag(arguments: &serde_json::Value, key: &str) -> bool {
    arguments
        .get(key)
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

/// Returns the JSON-RPC method and whether the message carries an id.
pub fn mcp_request_method(body: &[u8]) -> (Option<String>, bool) {
    let Ok(message) = serde_json::from_slice::<serde_json::Value>(body) else {
        return (None, false);
    };
    let method = message
        .get("method")
        .and_then(|method| method.as_str())
        .map(str::to_string);
    (method, message.get("id").is_some())
}

pub fn accepts_event_stream(accept_header: Option<&str>, http_version: (u8, u8)) -> bool {
    accept_header.is_some_and(|accept| {
        accept.split(',').any(|media_type| {
            media_type.split(';').next().is_some_and(|media_type| {
                media_type.trim().eq_ignore_ascii_case("text/event-stream")
            })
        })
    }) && http_version >= (1, 1)
}

/// `caller_key` identifies one MCP session of one caller: JSON-RPC ids
/// restart at 1 in each chat, so a call's cancel key is
/// `{caller_key}:{request id}`.
pub fn handle_mcp_message(tools: &dyn McpTools, caller_key: &str, body: &[u8]) -> McpReply {
    use serde_json::{Value, json};

    let message: Value = match serde_json::from_slice(body) {
        Ok(message) => message,
        Err(error) => {
            return mcp_error_reply(Value::Null, -32700, &format!("parse error: {error}"));
        }
    };
    let method = message
        .get("method")
        .and_then(|method| method.as_str())
        .unwrap_or("");
    let Some(id) = message.get("id").cloned() else {
        if method == "notifications/cancelled" {
            if let Some(request_id) = message.pointer("/params/requestId") {
                tools.cancel(&format!("{caller_key}:{}", mcp_request_id_text(request_id)));
            }
        }
        return McpReply::Accepted;
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => {
            let protocol_version = params
                .get("protocolVersion")
                .and_then(|version| version.as_str())
                .unwrap_or(MCP_DEFAULT_PROTOCOL_VERSION);
            mcp_result_reply(
                id,
                json!({
                    "protocolVersion": protocol_version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": tools.server_name(), "version": "1"}
                }),
            )
        }
        "ping" => mcp_result_reply(id, json!({})),
        "tools/list" => mcp_result_reply(id, json!({"tools": tools.definitions()})),
        "tools/call" => {
            let Some(name) = params.get("name").and_then(|name| name.as_str()) else {
                return mcp_error_reply(id, -32602, "tools/call requires a tool name");
            };
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            let call_key = format!("{caller_key}:{}", mcp_request_id_text(&id));
            let (text, is_error) = match tools.call(&call_key, name, &arguments) {
                Ok(text) => (text, false),
                Err(error) => (error, true),
            };
            mcp_result_reply(
                id,
                json!({"content": [{"type": "text", "text": text}], "isError": is_error}),
            )
        }
        other => mcp_error_reply(id, -32601, &format!("method not found: {other}")),
    }
}

/// Writes a chunked server-sent event response by hand, since tiny_http
/// buffers a streamed body and would hold back the headers and the
/// keep-alive comments.
pub fn write_event_stream(
    writer: &mut dyn std::io::Write,
    reply: &std::sync::mpsc::Receiver<String>,
    keep_alive: Duration,
) -> std::io::Result<()> {
    use std::sync::mpsc::RecvTimeoutError;

    writer.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\
          Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
    )?;
    writer.flush()?;
    loop {
        match reply.recv_timeout(keep_alive) {
            Ok(json) => {
                write_chunk(
                    writer,
                    format!("event: message\ndata: {json}\n\n").as_bytes(),
                )?;
                break;
            }
            Err(RecvTimeoutError::Timeout) => write_chunk(writer, b": keep-alive\n\n")?,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    writer.write_all(b"0\r\n\r\n")?;
    writer.flush()
}

fn write_chunk(writer: &mut dyn std::io::Write, data: &[u8]) -> std::io::Result<()> {
    write!(writer, "{:x}\r\n", data.len())?;
    writer.write_all(data)?;
    writer.write_all(b"\r\n")?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_stream_sends_keep_alives_until_the_reply() -> Result<(), String> {
        let (sender, receiver) = std::sync::mpsc::channel();
        let reply = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            sender.send(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_string())
        });
        let mut written = Vec::new();

        write_event_stream(&mut written, &receiver, Duration::from_millis(50))
            .map_err(|err| err.to_string())?;

        reply
            .join()
            .map_err(|_| "The reply thread panicked".to_string())?
            .map_err(|err| err.to_string())?;
        let written = String::from_utf8(written).map_err(|err| err.to_string())?;
        let (head, body) = written
            .split_once("\r\n\r\n")
            .ok_or_else(|| format!("No end of headers in {written:?}"))?;
        assert!(head.contains("Transfer-Encoding: chunked"), "{head}");
        assert!(body.contains(": keep-alive\n\n"), "{body}");
        assert!(
            body.contains("event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n"),
            "{body}"
        );
        assert!(body.ends_with("0\r\n\r\n"), "{body}");
        Ok(())
    }
}
