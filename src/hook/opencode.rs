//! `OpenCode`'s hook-envelope wire format.

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    ToolExecuteAfter,
    ChatSystem,
    ShellEnv,
    Compacting,
    Other,
}

#[derive(Debug)]
pub struct Event {
    value: Value,
    kind: EventKind,
}

#[derive(Debug, Clone, Copy)]
pub struct Parts {
    pub notice: bool,
    pub guidance: bool,
}

impl Event {
    pub fn parse(input: &str) -> serde_json::Result<Self> {
        let value: Value = serde_json::from_str(input)?;
        let kind = match value.get("event").and_then(Value::as_str) {
            Some("tool.execute.after") => EventKind::ToolExecuteAfter,
            Some("chat.system") => EventKind::ChatSystem,
            Some("shell.env") => EventKind::ShellEnv,
            Some("compacting") => EventKind::Compacting,
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

    pub fn tool(&self) -> Option<&str> {
        self.text("tool")
    }

    pub fn args(&self) -> Option<&Value> {
        self.value.get("args")
    }

    pub fn directory(&self) -> Option<&str> {
        self.text("directory")
    }

    pub fn cwd(&self) -> Option<&str> {
        self.text("cwd")
    }

    pub fn parts(&self) -> Parts {
        Parts {
            notice: self
                .value
                .pointer("/parts/notice")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            guidance: self
                .value
                .pointer("/parts/guidance")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        }
    }

    fn text(&self, name: &str) -> Option<&str> {
        self.value.get(name).and_then(Value::as_str)
    }
}

#[derive(Serialize)]
struct ToolOutput<'a> {
    addition: &'a str,
}

#[derive(Serialize)]
struct SystemOutput<'a> {
    system: &'a str,
    bodies: &'a [String],
}

#[derive(Serialize)]
struct EnvironmentOutput<'a> {
    owner: Option<&'a str>,
}

#[derive(Serialize)]
struct EmptyOutput {}

pub fn tool_response(addition: &str) -> serde_json::Result<String> {
    serde_json::to_string(&ToolOutput { addition })
}

pub fn system_response(system: &str, bodies: &[String]) -> serde_json::Result<String> {
    serde_json::to_string(&SystemOutput { system, bodies })
}

pub fn environment_response(owner: Option<&str>) -> serde_json::Result<String> {
    serde_json::to_string(&EnvironmentOutput { owner })
}

pub fn empty_response() -> serde_json::Result<String> {
    serde_json::to_string(&EmptyOutput {})
}
