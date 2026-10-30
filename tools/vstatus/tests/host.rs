//! `vstatus` against the real host: run the binary with `--tty` pointed
//! at a file, then replay what it wrote as a pane's output — inside a
//! portal, as vmux would forward it — through `veter-host`'s PRT engine,
//! and read back the `StatusChange` events the pane's client would get.

use prt_protocol::apc::ApcStream;
use prt_protocol::codec::Reader;
use prt_protocol::command::{
    AnchorMode, Command, CreatePortalBody, StatusLevel, StatusOp, StatusProgress, WritePortalBody,
};
use prt_protocol::encode::build_envelope;
use prt_protocol::envelope::parse_status_change;
use prt_protocol::frame::{EVT_STATUS_CHANGE, MARKER_T2C};
use veter_host::prt::PrtEngine;

/// Run `vstatus <args>` writing to a fresh file; return the bytes and
/// the exit code.
fn vstatus(args: &[&str]) -> (Vec<u8>, i32) {
    let dir = std::env::temp_dir().join(format!("vstatus-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let tty = dir.join(format!("tty-{}", args.join("_").replace('/', "_")));
    std::fs::write(&tty, b"").unwrap();
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_vstatus"))
        .arg("--tty")
        .arg(&tty)
        .args(args)
        .status()
        .unwrap();
    (std::fs::read(&tty).unwrap(), status.code().unwrap())
}

/// What the client of portal "pane" hears when the pane prints `output`.
fn status_events(output: &[u8]) -> Vec<(String, StatusOp)> {
    let mut engine = PrtEngine::new();
    let create = Command::CreatePortal(CreatePortalBody {
        id: "pane".into(),
        size_w: 80,
        size_h: 24,
        origin_x: 0,
        origin_y: 0,
        anchor_mode: AnchorMode::Live,
        is_visible: true,
        draw_order: 0,
        flags: 0,
        scrollback_lines: 0,
    });
    let write = Command::WritePortal(WritePortalBody {
        id: "pane".into(),
        data: output.to_vec(),
    });
    engine.process_pty_chunk(&build_envelope(&[(create, 1), (write, 2)]));

    let mut apc = ApcStream::with_marker(*MARKER_T2C);
    let mut events = Vec::new();
    for payload in apc.feed(&engine.take_responses()).into_payloads() {
        let mut r = Reader::new(&payload);
        r.u8().unwrap();
        r.u32().unwrap();
        while !r.at_end() {
            let kind = r.u8().unwrap();
            r.u32().unwrap();
            let len = r.u32().unwrap() as usize;
            let body = r.take(len).unwrap();
            if kind == EVT_STATUS_CHANGE {
                events.push(parse_status_change(body).unwrap());
            }
        }
    }
    events
}

fn ops(output: &[u8]) -> Vec<StatusOp> {
    status_events(output)
        .into_iter()
        .map(|(id, op)| {
            assert_eq!(id, "pane");
            op
        })
        .collect()
}

#[test]
fn set_and_clear_reach_the_pane_s_client() {
    let (set, code) = vstatus(&["set", "-k", "dl", "-l", "warn", "-p", "42", "ubuntu", "iso"]);
    assert_eq!(code, 0);
    let (clear, _) = vstatus(&["clear", "--prefix", "-k", "d"]);
    let (all, _) = vstatus(&["clear", "--all"]);
    let mut output = b"some program output\r\n".to_vec();
    output.extend(&set);
    output.extend(&clear);
    output.extend(&all);
    assert_eq!(
        ops(&output),
        vec![
            StatusOp::Set {
                key: "dl".into(),
                level: StatusLevel::Warn,
                progress: Some(StatusProgress::Percent(42)),
                text: "ubuntu iso".into(),
            },
            StatusOp::Remove {
                key: "d".into(),
                prefix: true
            },
            StatusOp::Remove {
                key: String::new(),
                prefix: true
            },
        ]
    );
}

#[test]
fn run_clears_on_success_and_leaves_an_error_on_failure() {
    let (ok, code) = vstatus(&["run", "--spin", "--", "true"]);
    assert_eq!(code, 0);
    let run = |text: &str, level, progress| StatusOp::Set {
        key: "run".into(),
        level,
        progress,
        text: text.into(),
    };
    assert_eq!(
        ops(&ok),
        vec![
            run(
                "true",
                StatusLevel::Info,
                Some(StatusProgress::Indeterminate)
            ),
            StatusOp::Remove {
                key: "run".into(),
                prefix: false
            },
        ]
    );

    let (failed, code) = vstatus(&["run", "-t", "build", "--", "sh", "-c", "exit 3"]);
    assert_eq!(code, 3);
    assert_eq!(
        ops(&failed),
        vec![
            run("build", StatusLevel::Info, None),
            run("build — exit 3", StatusLevel::Error, None),
        ]
    );
}

#[test]
fn without_a_veter_pane_in_sight_nothing_is_written() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_vstatus"))
        .args(["set", "hello"])
        .env_remove("VMUX_PANE_TTY")
        .env_remove("VETER")
        .env_remove("SSH_CONNECTION")
        .env_remove("SSH_TTY")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
}

/// Run `vstatus claude-hook` with `event` on stdin, as Claude Code does;
/// return what it wrote to the tty, its exit code and its stdout+stderr.
fn claude_hook(name: &str, event: &str) -> (Vec<u8>, i32, Vec<u8>) {
    use std::io::Write;
    let dir = std::env::temp_dir().join(format!("vstatus-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let tty = dir.join(format!("hook-{name}"));
    std::fs::write(&tty, b"").unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_vstatus"))
        .arg("--tty")
        .arg(&tty)
        .arg("claude-hook")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(event.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let mut said = out.stdout;
    said.extend(out.stderr);
    (
        std::fs::read(&tty).unwrap(),
        out.status.code().unwrap(),
        said,
    )
}

#[test]
fn claude_hook_shows_the_tool_and_clears_on_stop() {
    let (pre, code, said) = claude_hook(
        "pre",
        r#"{"hook_event_name":"PreToolUse","cwd":"/w","tool_name":"Edit",
            "tool_input":{"file_path":"/w/src/lib.rs"}}"#,
    );
    assert_eq!(code, 0);
    assert!(said.is_empty());
    let (stop, _, _) = claude_hook("stop", r#"{"hook_event_name":"Stop"}"#);
    let mut output = pre;
    output.extend(&stop);
    assert_eq!(
        ops(&output),
        vec![
            StatusOp::Set {
                key: "claude.state".into(),
                level: StatusLevel::Info,
                progress: Some(StatusProgress::Indeterminate),
                text: "Edit: src/lib.rs".into(),
            },
            StatusOp::Remove {
                key: "claude.state".into(),
                prefix: false
            },
        ]
    );
}

/// A `PreToolUse` hook that exits 2 blocks the tool, so nothing the hook
/// is fed may make it fail or speak.
#[test]
fn claude_hook_never_fails_and_never_speaks() {
    for (name, event) in [
        ("garbage", "not json"),
        ("empty", ""),
        ("unknown", r#"{"hook_event_name":"X"}"#),
    ] {
        let (wrote, code, said) = claude_hook(name, event);
        assert_eq!(code, 0, "{name}");
        assert!(wrote.is_empty() && said.is_empty(), "{name}");
    }
}
