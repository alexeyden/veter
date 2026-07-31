//! vproto — speak veter's protocols from a script.
//!
//! One JSON array of commands in, one APC envelope out, the terminal's
//! reply back as JSON. It replaces the hand-written `vge-cli` and
//! `prt-cli`, which were task-shaped (`create-rect`, `fill-polygon`)
//! rather than command-shaped and so covered whatever subset someone
//! needed that day — 11 of VGE's 15 commands, with no way to reach the
//! cursor or marker anchors at all. Deserializing straight into the
//! protocol crates' own types means the surface is the wire format by
//! construction and cannot drift from it.
//!
//! What that costs is discoverability: seventeen clap subcommands
//! documented themselves and a JSON blob does not. `vproto schema`
//! exists to pay it back, generated from the same types.

mod payload;
mod proto;
mod tty;

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use proto::Proto;

#[derive(Parser, Debug)]
#[command(
    about = "Speak veter's protocols from a script: JSON commands in, JSON responses out",
    long_about = None,
    after_help = "Commands are read from stdin as a JSON array. See `vproto schema` for their shape."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Verb,
}

#[derive(Subcommand, Debug)]
enum Verb {
    /// Send a JSON command array to the terminal and print the reply.
    Send(SendArgs),
    /// Encode a JSON command array to raw envelope bytes, sending
    /// nothing. Useful for nesting one protocol inside another —
    /// a VGE envelope becomes the `data_file` of a PRT WritePortal.
    Emit(EmitArgs),
    /// Report the cell footprint an image would occupy, without
    /// drawing it. An application must reserve rows before an image can
    /// be placed into them, so it needs the number first.
    Measure(MeasureArgs),
    /// Print the terminal's cell metrics and limits as JSON.
    Caps(CapsArgs),
    /// Print the JSON Schema for a protocol's commands.
    Schema(SchemaArgs),
}

#[derive(Parser, Debug)]
struct SendArgs {
    #[command(flatten)]
    common: Common,
    /// Target tty. Defaults to `$VMUX_PANE_TTY`, then `/dev/tty`.
    #[arg(long)]
    tty: Option<PathBuf>,
    /// Stamp every frame `REQ_ID_NO_RESPONSE` and read nothing back.
    ///
    /// Required whenever this process is not the target pane's
    /// foreground program: a reply would land in that program's input
    /// queue, and whoever the kernel wakes reads it.
    #[arg(long)]
    no_response: bool,
    /// How long to wait for the terminal's reply.
    #[arg(long, default_value_t = 500)]
    timeout_ms: u64,
}

#[derive(Parser, Debug)]
struct EmitArgs {
    #[command(flatten)]
    common: Common,
    /// Write to this file instead of stdout.
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Stamp every frame `REQ_ID_NO_RESPONSE`, as `send --no-response`
    /// would. Off by default so `emit` and `send` produce identical
    /// bytes for identical input.
    #[arg(long)]
    no_response: bool,
}

#[derive(Parser, Debug)]
struct Common {
    /// Which protocol the commands belong to. One envelope carries one
    /// protocol's frames, so this applies to the whole array.
    #[arg(long, value_enum, default_value = "vge")]
    proto: Proto,
    /// Read commands from this file instead of stdin.
    #[arg(long)]
    file: Option<PathBuf>,
}

#[derive(Parser, Debug)]
struct MeasureArgs {
    /// Image to measure.
    #[arg(long)]
    image: PathBuf,
    /// Cap the height in rows, scaling the image down (aspect
    /// preserved) rather than letting it overrun.
    #[arg(long)]
    max_rows: Option<u32>,
    /// Force a width in cells instead of using the natural size.
    #[arg(long)]
    width_cells: Option<u32>,
    #[arg(long)]
    tty: Option<PathBuf>,
}

#[derive(Parser, Debug)]
struct CapsArgs {
    #[arg(long)]
    tty: Option<PathBuf>,
}

#[derive(Parser, Debug)]
struct SchemaArgs {
    #[arg(long, value_enum, default_value = "vge")]
    proto: Proto,
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Verb::Send(a) => send(a),
        Verb::Emit(a) => emit(a),
        Verb::Measure(a) => measure(a),
        Verb::Caps(a) => caps(a),
        Verb::Schema(a) => schema(a),
    }
}

/// Read the command array and turn it into one envelope.
fn build(common: &Common, no_response: bool) -> Result<Vec<u8>> {
    let text = match &common.file {
        Some(p) => std::fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?,
        None => {
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .context("read commands from stdin")?;
            s
        }
    };
    if text.trim().is_empty() {
        bail!("no commands on stdin (expected a JSON array)");
    }
    let parsed: Value = serde_json::from_str(&text).context("commands are not valid JSON")?;
    let array = match parsed {
        Value::Array(a) => a,
        // A bare object is a common slip and the fix is obvious, so say
        // so rather than letting serde complain about a type mismatch.
        other => bail!(
            "expected a JSON array of commands, got {}. Wrap it: [{}]",
            kind_of(&other),
            serde_json::to_string(&other).unwrap_or_default()
        ),
    };

    let expanded = payload::expand(common.proto, array)?;
    let ids = assign_ids(&expanded, no_response);
    proto::encode(common.proto, &expanded, &ids)
}

/// Resolve each frame's request id: an explicit `request_id` wins,
/// otherwise they run 1, 2, 3… so a reply can be matched to the command
/// that caused it. `--no-response` overrides everything with the
/// sentinel, which tells the host to stay silent (§4).
fn assign_ids(commands: &[Value], no_response: bool) -> Vec<u32> {
    let sentinel = vge_protocol::frame::REQ_ID_NO_RESPONSE;
    let mut next = 1u32;
    commands
        .iter()
        .map(|c| {
            if no_response {
                return sentinel;
            }
            let explicit = c
                .as_object()
                .and_then(|o| o.values().next())
                .and_then(|b| b.get("request_id"))
                .and_then(Value::as_u64);
            match explicit {
                Some(id) => id as u32,
                None => {
                    let id = next;
                    next += 1;
                    id
                }
            }
        })
        .collect()
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn send(a: SendArgs) -> Result<()> {
    let envelope = build(&a.common, a.no_response)?;
    let path = tty::resolve(a.tty.as_deref())?;
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;

    // Raw mode goes on *before* the write: a reply can arrive while we
    // are still in the foreground, and cooked mode would echo it.
    let want_reply = !a.no_response;
    let _raw = if want_reply { Some(tty::RawTty::enable()?) } else { None };

    out.write_all(&envelope).context("write envelope")?;
    out.flush()?;

    let mut report = json!({ "proto": a.common.proto.as_str(), "sent_bytes": envelope.len() });
    let mut failed = false;
    if want_reply {
        match tty::read_response(a.common.proto, Duration::from_millis(a.timeout_ms))? {
            Some(frames) => {
                let mut responses = Vec::new();
                let mut events = Vec::new();
                for f in &frames {
                    let v = proto::frame_to_json(a.common.proto, f);
                    // Event frame types are the 0x80.. range in every
                    // protocol that has them; they carry no request id
                    // the caller asked for.
                    if f.kind >= 0x80 { events.push(v) } else {
                        failed |= v.get("error").is_some();
                        responses.push(v)
                    }
                }
                report["responses"] = Value::Array(responses);
                report["events"] = Value::Array(events);
            }
            None => {
                report["responses"] = Value::Array(vec![]);
                report["events"] = Value::Array(vec![]);
                report["timeout_ms"] = json!(a.timeout_ms);
            }
        }
    }
    println!("{}", serde_json::to_string_pretty(&report)?);
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

fn emit(a: EmitArgs) -> Result<()> {
    let envelope = build(&a.common, a.no_response)?;
    match &a.output {
        Some(p) => std::fs::write(p, &envelope).with_context(|| format!("write {}", p.display()))?,
        None => {
            std::io::stdout().write_all(&envelope)?;
            std::io::stdout().flush()?;
        }
    }
    Ok(())
}

fn measure(a: MeasureArgs) -> Result<()> {
    let path = tty::resolve(a.tty.as_deref())?;
    let f = std::fs::File::options()
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    let caps = tty::Caps::probe(&f)?;

    let img = image::open(&a.image).with_context(|| format!("decode {}", a.image.display()))?;
    let (w_px, h_px) = (img.width(), img.height());
    if w_px == 0 || h_px == 0 {
        bail!("image has a zero dimension");
    }

    let p = fit(
        w_px,
        h_px,
        &caps,
        a.width_cells,
        a.max_rows.unwrap_or(u32::MAX),
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "rows": p.h_cells,
            "cols": p.w_cells,
            "target_px": [p.target_px_w, p.target_px_h],
            "rect_h": p.target_rect_h,
            "cell_px": caps.cell_px,
            "pane": { "cols": caps.cols, "rows": caps.rows },
        }))?
    );
    Ok(())
}

/// Fit an image to the pane, then shrink it until it also fits
/// `max_rows`. `compute_placement` clamps width but leaves height
/// unbounded, so a tall image would otherwise overrun the rows the
/// application reserved and paint over whatever follows. Width is the
/// only lever — height follows from it and the aspect ratio.
fn fit(
    w_px: u32,
    h_px: u32,
    caps: &tty::Caps,
    forced_w: Option<u32>,
    max_rows: u32,
) -> vge_render::Placement {
    let compute = |w: Option<u32>| {
        vge_render::compute_placement(
            w_px,
            h_px,
            caps.cell_px[0] as f32,
            caps.cell_px[1] as f32,
            caps.cols as u32,
            w,
        )
    };
    let p = compute(forced_w);
    if p.h_cells <= max_rows {
        return p;
    }
    // One proportional step lands close; the loop walks off the last
    // cell or two, since ceil() makes the relationship non-linear.
    // Bounded by the width, so it always terminates.
    let scaled = (p.w_cells as f32 * max_rows as f32 / p.target_rect_h).floor() as u32;
    let mut w = scaled.clamp(1, p.w_cells);
    loop {
        let q = compute(Some(w));
        if q.h_cells <= max_rows || w <= 1 {
            return q;
        }
        w -= 1;
    }
}

fn caps(a: CapsArgs) -> Result<()> {
    let path = tty::resolve(a.tty.as_deref())?;
    let f = std::fs::File::options()
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    println!("{}", serde_json::to_string_pretty(&tty::Caps::probe(&f)?)?);
    Ok(())
}

fn schema(a: SchemaArgs) -> Result<()> {
    let s = match a.proto {
        Proto::Vge => schemars::schema_for!(Vec<vge_protocol::command::Command>),
        Proto::Prt => schemars::schema_for!(Vec<prt_protocol::command::Command>),
        Proto::Ses => schemars::schema_for!(Vec<ses_protocol::Command>),
    };
    println!("{}", serde_json::to_string_pretty(&s)?);
    Ok(())
}
