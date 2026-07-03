//! Shared IPC message types for the koshell terminal process (`koshell-rs`) and
//! the Node AI daemon (`koshell-ai-daemon`).
//!
//! The wire format is newline-delimited JSON (JSONL) over a Unix domain socket.
//! Each message is a single JSON object tagged by a `type` field. Both ends must
//! agree on [`PROTOCOL_VERSION`], negotiated during the `hello` handshake: the
//! daemon serves `ai_request`s only after a version-matching `hello`, and answers
//! anything else with an explicit `ai_error` (mixed versions are expected in
//! practice — terminals are long-lived while the daemon restarts independently).
//!
//! # Evolution rules
//!
//! - The `hello` shape is frozen: existing fields never change meaning or type, so
//!   any version of either side can always read the other's handshake. New fields
//!   must be optional.
//! - Evolve additively without bumping the version: new optional fields on existing
//!   messages, and new message types. Receivers must ignore message types they do
//!   not know (`koshell-rs` skips them in its IPC reader; the daemon's parser
//!   returns null and logs).
//! - Bump [`PROTOCOL_VERSION`] only for breaking changes: removing or retyping a
//!   field, or changing message semantics. Keep `packages/ai-daemon/src/protocol.ts`
//!   in lockstep.

use serde::{Deserialize, Serialize};

/// IPC protocol version. Bump on any breaking change to the message shapes (see the
/// evolution rules in the crate docs).
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
///
/// Per request, the daemon sends `ack` first (the request was parsed and
/// enqueued), then zero or more `ai_delta` chunks, then exactly one of
/// `ai_response_end` or `ai_error`. `ai_tool_call` is reserved for the tool
/// round-trip stage.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Acknowledges receipt of an [`ClientMessage::AiRequest`], before any
    /// streaming output.
    Ack { request_id: String },
    /// One streamed chunk of assistant text for an in-flight request.
    AiDelta { request_id: String, delta: String },
    /// Terminal success marker: no further messages follow for this request.
    AiResponseEnd { request_id: String },
    /// Terminal failure marker: no further messages follow for this request.
    AiError { request_id: String, message: String },
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

    #[test]
    fn streaming_messages_round_trip_as_tagged_json() {
        let delta = serde_json::to_string(&ServerMessage::AiDelta {
            request_id: "req-1".into(),
            delta: "Hello".into(),
        })
        .unwrap();
        assert_eq!(
            delta,
            r#"{"type":"ai_delta","request_id":"req-1","delta":"Hello"}"#
        );

        let end = serde_json::to_string(&ServerMessage::AiResponseEnd {
            request_id: "req-1".into(),
        })
        .unwrap();
        assert_eq!(end, r#"{"type":"ai_response_end","request_id":"req-1"}"#);

        let error = serde_json::to_string(&ServerMessage::AiError {
            request_id: "req-1".into(),
            message: "no provider configured".into(),
        })
        .unwrap();
        assert_eq!(
            error,
            r#"{"type":"ai_error","request_id":"req-1","message":"no provider configured"}"#
        );

        let back: ServerMessage = serde_json::from_str(&delta).unwrap();
        match back {
            ServerMessage::AiDelta { request_id, delta } => {
                assert_eq!(request_id, "req-1");
                assert_eq!(delta, "Hello");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }
}
