//! Claude Code's hook-event wire format.

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    SessionStart,
    PostToolUse,
    PreCompact,
    SessionEnd,
    Other,
}

pub const SESSION_START_WIRE_NAME: &str = "SessionStart";
pub const POST_TOOL_USE_WIRE_NAME: &str = "PostToolUse";

#[derive(Debug)]
pub struct Event {
    value: Value,
    kind: EventKind,
}

impl Event {
    pub fn parse(input: &str) -> serde_json::Result<Self> {
        let value: Value = serde_json::from_str(input)?;
        let kind = match value.get("hook_event_name").and_then(Value::as_str) {
            Some("SessionStart") => EventKind::SessionStart,
            Some("PostToolUse") => EventKind::PostToolUse,
            Some("PreCompact") => EventKind::PreCompact,
            Some("SessionEnd") => EventKind::SessionEnd,
            _ => EventKind::Other,
        };
        Ok(Self { value, kind })
    }

    pub const fn kind(&self) -> EventKind {
        self.kind
    }

    pub fn session_id(&self) -> Option<&str> {
        self.text("session_id")
    }

    /// What the session's hook state is kept under: the session id, and for
    /// a subagent its `agent_id` too. A subagent's tool events carry the
    /// parent's `session_id` and their own `agent_id`, and a subagent's
    /// context holds none of what the parent was told, nor the parent any of
    /// what the subagent was told.
    pub fn state_key(&self) -> Option<String> {
        let session_id = self.session_id()?;
        Some(self.text("agent_id").map_or_else(
            || session_id.to_owned(),
            |agent| format!("{session_id}.agent-{agent}"),
        ))
    }

    pub fn cwd(&self) -> Option<&str> {
        self.text("cwd")
    }

    pub fn source(&self) -> Option<&str> {
        self.text("source")
    }

    pub fn tool_name(&self) -> Option<&str> {
        self.text("tool_name")
    }

    pub fn tool_input(&self) -> Option<&Value> {
        self.value.get("tool_input")
    }

    fn text(&self, name: &str) -> Option<&str> {
        self.value.get(name).and_then(Value::as_str)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HookOutput<'a> {
    hook_specific_output: SpecificOutput<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SpecificOutput<'a> {
    hook_event_name: &'a str,
    additional_context: &'a str,
}

pub fn response(event_name: &str, additional_context: &str) -> serde_json::Result<String> {
    serde_json::to_string(&HookOutput {
        hook_specific_output: SpecificOutput {
            hook_event_name: event_name,
            additional_context,
        },
    })
}
