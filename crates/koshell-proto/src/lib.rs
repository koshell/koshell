//! Shared IPC message types for the koshell terminal process (`koshell-rs`) and
//! the Node AI daemon (`koshell-ai-daemon`).
//!
//! The wire format is newline-delimited JSON (JSONL) over a Unix domain socket.
//! Each message is a single JSON object tagged by a `type` field. Both ends must
//! agree on [`PROTOCOL_VERSION`], negotiated during the `hello` handshake.

use serde::{Deserialize, Serialize};

/// IPC protocol version. Bump on any breaking change to the message shapes.
pub const PROTOCOL_VERSION: u32 = 1;

/// A message sent from the terminal process to the AI daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// First message on a new connection; establishes the terminal session.
    Hello {
        protocol_version: u32,
        terminal_session_id: String,
        cwd: String,
        shell: String,
        rows: u16,
        cols: u16,
    },
    /// A `#?` question raised in the terminal, with the terminal context the
    /// daemon should ground its answer in.
    ///
    /// `context_package` is treated as opaque JSON on the wire. `koshell-rs`
    /// assembles it from its own terminal facts; the daemon consumes it. Keeping
    /// it as [`serde_json::Value`] avoids coupling the proto crate to the
    /// terminal-core context types while the shape is still stabilizing.
    AiRequest {
        request_id: String,
        question: String,
        trigger: String,
        context_package: serde_json::Value,
    },
    /// Response to a daemon-initiated context tool call. Reserved for the next
    /// stage (tool round-trips); not produced yet.
    ToolResponse {
        request_id: String,
        tool_call_id: String,
        result: serde_json::Value,
    },
    /// Graceful shutdown of a terminal session.
    Bye { terminal_session_id: String },
}

/// A message sent from the AI daemon back to the terminal process.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Acknowledges receipt of an [`ClientMessage::AiRequest`]. This is the only
    /// server message implemented in the current stage; `ai_delta`,
    /// `ai_tool_call`, `ai_response_end`, and `ai_error` arrive with pi.
    Ack { request_id: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_request_round_trips_as_tagged_json() {
        let msg = ClientMessage::AiRequest {
            request_id: "req-1".into(),
            question: "explain this output".into(),
            trigger: "#?".into(),
            context_package: serde_json::json!({ "primaryText": "hello" }),
        };
        let line = serde_json::to_string(&msg).unwrap();
        assert!(line.contains("\"type\":\"ai_request\""));
        let back: ClientMessage = serde_json::from_str(&line).unwrap();
        match back {
            ClientMessage::AiRequest { request_id, .. } => assert_eq!(request_id, "req-1"),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn ack_round_trips() {
        let line = serde_json::to_string(&ServerMessage::Ack {
            request_id: "req-1".into(),
        })
        .unwrap();
        assert_eq!(line, r#"{"type":"ack","request_id":"req-1"}"#);
    }
}
