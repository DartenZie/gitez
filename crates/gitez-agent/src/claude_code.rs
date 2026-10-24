//! Claude Code, run headless: `claude -p` with the prompt on standard input
//! and its stream-JSON events on standard output.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::session::{Session, find_program};
use crate::{Agent, AgentEvent, Error, Finished, Result};

const PROGRAM: &str = "claude";

/// Arguments of every run. It cannot ask for permission while running
/// headless, so it runs with all permissions (decided with the owner,
/// see CLAUDE.md).
const ARGS: [&str; 6] = [
    "--print",
    "--output-format",
    "stream-json",
    "--verbose",
    "--permission-mode",
    "bypassPermissions",
];

/// Claude Code's command-line agent.
#[derive(Debug, Clone, Default)]
pub struct ClaudeCode {
    /// Where `claude` is, when not found on the `PATH`.
    program: Option<PathBuf>,
}

impl ClaudeCode {
    pub const ID: &'static str = "claude-code";

    pub fn new() -> Self {
        Self::default()
    }

    /// Runs `program` instead of looking for `claude`.
    pub fn with_program(program: impl Into<PathBuf>) -> Self {
        Self {
            program: Some(program.into()),
        }
    }

    /// Where `claude` is: on the `PATH`, or where its installers put it. An
    /// app started from the Finder does not get the shell's `PATH`.
    fn program(&self) -> Result<PathBuf> {
        if let Some(program) = &self.program {
            return Ok(program.clone());
        }
        let candidates: Vec<PathBuf> = std::env::var_os("HOME")
            .map(PathBuf::from)
            .into_iter()
            .flat_map(|home| {
                [
                    home.join(".local/bin/claude"),
                    home.join(".claude/local/claude"),
                ]
            })
            .chain(["/opt/homebrew/bin/claude", "/usr/local/bin/claude"].map(PathBuf::from))
            .collect();
        find_program(PROGRAM, std::env::var_os("PATH").as_deref(), &candidates).ok_or_else(|| {
            Error::NotInstalled {
                name: "Claude Code",
                program: PROGRAM,
                searched: candidates
                    .iter()
                    .map(|candidate| candidate.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            }
        })
    }
}

impl Agent for ClaudeCode {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn name(&self) -> &'static str {
        "Claude Code"
    }

    fn start(&self, prompt: &str, dir: &Path) -> Result<Session> {
        Session::start(&self.program()?, &ARGS, dir, prompt)
    }

    fn parse(&self, line: &str) -> Option<AgentEvent> {
        parse(line)
    }
}

/// The event a line of `claude -p --output-format stream-json` reports.
fn parse(line: &str) -> Option<AgentEvent> {
    let event: Value = serde_json::from_str(line).ok()?;
    let text = |value: &Value| value.as_str().map(str::to_owned);
    match event.get("type")?.as_str()? {
        "system" if event.get("subtype")?.as_str()? == "init" => Some(AgentEvent::Started {
            model: event.get("model").and_then(text),
        }),
        "assistant" => {
            // One content block per message in practice; the first that
            // shows something is reported.
            content(&event)?
                .iter()
                .find_map(|block| match block.get("type")?.as_str()? {
                    "text" => {
                        let text = block.get("text")?.as_str()?.trim();
                        (!text.is_empty()).then(|| AgentEvent::Text(text.to_owned()))
                    }
                    "tool_use" => Some(AgentEvent::Tool {
                        name: block.get("name")?.as_str()?.to_owned(),
                        detail: tool_detail(block.get("input")),
                    }),
                    _ => None,
                })
        }
        "user" => content(&event)?.iter().find_map(|block| {
            let failed = block.get("type")?.as_str()? == "tool_result"
                && block.get("is_error")?.as_bool()?;
            failed.then(|| AgentEvent::ToolFailed(first_line(&result_text(block.get("content")))))
        }),
        "result" => Some(AgentEvent::Finished(Finished {
            success: event.get("subtype").and_then(Value::as_str) == Some("success")
                && !event
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            text: event.get("result").and_then(text).unwrap_or_default(),
            cost_usd: event.get("total_cost_usd").and_then(Value::as_f64),
            turns: event.get("num_turns").and_then(Value::as_u64),
            duration_ms: event.get("duration_ms").and_then(Value::as_u64),
        })),
        _ => None,
    }
}

fn content(event: &Value) -> Option<&Vec<Value>> {
    event.get("message")?.get("content")?.as_array()
}

/// What a tool was used on: the command, file, pattern or description.
fn tool_detail(input: Option<&Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    [
        "command",
        "file_path",
        "notebook_path",
        "pattern",
        "url",
        "description",
    ]
    .iter()
    .find_map(|key| input.get(*key)?.as_str())
    .map(first_line)
    .unwrap_or_default()
}

/// A tool result's text: a string, or the text of its blocks.
fn result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text")?.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn first_line(text: &str) -> String {
    text.trim().lines().next().unwrap_or_default().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_of_a_run_are_parsed() {
        let lines = [
            r#"{"type":"system","subtype":"hook_started","hook_id":"x"}"#,
            r#"{"type":"system","subtype":"init","cwd":"/r","model":"claude-opus-5-5","tools":["Bash"]}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test\n--quiet","description":"Run tests"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"tool_use_id":"t1","type":"tool_result","content":"ok","is_error":false}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"Edit","input":{"file_path":"src/a.rs","old_string":"a","new_string":"b"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"tool_use_id":"t2","type":"tool_result","content":[{"type":"text","text":"File has not been read yet.\nRead it first."}],"is_error":true}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":""}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Done.\n"}]}}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Done.","total_cost_usd":0.25,"num_turns":4,"duration_ms":4729}"#,
            "not json",
        ];
        let events: Vec<AgentEvent> = lines.iter().filter_map(|line| parse(line)).collect();
        assert_eq!(
            events,
            [
                AgentEvent::Started {
                    model: Some("claude-opus-5-5".to_owned())
                },
                AgentEvent::Tool {
                    name: "Bash".to_owned(),
                    detail: "cargo test".to_owned()
                },
                AgentEvent::Tool {
                    name: "Edit".to_owned(),
                    detail: "src/a.rs".to_owned()
                },
                AgentEvent::ToolFailed("File has not been read yet.".to_owned()),
                AgentEvent::Text("Done.".to_owned()),
                AgentEvent::Finished(Finished {
                    success: true,
                    text: "Done.".to_owned(),
                    cost_usd: Some(0.25),
                    turns: Some(4),
                    duration_ms: Some(4729),
                }),
            ]
        );
    }

    #[test]
    fn an_error_result_is_not_a_success() {
        let line = r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#;
        let Some(AgentEvent::Finished(finished)) = parse(line) else {
            panic!("a result");
        };
        assert!(!finished.success);
        assert_eq!(finished.text, "");
    }
}
