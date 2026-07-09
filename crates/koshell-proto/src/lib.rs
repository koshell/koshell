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
    /// Withdraws an in-flight [`ClientMessage::AiRequest`] after a user interrupt
    /// (Ctrl+C). Best-effort: the terminal has already stopped rendering the
    /// response locally and suppresses whatever still arrives, so this only asks
    /// the daemon to stop generating (saving tokens) and to skip the request if it
    /// is still queued (unblocking later questions). The daemon still terminates
    /// the request with its usual single end/error marker; a daemon that does not
    /// know this message type ignores it, per the additive-evolution rule.
    AiCancel { request_id: String },
    /// Response to a daemon-initiated context tool call. Reserved for the next
    /// stage (tool round-trips); not produced yet.
    ToolResponse {
        request_id: String,
        tool_call_id: String,
        result: serde_json::Value,
    },
    /// Graceful shutdown of a terminal session.
    Bye { terminal_session_id: String },
    /// Diagnostics request for `koshell daemon status`; answered with
    /// [`ServerMessage::Status`] regardless of the `hello` handshake (asking a
    /// version-mismatched daemon for its identity is exactly the use case).
    /// Additive: a daemon that does not know this type ignores it.
    StatusRequest {},
    /// Starts an interactive OAuth login for `provider` on this connection
    /// (`koshell auth login`). The daemon replies `ack`, streams display and
    /// prompt events, and terminates with exactly one
    /// [`ServerMessage::AuthResult`]. Dropping the connection aborts the flow.
    /// Additive: a daemon that does not know this type ignores it (no `ack`),
    /// which the client treats as "daemon too old".
    AuthLogin {
        request_id: String,
        provider: String,
    },
    /// Removes the stored credential for `provider` (`koshell auth logout`).
    /// Answered with `ack` then one [`ServerMessage::AuthResult`]. Additive.
    AuthLogout {
        request_id: String,
        provider: String,
    },
    /// Asks for per-provider credential status (`koshell auth status`);
    /// `provider` limits the report to one entry. Answered with `ack` then one
    /// [`ServerMessage::AuthStatus`]. Additive.
    AuthStatusRequest {
        request_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
    },
    /// Answers a daemon-initiated [`ServerMessage::AuthPrompt`] or
    /// [`ServerMessage::AuthSelect`]. `value` is the typed text (prompt) or the
    /// chosen option id (select); `None` means the user declined (EOF).
    AuthPromptResponse {
        request_id: String,
        prompt_id: String,
        value: Option<String>,
    },
    /// Asks the daemon to re-read koshell.toml and rebuild live sessions
    /// (`koshell reload`). `session_id` targets one instance's conversation;
    /// `None` (the `--all` form) resets every active session. Answered with one
    /// [`ServerMessage::Reload`] regardless of the `hello` handshake — it is
    /// daemon-global, like status, and routed by the `session_id` in the message
    /// rather than the requester's own connection. Additive: a daemon that does
    /// not know this type ignores it, which the client treats as "daemon too old".
    ReloadRequest {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    /// Asks for one instance's live state (`koshell status`), routed by
    /// `session_id` (the wrapper's `terminal_session_id`). Answered with one
    /// [`ServerMessage::InstanceStatus`], again without a `hello` handshake.
    /// Additive: a daemon that does not know this type ignores it.
    InstanceStatusRequest { session_id: String },
}

/// One choice offered by [`ServerMessage::AuthSelect`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthSelectOption {
    pub id: String,
    pub label: String,
}

/// One provider row in [`ServerMessage::AuthStatus`].
///
/// `source` is a koshell-defined label ("stored", "environment", "config"), kept
/// as a free string on the wire so a new label never breaks an older client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthStatusEntry {
    /// pi provider id, e.g. "github-copilot".
    pub provider: String,
    /// Display name, e.g. "GitHub Copilot".
    pub name: String,
    /// Whether `koshell auth login` applies to this provider.
    pub oauth: bool,
    /// Whether any usable credential was found (stored, environment, or config).
    pub configured: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Human detail for the source, e.g. the environment variable name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
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
    /// Reply to [`ClientMessage::StatusRequest`]: the daemon's identity and load.
    /// `version` is the daemon package version; `connections` is the live terminal
    /// count at reply time. Additive: an older terminal's IPC reader ignores it.
    Status {
        pid: u32,
        version: String,
        protocol_version: u32,
        uptime_ms: u64,
        connections: u32,
    },
    /// Login display event: "open this URL to authorize".
    AuthUrl {
        request_id: String,
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
    },
    /// Login display event: enter `user_code` at `verification_uri` (device-code
    /// flows: github-copilot, openai-codex).
    AuthDeviceCode {
        request_id: String,
        user_code: String,
        verification_uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interval_seconds: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_in_seconds: Option<u64>,
    },
    /// Login display event: free-form progress line ("Waiting for
    /// authorization...").
    AuthProgress { request_id: String, message: String },
    /// Daemon-initiated free-text prompt; answered by
    /// [`ClientMessage::AuthPromptResponse`] with the same `prompt_id`.
    AuthPrompt {
        request_id: String,
        prompt_id: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
        allow_empty: bool,
    },
    /// Daemon-initiated selection; answered by
    /// [`ClientMessage::AuthPromptResponse`] with the chosen option id.
    AuthSelect {
        request_id: String,
        prompt_id: String,
        message: String,
        options: Vec<AuthSelectOption>,
    },
    /// Terminal marker for [`ClientMessage::AuthLogin`] and
    /// [`ClientMessage::AuthLogout`]: exactly one per request.
    AuthResult {
        request_id: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// Terminal reply to [`ClientMessage::AuthStatusRequest`].
    AuthStatus {
        request_id: String,
        entries: Vec<AuthStatusEntry>,
    },
    /// Reply to [`ClientMessage::ReloadRequest`]. `ok` is whether the new config
    /// validated and was applied; `message` is a human summary (applied-session
    /// count, or the validation error). Kept as a single message + bool so later
    /// Skills/plugin reloading extends it without a rigid schema. Additive: an
    /// older terminal's IPC reader ignores it.
    Reload {
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// Reply to [`ClientMessage::InstanceStatusRequest`]. `known` is whether the
    /// daemon has a live connection for that `session_id` (an instance that has
    /// not issued a `#?` yet is unknown); the per-connection fields are populated
    /// only when `known`, while the daemon-global fields are always present so
    /// `koshell status` can report the daemon even for a not-yet-connected
    /// instance. Additive: an older terminal's IPC reader ignores it.
    InstanceStatus {
        known: bool,
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shell: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        conversation: bool,
        daemon_pid: u32,
        uptime_ms: u64,
        version: String,
        protocol_version: u32,
        connections: u32,
    },
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
    fn ai_cancel_round_trips_as_tagged_json() {
        let line = serde_json::to_string(&ClientMessage::AiCancel {
            request_id: "req-1".into(),
        })
        .unwrap();
        assert_eq!(line, r#"{"type":"ai_cancel","request_id":"req-1"}"#);
        let back: ClientMessage = serde_json::from_str(&line).unwrap();
        match back {
            ClientMessage::AiCancel { request_id } => assert_eq!(request_id, "req-1"),
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

    #[test]
    fn auth_requests_round_trip_as_tagged_json() {
        let login = serde_json::to_string(&ClientMessage::AuthLogin {
            request_id: "auth-1".into(),
            provider: "anthropic".into(),
        })
        .unwrap();
        assert_eq!(
            login,
            r#"{"type":"auth_login","request_id":"auth-1","provider":"anthropic"}"#
        );

        let logout = serde_json::to_string(&ClientMessage::AuthLogout {
            request_id: "auth-1".into(),
            provider: "anthropic".into(),
        })
        .unwrap();
        assert_eq!(
            logout,
            r#"{"type":"auth_logout","request_id":"auth-1","provider":"anthropic"}"#
        );

        let back: ClientMessage = serde_json::from_str(&login).unwrap();
        match back {
            ClientMessage::AuthLogin {
                request_id,
                provider,
            } => {
                assert_eq!(request_id, "auth-1");
                assert_eq!(provider, "anthropic");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn auth_status_request_omits_an_absent_provider() {
        let all = serde_json::to_string(&ClientMessage::AuthStatusRequest {
            request_id: "auth-1".into(),
            provider: None,
        })
        .unwrap();
        assert_eq!(
            all,
            r#"{"type":"auth_status_request","request_id":"auth-1"}"#
        );

        let back: ClientMessage = serde_json::from_str(&all).unwrap();
        match back {
            ClientMessage::AuthStatusRequest { provider, .. } => assert_eq!(provider, None),
            other => panic!("unexpected variant: {other:?}"),
        }

        let one = serde_json::to_string(&ClientMessage::AuthStatusRequest {
            request_id: "auth-1".into(),
            provider: Some("openai-codex".into()),
        })
        .unwrap();
        assert_eq!(
            one,
            r#"{"type":"auth_status_request","request_id":"auth-1","provider":"openai-codex"}"#
        );
    }

    #[test]
    fn auth_prompt_response_serializes_a_declined_value_as_null() {
        let declined = serde_json::to_string(&ClientMessage::AuthPromptResponse {
            request_id: "auth-1".into(),
            prompt_id: "prompt-1".into(),
            value: None,
        })
        .unwrap();
        assert_eq!(
            declined,
            r#"{"type":"auth_prompt_response","request_id":"auth-1","prompt_id":"prompt-1","value":null}"#
        );

        let answered = serde_json::to_string(&ClientMessage::AuthPromptResponse {
            request_id: "auth-1".into(),
            prompt_id: "prompt-1".into(),
            value: Some("code".into()),
        })
        .unwrap();
        let back: ClientMessage = serde_json::from_str(&answered).unwrap();
        match back {
            ClientMessage::AuthPromptResponse { value, .. } => {
                assert_eq!(value.as_deref(), Some("code"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn auth_events_round_trip_as_tagged_json() {
        let url = serde_json::to_string(&ServerMessage::AuthUrl {
            request_id: "auth-1".into(),
            url: "https://example.test/authorize".into(),
            instructions: None,
        })
        .unwrap();
        assert_eq!(
            url,
            r#"{"type":"auth_url","request_id":"auth-1","url":"https://example.test/authorize"}"#
        );

        let device = serde_json::to_string(&ServerMessage::AuthDeviceCode {
            request_id: "auth-1".into(),
            user_code: "ABCD-1234".into(),
            verification_uri: "https://example.test/device".into(),
            interval_seconds: Some(5),
            expires_in_seconds: None,
        })
        .unwrap();
        let back: ServerMessage = serde_json::from_str(&device).unwrap();
        match back {
            ServerMessage::AuthDeviceCode {
                user_code,
                interval_seconds,
                expires_in_seconds,
                ..
            } => {
                assert_eq!(user_code, "ABCD-1234");
                assert_eq!(interval_seconds, Some(5));
                assert_eq!(expires_in_seconds, None);
            }
            other => panic!("unexpected variant: {other:?}"),
        }

        let result = serde_json::to_string(&ServerMessage::AuthResult {
            request_id: "auth-1".into(),
            ok: true,
            message: Some("logged in".into()),
        })
        .unwrap();
        assert_eq!(
            result,
            r#"{"type":"auth_result","request_id":"auth-1","ok":true,"message":"logged in"}"#
        );
    }

    #[test]
    fn auth_prompts_and_status_round_trip_nested_structs() {
        let select = serde_json::to_string(&ServerMessage::AuthSelect {
            request_id: "auth-1".into(),
            prompt_id: "prompt-2".into(),
            message: "How do you want to sign in?".into(),
            options: vec![
                AuthSelectOption {
                    id: "browser".into(),
                    label: "Open a browser".into(),
                },
                AuthSelectOption {
                    id: "device_code".into(),
                    label: "Enter a device code".into(),
                },
            ],
        })
        .unwrap();
        let back: ServerMessage = serde_json::from_str(&select).unwrap();
        match back {
            ServerMessage::AuthSelect { options, .. } => {
                assert_eq!(options.len(), 2);
                assert_eq!(options[0].id, "browser");
            }
            other => panic!("unexpected variant: {other:?}"),
        }

        let prompt = serde_json::to_string(&ServerMessage::AuthPrompt {
            request_id: "auth-1".into(),
            prompt_id: "prompt-1".into(),
            message: "Paste the authorization code".into(),
            placeholder: None,
            allow_empty: false,
        })
        .unwrap();
        assert_eq!(
            prompt,
            r#"{"type":"auth_prompt","request_id":"auth-1","prompt_id":"prompt-1","message":"Paste the authorization code","allow_empty":false}"#
        );

        let status = serde_json::to_string(&ServerMessage::AuthStatus {
            request_id: "auth-1".into(),
            entries: vec![AuthStatusEntry {
                provider: "anthropic".into(),
                name: "Anthropic (Claude Pro/Max)".into(),
                oauth: true,
                configured: true,
                source: Some("environment".into()),
                label: Some("ANTHROPIC_API_KEY".into()),
            }],
        })
        .unwrap();
        let back: ServerMessage = serde_json::from_str(&status).unwrap();
        match back {
            ServerMessage::AuthStatus { entries, .. } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].source.as_deref(), Some("environment"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn reload_request_omits_an_absent_session() {
        let all =
            serde_json::to_string(&ClientMessage::ReloadRequest { session_id: None }).unwrap();
        assert_eq!(all, r#"{"type":"reload_request"}"#);
        let one = serde_json::to_string(&ClientMessage::ReloadRequest {
            session_id: Some("koshell-42".into()),
        })
        .unwrap();
        assert_eq!(
            one,
            r#"{"type":"reload_request","session_id":"koshell-42"}"#
        );
        let back: ClientMessage = serde_json::from_str(&all).unwrap();
        match back {
            ClientMessage::ReloadRequest { session_id } => assert_eq!(session_id, None),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn reload_reply_round_trips_with_optional_message() {
        let ok = serde_json::to_string(&ServerMessage::Reload {
            ok: true,
            message: Some("configuration reloaded".into()),
        })
        .unwrap();
        assert_eq!(
            ok,
            r#"{"type":"reload","ok":true,"message":"configuration reloaded"}"#
        );
        let back: ServerMessage = serde_json::from_str(r#"{"type":"reload","ok":false}"#).unwrap();
        match back {
            ServerMessage::Reload { ok, message } => {
                assert!(!ok);
                assert_eq!(message, None);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn instance_status_round_trips() {
        let request = serde_json::to_string(&ClientMessage::InstanceStatusRequest {
            session_id: "koshell-42".into(),
        })
        .unwrap();
        assert_eq!(
            request,
            r#"{"type":"instance_status_request","session_id":"koshell-42"}"#
        );

        let known = ServerMessage::InstanceStatus {
            known: true,
            session_id: "koshell-42".into(),
            cwd: Some("/home/u/proj".into()),
            shell: Some("/bin/zsh".into()),
            model: Some("anthropic/claude-sonnet-4-5".into()),
            conversation: true,
            daemon_pid: 1234,
            uptime_ms: 9000,
            version: "0.1.0".into(),
            protocol_version: PROTOCOL_VERSION,
            connections: 2,
        };
        let line = serde_json::to_string(&known).unwrap();
        let back: ServerMessage = serde_json::from_str(&line).unwrap();
        match back {
            ServerMessage::InstanceStatus {
                known,
                model,
                conversation,
                connections,
                ..
            } => {
                assert!(known);
                assert_eq!(model.as_deref(), Some("anthropic/claude-sonnet-4-5"));
                assert!(conversation);
                assert_eq!(connections, 2);
            }
            other => panic!("unexpected variant: {other:?}"),
        }

        // An unknown instance omits the per-connection fields but keeps the
        // daemon-global ones.
        let unknown = serde_json::to_string(&ServerMessage::InstanceStatus {
            known: false,
            session_id: "koshell-99".into(),
            cwd: None,
            shell: None,
            model: None,
            conversation: false,
            daemon_pid: 1234,
            uptime_ms: 9000,
            version: "0.1.0".into(),
            protocol_version: PROTOCOL_VERSION,
            connections: 2,
        })
        .unwrap();
        assert!(!unknown.contains("\"cwd\""));
        assert!(unknown.contains("\"known\":false"));
    }
}
