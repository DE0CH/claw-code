//! Machine-readable turn events (NDJSON on stdout) for hosts that drive `claw` as an
//! agent loop, e.g. an OpenClaw agent harness. Enabled by `CLAW_EVENT_STREAM=1` with
//! `--output-format json prompt …`; when disabled every call here is a no-op.
//!
//! One JSON object per line, each with a `type`:
//!   session          {session_id, path, model, cwd, resumed, permission_mode}
//!   message_start    a model response begins
//!   text_delta       {text}
//!   thinking_delta   {text}
//!   tool_use         {id, name, input}            input = parsed JSON (raw string if unparsable)
//!   tool_result      {id, name, output, is_error}
//!   message_stop     a model response ended
//!   permission_request {request_id, tool_name, input, current_mode, required_mode, reason}
//!                    the host answers on stdin with one line:
//!                    {"type":"permission_response","request_id":…,"allow":true|false,"reason":…}
//!   result           the turn's summary (message, usage, iterations, …)
//!   error            {message}

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::session::{ContentBlock, ConversationMessage};

static ENABLED: AtomicBool = AtomicBool::new(false);
static WRITE_LOCK: Mutex<()> = Mutex::new(());
// Tests capture lines here instead of stdout.
static CAPTURE: Mutex<Option<Vec<Value>>> = Mutex::new(None);

pub fn enable() {
    ENABLED.store(true, Ordering::SeqCst);
}

#[must_use]
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::SeqCst)
}

/// True when `CLAW_EVENT_STREAM` asks for the stream.
#[must_use]
pub fn requested_by_env() -> bool {
    std::env::var("CLAW_EVENT_STREAM").is_ok_and(|value| matches!(value.trim(), "1" | "true" | "yes"))
}

/// Write one event line (no-op unless enabled).
pub fn emit(event: Value) {
    if !is_enabled() {
        return;
    }
    if let Ok(mut capture) = CAPTURE.lock() {
        if let Some(lines) = capture.as_mut() {
            lines.push(event);
            return;
        }
    }
    let _guard = WRITE_LOCK.lock();
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{event}");
    let _ = out.flush();
}

/// Tool input as JSON when it parses, else the raw string.
#[must_use]
pub fn tool_input_value(input: &str) -> Value {
    serde_json::from_str(input).unwrap_or_else(|_| Value::String(input.to_string()))
}

pub fn emit_tool_use(id: &str, name: &str, input: &str) {
    emit(json!({"type": "tool_use", "id": id, "name": name, "input": tool_input_value(input)}));
}

pub fn emit_tool_results(message: &ConversationMessage) {
    if !is_enabled() {
        return;
    }
    for block in &message.blocks {
        if let ContentBlock::ToolResult { tool_use_id, tool_name, output, is_error } = block {
            emit(json!({"type": "tool_result", "id": tool_use_id, "name": tool_name, "output": output, "is_error": is_error}));
        }
    }
}

/// Test support: enable the stream and collect events in memory instead of stdout.
pub fn start_capture() {
    enable();
    if let Ok(mut capture) = CAPTURE.lock() {
        *capture = Some(Vec::new());
    }
}

/// Test support: stop capturing and return what was emitted.
#[must_use]
pub fn take_capture() -> Vec<Value> {
    CAPTURE.lock().ok().and_then(|mut capture| capture.take()).unwrap_or_default()
}

/// Serialises tests that use the global capture.
pub static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_tool_use_and_results() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        start_capture();
        emit_tool_use("t1", "bash", r#"{"command":"ls"}"#);
        emit_tool_use("t2", "bash", "not json");
        emit_tool_results(&ConversationMessage::tool_result("t1", "bash", "out", false));
        let lines = take_capture();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["input"]["command"], "ls");
        assert_eq!(lines[1]["input"], "not json");
        assert_eq!(lines[2], json!({"type":"tool_result","id":"t1","name":"bash","output":"out","is_error":false}));
    }
}
