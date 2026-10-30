//! `vstatus claude-hook`: Claude Code's hook events as a pane status.
//!
//! Registered for several events in `~/.claude/settings.json`, it reads
//! the event Claude Code writes to its stdin and sets one item, keyed
//! [`KEY`], to what the session is doing — thinking, running a tool,
//! waiting on a permission prompt — and clears it when the turn ends.
//!
//! A hook's exit status is an instruction to Claude Code (a `PreToolUse`
//! exiting 2 blocks the tool), and its stdout can be one too. This one
//! has nothing to say about the session, so whatever happens it prints
//! nothing and exits 0: a status line is never worth a blocked tool.

use prt_protocol::command::{StatusLevel, StatusOp, StatusProgress};
use serde_json::Value;

use crate::one_line;

/// The item the session's state is shown as. One key, so each event
/// replaces the last rather than piling up beside it.
pub const KEY: &str = "claude.state";
/// Every key this hook writes starts with this, so a session's end
/// clears them all, whatever a later version adds.
pub const PREFIX: &str = "claude.";

/// What `event` (a hook's stdin, parsed) does to the pane's status, or
/// `None` for an event that changes nothing.
pub fn op_for(event: &Value) -> Option<StatusOp> {
    let busy = |text: String| StatusOp::Set {
        key: KEY.into(),
        level: StatusLevel::Info,
        progress: Some(StatusProgress::Indeterminate),
        text: one_line(&text),
    };
    let clear = |key: &str, prefix: bool| StatusOp::Remove {
        key: key.into(),
        prefix,
    };
    let str_of = |field: &str| event.get(field).and_then(Value::as_str);
    match str_of("hook_event_name")? {
        // A new session (or a resumed one) starts with nothing left over
        // from a previous one that died without its `SessionEnd`.
        "SessionStart" | "SessionEnd" => Some(clear(PREFIX, true)),
        "UserPromptSubmit" | "PostToolUse" | "PostToolUseFailure" | "SubagentStop" => {
            Some(busy("thinking…".into()))
        }
        "PreToolUse" => Some(busy(tool_label(
            str_of("tool_name").unwrap_or("tool"),
            event.get("tool_input").unwrap_or(&Value::Null),
            str_of("cwd"),
        ))),
        "PreCompact" => Some(busy("compacting…".into())),
        "Notification" => notification(str_of("notification_type"), str_of("message")?),
        "Stop" => Some(clear(KEY, false)),
        _ => None,
    }
}

/// A notification is Claude Code asking for the user, which is what a
/// warning is for — except the idle reminder, which only says a
/// finished turn is still finished.
fn notification(kind: Option<&str>, message: &str) -> Option<StatusOp> {
    let idle = match kind {
        Some(kind) => kind == "idle_prompt",
        // Builds that predate `notification_type`.
        None => message.contains("waiting for your input"),
    };
    (!idle).then(|| StatusOp::Set {
        key: KEY.into(),
        level: StatusLevel::Warn,
        progress: None,
        text: one_line(message),
    })
}

/// "Bash: run the tests", "Edit: src/main.rs": the tool, and the one
/// argument that says what it is doing to what.
fn tool_label(tool: &str, input: &Value, cwd: Option<&str>) -> String {
    let arg = |field: &str| {
        input
            .get(field)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    let path = |field: &str| arg(field).map(|p| relative(p, cwd));
    let detail = match tool {
        // The description is what Claude says the command is for; the
        // command itself is often a screenful.
        "Bash" => arg("description").or(arg("command")).map(str::to_owned),
        "Read" | "Edit" | "MultiEdit" | "Write" => path("file_path"),
        "NotebookEdit" => path("notebook_path"),
        "Grep" | "Glob" => arg("pattern").map(str::to_owned),
        "WebFetch" => arg("url").map(str::to_owned),
        "WebSearch" => arg("query").map(str::to_owned),
        "Task" | "Agent" => arg("description").map(str::to_owned),
        _ => None,
    };
    // `mcp__server__tool` → `server: tool`.
    let name = match tool.strip_prefix("mcp__") {
        Some(rest) => rest.replacen("__", ": ", 1),
        None => tool.to_owned(),
    };
    match detail {
        Some(d) => format!("{name}: {d}"),
        None => name,
    }
}

/// `path` relative to the session's directory when it is under it — the
/// part of the path that tells files apart.
fn relative(path: &str, cwd: Option<&str>) -> String {
    cwd.and_then(|cwd| path.strip_prefix(cwd.trim_end_matches('/')))
        .and_then(|rest| rest.strip_prefix('/'))
        .filter(|rest| !rest.is_empty())
        .unwrap_or(path)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(op: Option<StatusOp>) -> Option<(StatusLevel, Option<StatusProgress>, String)> {
        match op? {
            StatusOp::Set {
                key,
                level,
                progress,
                text,
            } => {
                assert_eq!(key, KEY);
                Some((level, progress, text))
            }
            StatusOp::Remove { .. } => None,
        }
    }

    #[test]
    fn a_tool_call_names_the_tool_and_what_it_works_on() {
        let pre = |tool: &str, input: Value| {
            text(op_for(&json!({
                "hook_event_name": "PreToolUse",
                "cwd": "/src/veter",
                "tool_name": tool,
                "tool_input": input,
            })))
            .unwrap()
        };
        let (level, progress, t) = pre(
            "Edit",
            json!({"file_path": "/src/veter/tools/vmux/src/main.rs"}),
        );
        assert_eq!(t, "Edit: tools/vmux/src/main.rs");
        assert_eq!(
            (level, progress),
            (StatusLevel::Info, Some(StatusProgress::Indeterminate))
        );
        assert_eq!(
            pre("Read", json!({"file_path": "/etc/hosts"})).2,
            "Read: /etc/hosts"
        );
        assert_eq!(
            pre(
                "Bash",
                json!({"command": "cargo test\n  -p vmux", "description": "Run vmux tests"})
            )
            .2,
            "Bash: Run vmux tests"
        );
        assert_eq!(
            pre("Bash", json!({"command": "cargo test\n  -p vmux"})).2,
            "Bash: cargo test -p vmux"
        );
        assert_eq!(pre("mcp__docs__read", json!({})).2, "docs: read");
        assert_eq!(pre("TodoWrite", json!({"todos": []})).2, "TodoWrite");
    }

    #[test]
    fn a_permission_prompt_warns_and_the_idle_reminder_does_not() {
        let note = |v: Value| op_for(&v);
        let (level, progress, t) = text(note(json!({
            "hook_event_name": "Notification",
            "notification_type": "permission_prompt",
            "message": "Claude needs your permission to use Bash",
        })))
        .unwrap();
        assert_eq!((level, progress), (StatusLevel::Warn, None));
        assert_eq!(t, "Claude needs your permission to use Bash");
        assert!(
            note(json!({
                "hook_event_name": "Notification",
                "notification_type": "idle_prompt",
                "message": "Claude is waiting for your input",
            }))
            .is_none()
        );
        assert!(
            note(json!({
                "hook_event_name": "Notification",
                "message": "Claude is waiting for your input",
            }))
            .is_none()
        );
    }

    #[test]
    fn a_turn_ends_with_its_item_gone_and_a_session_with_all_of_them() {
        assert_eq!(
            op_for(&json!({"hook_event_name": "Stop"})),
            Some(StatusOp::Remove {
                key: KEY.into(),
                prefix: false
            })
        );
        assert_eq!(
            op_for(&json!({"hook_event_name": "SessionEnd"})),
            Some(StatusOp::Remove {
                key: PREFIX.into(),
                prefix: true
            })
        );
        assert!(op_for(&json!({"hook_event_name": "SomethingNew"})).is_none());
        assert!(op_for(&json!({})).is_none());
    }
}
