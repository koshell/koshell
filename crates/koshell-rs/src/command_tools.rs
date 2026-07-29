//! Terminal-side execution of the read-only context tools the daemon calls.
//!
//! The daemon owns the agent and its tool definitions; this module owns what those
//! tools actually read. It is the enforcement point for the observe-only boundary:
//! the whole reachable surface is the two functions below, both of which return
//! bounded, already-captured terminal facts. Nothing here writes to the PTY, spawns a
//! process, touches the filesystem, or can reach another terminal session — a tool
//! name outside this catalog is an error, not a dynamic dispatch.
//!
//! Arguments arrive as untrusted JSON from the socket and are validated here before
//! becoming typed values. Every failure is a structured [`ToolError`] the agent can
//! read and work around, never a panic and never terminal output: a bad tool call must
//! not disturb the user's shell.

use koshell_proto::ToolError;
use serde_json::{Value, json};

use crate::command_history::{
    CommandOutputInfo, CompletedCommand, MAX_COMPLETED_COMMANDS, MAX_READ_LIMIT, ReadError,
    ReadPage,
};
use crate::trigger::SessionState;

pub const TOOL_LIST_RECENT_COMMANDS: &str = "list_recent_commands";
pub const TOOL_READ_COMMAND_OUTPUT: &str = "read_command_output";

fn error(code: &str, message: impl Into<String>) -> ToolError {
    ToolError {
        code: code.to_string(),
        message: message.into(),
        details: None,
    }
}

fn error_with(code: &str, message: impl Into<String>, details: Value) -> ToolError {
    ToolError {
        code: code.to_string(),
        message: message.into(),
        details: Some(details),
    }
}

fn output_info_json(info: &CommandOutputInfo) -> Value {
    json!({
        "format": info.format,
        "totalBytes": info.total_bytes,
        "totalCharacters": info.total_characters,
        "retainedBytes": info.retained_bytes,
        "retainedCharacters": info.retained_characters,
        "retainedStartOffset": info.retained_start_offset,
        "droppedPrefixBytes": info.dropped_prefix_bytes,
        "sourceTruncated": info.source_truncated,
        "available": info.available,
    })
}

fn row_json(command: &CompletedCommand) -> Value {
    let (preview, truncated) = command.command_preview();
    let mut row = json!({
        "commandId": command.command_id,
        "commandPreview": preview,
        "commandTruncated": truncated,
        "startedAt": command.started_at,
        "endedAt": command.ended_at,
        "durationMs": command.duration_ms,
        "output": output_info_json(&command.output_info()),
    });
    if let Some(cwd) = &command.cwd {
        row["cwd"] = Value::String(cwd.clone());
    }
    if let Some(exit_code) = command.exit_code {
        row["exitCode"] = json!(exit_code);
    }
    row
}

fn page_json(page: &ReadPage) -> Value {
    let mut value = output_info_json(&page.info);
    value["commandId"] = Value::String(page.command_id.clone());
    value["offset"] = json!(page.offset);
    value["nextOffset"] = json!(page.next_offset);
    value["hasMore"] = json!(page.has_more);
    value["content"] = Value::String(page.content.clone());
    value
}

/// Reads an optional non-negative integer argument. A present-but-wrong-typed value is
/// rejected rather than coerced: silently reading offset `0` for `"offset": "abc"`
/// would hand the agent a page it did not ask for.
fn optional_usize(arguments: &Value, key: &str) -> Result<Option<usize>, ToolError> {
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_u64().map(|n| Some(n as usize)).ok_or_else(|| {
            error(
                "invalid_arguments",
                format!("\"{key}\" must be a non-negative integer"),
            )
        }),
    }
}

/// Executes one tool call against the terminal's own session state.
pub fn execute(
    state: &SessionState,
    tool_name: &str,
    arguments: &Value,
) -> Result<Value, ToolError> {
    match tool_name {
        TOOL_LIST_RECENT_COMMANDS => Ok(list_recent_commands(state)),
        TOOL_READ_COMMAND_OUTPUT => read_command_output(state, arguments),
        other => Err(error_with(
            "unsupported_tool",
            format!("this terminal does not serve a tool named \"{other}\""),
            json!({ "supported": [TOOL_LIST_RECENT_COMMANDS, TOOL_READ_COMMAND_OUTPUT] }),
        )),
    }
}

/// The completed-command index, newest first, without any output bodies. Listing is
/// cheap and bounded; the agent picks an id from it and pays for content only on the
/// read that follows.
fn list_recent_commands(state: &SessionState) -> Value {
    let history = state.command_history();
    json!({
        "commands": history.recent().into_iter().map(row_json).collect::<Vec<_>>(),
        "maxCommands": MAX_COMPLETED_COMMANDS,
    })
}

fn read_command_output(state: &SessionState, arguments: &Value) -> Result<Value, ToolError> {
    let command_id = arguments
        .get("commandId")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            error(
                "invalid_arguments",
                "\"commandId\" is required and must be a string",
            )
        })?;

    let offset = optional_usize(arguments, "offset")?;
    let limit = optional_usize(arguments, "limit")?;
    if let Some(limit) = limit
        && limit == 0
    {
        return Err(error("invalid_arguments", "\"limit\" must be at least 1"));
    }

    match state.command_history().read(command_id, offset, limit) {
        Ok(page) => Ok(page_json(&page)),
        Err(ReadError::CommandNotFound) => Err(error_with(
            "command_not_found",
            format!(
                "no completed command with id \"{command_id}\"; call {TOOL_LIST_RECENT_COMMANDS} for the current ids"
            ),
            json!({ "commandId": command_id }),
        )),
        // Naming the earliest available offset turns a dead end into a retry: the
        // agent re-asks precisely instead of guessing at the retained window.
        Err(ReadError::OutputEvicted { earliest_offset }) => Err(error_with(
            "output_evicted",
            format!("output before offset {earliest_offset} was discarded by the retention bounds"),
            json!({ "commandId": command_id, "earliestOffset": earliest_offset }),
        )),
    }
}

/// The pushed inventory's command-output section. The agent fails to pull mostly
/// because it does not know what exists, so every request advertises availability,
/// how many commands are indexed, and the newest id — and advertises nothing when the
/// index is empty, so it never names an id that cannot be read.
pub fn pull_inventory(state: &SessionState) -> Value {
    let history = state.command_history();
    let count = history.completed_count();
    let mut inventory = json!({
        "available": count > 0,
        "recentCompletedCount": count,
        "maxCommands": MAX_COMPLETED_COMMANDS,
        "maxReadCharacters": MAX_READ_LIMIT,
    });
    if let Some(latest) = history.latest_command_id() {
        inventory["latestCommandId"] = Value::String(latest.to_string());
    }
    inventory
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell_integration::{MarkerKind, ShellIntegrationMarker};
    use std::time::Instant;

    fn marker(kind: MarkerKind, command: &str, exit_code: Option<i32>) -> ShellIntegrationMarker {
        ShellIntegrationMarker {
            kind,
            command: Some(command.to_string()),
            exit_code,
            cwd: None,
            executed: true,
        }
    }

    /// Drives a session through one real command span.
    fn session_with(commands: &[(&str, &str, i32)]) -> SessionState {
        let mut state = SessionState::new(80, 24, true);
        let now = Instant::now();
        for (command, output, exit_code) in commands {
            state.handle_marker(marker(MarkerKind::CommandStart, command, None), now);
            state.record_output(output.as_bytes(), now);
            state.handle_marker(
                marker(MarkerKind::CommandEnd, command, Some(*exit_code)),
                now,
            );
        }
        state
    }

    #[test]
    fn list_returns_completed_commands_newest_first_without_bodies() {
        let state = session_with(&[("ls", "a\r\nb\r\n", 0), ("false", "", 1)]);
        let result = execute(&state, TOOL_LIST_RECENT_COMMANDS, &json!({})).unwrap();

        let commands = result["commands"].as_array().unwrap();
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0]["commandPreview"], "false");
        assert_eq!(commands[0]["exitCode"], 1);
        assert_eq!(commands[1]["commandPreview"], "ls");
        assert_eq!(result["maxCommands"], MAX_COMPLETED_COMMANDS);
        // Listing never carries output content.
        for command in commands {
            assert!(command.get("content").is_none());
            assert_eq!(command["output"]["format"], "pty_text");
        }
    }

    #[test]
    fn list_is_empty_before_any_command_completes() {
        let state = SessionState::new(80, 24, true);
        let result = execute(&state, TOOL_LIST_RECENT_COMMANDS, &json!({})).unwrap();
        assert_eq!(result["commands"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn read_returns_the_span_output_with_truthful_accounting() {
        let state = session_with(&[("ls", "a\r\nb\r\n", 0)]);
        let result = execute(
            &state,
            TOOL_READ_COMMAND_OUTPUT,
            &json!({ "commandId": "command-1" }),
        )
        .unwrap();

        assert_eq!(result["content"], "a\r\nb\r\n");
        assert_eq!(result["offset"], 0);
        assert_eq!(result["nextOffset"], 6);
        assert_eq!(result["hasMore"], false);
        assert_eq!(result["available"], true);
        assert_eq!(result["sourceTruncated"], false);
        assert_eq!(result["totalCharacters"], 6);
    }

    #[test]
    fn an_unknown_tool_name_is_rejected_rather_than_dispatched() {
        let state = SessionState::new(80, 24, true);
        let error = execute(&state, "run_command", &json!({})).unwrap_err();
        assert_eq!(error.code, "unsupported_tool");
        // The reply names the whole reachable surface, which is exactly two readers.
        let supported = error.details.unwrap()["supported"].clone();
        assert_eq!(
            supported,
            json!([TOOL_LIST_RECENT_COMMANDS, TOOL_READ_COMMAND_OUTPUT])
        );
    }

    #[test]
    fn a_missing_or_mistyped_command_id_is_invalid_arguments() {
        let state = session_with(&[("ls", "x", 0)]);
        for arguments in [
            json!({}),
            json!({ "commandId": 7 }),
            json!({ "commandId": null }),
        ] {
            let error = execute(&state, TOOL_READ_COMMAND_OUTPUT, &arguments).unwrap_err();
            assert_eq!(error.code, "invalid_arguments");
        }
    }

    #[test]
    fn a_mistyped_offset_is_rejected_instead_of_coerced() {
        let state = session_with(&[("ls", "x", 0)]);
        for arguments in [
            json!({ "commandId": "command-1", "offset": "abc" }),
            json!({ "commandId": "command-1", "offset": -1 }),
            json!({ "commandId": "command-1", "limit": 0 }),
        ] {
            let error = execute(&state, TOOL_READ_COMMAND_OUTPUT, &arguments).unwrap_err();
            assert_eq!(error.code, "invalid_arguments", "for {arguments}");
        }
    }

    #[test]
    fn an_unknown_command_id_points_at_the_list_tool() {
        let state = session_with(&[("ls", "x", 0)]);
        let error = execute(
            &state,
            TOOL_READ_COMMAND_OUTPUT,
            &json!({ "commandId": "command-99" }),
        )
        .unwrap_err();
        assert_eq!(error.code, "command_not_found");
        assert!(error.message.contains(TOOL_LIST_RECENT_COMMANDS));
    }

    // A comment-only `#?` line runs nothing, so it must not appear as a command.
    #[test]
    fn synthetic_trigger_markers_create_no_command_row() {
        let mut state = SessionState::new(80, 24, true);
        let now = Instant::now();
        let mut start = marker(MarkerKind::CommandStart, "#? why", None);
        start.executed = false;
        let mut end = marker(MarkerKind::CommandEnd, "#? why", Some(0));
        end.executed = false;

        state.handle_marker(start, now);
        state.record_output(b"$ ", now);
        state.handle_marker(end, now);

        let result = execute(&state, TOOL_LIST_RECENT_COMMANDS, &json!({})).unwrap();
        assert_eq!(result["commands"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn the_returning_prompt_is_not_part_of_the_span() {
        let mut state = SessionState::new(80, 24, true);
        let now = Instant::now();
        state.handle_marker(marker(MarkerKind::CommandStart, "echo hi", None), now);
        state.record_output(b"hi\r\n", now);
        state.handle_marker(marker(MarkerKind::CommandEnd, "echo hi", Some(0)), now);
        // Everything after the end marker belongs to the prompt, not the command.
        state.record_output(b"user@host:~$ ", now);

        let result = execute(
            &state,
            TOOL_READ_COMMAND_OUTPUT,
            &json!({ "commandId": "command-1" }),
        )
        .unwrap();
        assert_eq!(result["content"], "hi\r\n");
    }

    #[test]
    fn the_inventory_advertises_only_what_can_be_read() {
        let empty = SessionState::new(80, 24, true);
        let inventory = pull_inventory(&empty);
        assert_eq!(inventory["available"], false);
        assert_eq!(inventory["recentCompletedCount"], 0);
        assert!(
            inventory.get("latestCommandId").is_none(),
            "an empty index names no id"
        );

        let state = session_with(&[("ls", "x", 0), ("pwd", "/tmp", 0)]);
        let inventory = pull_inventory(&state);
        assert_eq!(inventory["available"], true);
        assert_eq!(inventory["recentCompletedCount"], 2);
        assert_eq!(inventory["latestCommandId"], "command-2");
    }

    #[test]
    fn paging_a_long_span_reassembles_it_exactly() {
        let mut state = SessionState::new(80, 24, true);
        let now = Instant::now();
        let output: String = (0..20_000)
            .map(|n| char::from(b'a' + (n % 26) as u8))
            .collect();
        state.handle_marker(marker(MarkerKind::CommandStart, "seq", None), now);
        state.record_output(output.as_bytes(), now);
        state.handle_marker(marker(MarkerKind::CommandEnd, "seq", Some(0)), now);

        let mut assembled = String::new();
        let mut offset = 0u64;
        loop {
            let page = execute(
                &state,
                TOOL_READ_COMMAND_OUTPUT,
                &json!({ "commandId": "command-1", "offset": offset }),
            )
            .unwrap();
            assembled.push_str(page["content"].as_str().unwrap());
            offset = page["nextOffset"].as_u64().unwrap();
            if !page["hasMore"].as_bool().unwrap() {
                break;
            }
        }
        assert_eq!(assembled, output);
    }
}
