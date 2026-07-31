//! Round-trip: JSON in, envelope bytes out, decoded back through the
//! protocol crate's own parser, re-serialized to JSON, compared.
//!
//! That full loop is what the hand-written `vge-cli` tests could not
//! do. They asserted on hand-decoded bytes for the nine commands
//! somebody wrote cases for, which is why four VGE commands and both
//! non-viewport anchors went untested for as long as they went
//! unimplemented. Here the loop closes through serde on both ends, so a
//! command that survives it is provably reachable from a script.

use serde_json::{Value, json};

/// Run `vproto emit` on a command array and return the envelope bytes.
fn emit(proto: &str, commands: &Value) -> Vec<u8> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut ch = Command::new(env!("CARGO_BIN_EXE_vproto"))
        .args(["emit", "--proto", proto])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn vproto");
    ch.stdin
        .as_mut()
        .unwrap()
        .write_all(commands.to_string().as_bytes())
        .unwrap();
    let out = ch.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "vproto emit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// Same, but expect failure and return stderr.
fn emit_err(proto: &str, commands: &Value) -> String {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut ch = Command::new(env!("CARGO_BIN_EXE_vproto"))
        .args(["emit", "--proto", proto])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn vproto");
    ch.stdin
        .as_mut()
        .unwrap()
        .write_all(commands.to_string().as_bytes())
        .unwrap();
    let out = ch.wait_with_output().unwrap();
    assert!(!out.status.success(), "expected vproto to reject this input");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Unwrap an envelope into `(frame_type, request_id, body)` triples.
fn frames(bytes: &[u8]) -> Vec<(u8, u32, Vec<u8>)> {
    let mut apc = vge_protocol::apc::ApcStream::with_marker(*b"VGE");
    let mut out = apc.feed(bytes);
    if out.payloads.is_empty() {
        // Not VGE — try the other two markers.
        for m in [b"PRT", b"SES"] {
            let mut s = vge_protocol::apc::ApcStream::with_marker(*m);
            out = s.feed(bytes);
            if !out.payloads.is_empty() {
                break;
            }
        }
    }
    let payload = out.payloads.first().expect("no envelope emitted").clone();

    let mut r = vge_protocol::codec::Reader::new(&payload);
    let _version = r.u8().unwrap();
    let _len = r.u32().unwrap();
    let mut frames = Vec::new();
    while !r.at_end() {
        let ty = r.u8().unwrap();
        let req = r.u32().unwrap();
        let n = r.u32().unwrap() as usize;
        frames.push((ty, req, r.take(n).unwrap().to_vec()));
    }
    frames
}

/// JSON → bytes → `Command` → JSON. The result must equal the input.
fn vge_roundtrip(cmd: Value) {
    let bytes = emit("vge", &json!([cmd]));
    let f = frames(&bytes);
    assert_eq!(f.len(), 1, "expected exactly one frame");
    let decoded = vge_protocol::command::parse(f[0].0, &f[0].2)
        .unwrap_or_else(|e| panic!("host parser rejected our own encoding: err {e:#x}"));
    let back = serde_json::to_value(&decoded).unwrap();
    assert_eq!(back, cmd, "round-trip changed the command");
}

fn prt_roundtrip(cmd: Value) {
    let bytes = emit("prt", &json!([cmd]));
    let f = frames(&bytes);
    assert_eq!(f.len(), 1);
    let decoded = prt_protocol::command::parse(f[0].0, &f[0].2)
        .unwrap_or_else(|e| panic!("host parser rejected our own encoding: err {e:#x}"));
    let back = serde_json::to_value(&decoded).unwrap();
    assert_eq!(back, cmd, "round-trip changed the command");
}

fn element(anchor: Value) -> Value {
    json!({"CreateElement": {
        "id": "t.el",
        "commands": [{"FillRectangles": {
            "fill": {"Flat": {"r": 1.0, "g": 0.0, "b": 0.0, "a": 1.0}},
            "rects": [{"x": 0.0, "y": 0.0, "w": 4.0, "h": 2.0}]
        }}],
        "origin": {"x": 0.0, "y": 0.0},
        "is_visible": true,
        "draw_order": 0,
        "parent": null,
        "size": null,
        "transform": null,
        "anchor": anchor,
    }})
}

#[test]
fn every_origin_anchor_survives_the_round_trip() {
    // The gap that motivated this tool: `vge-cli` hardcoded Viewport on
    // every create path, so neither of the other two anchors could be
    // exercised from a script at all.
    vge_roundtrip(element(json!("Viewport")));
    vge_roundtrip(element(json!("Cursor")));
    vge_roundtrip(element(json!({"Marker": "[IMAGE: chart1]"})));
}

#[test]
fn commands_the_old_cli_could_not_emit() {
    // UpdateTransform (§9.11), UpdateText, UpdateCommand and
    // UpdateCommands had no CLI surface — four of VGE's fifteen.
    vge_roundtrip(json!({"UpdateTransform": {
        "id": "t.el",
        "transform": {"a": 0.0, "b": 1.0, "c": -1.0, "d": 0.0, "e": 0.5, "f": 1.5}
    }}));
    vge_roundtrip(json!({"UpdateText": {
        "id": "t.el", "command_index": 0, "range": "Whole", "replacement": "hello"
    }}));
    vge_roundtrip(json!({"UpdateText": {
        "id": "t.el", "command_index": 0,
        "range": {"Range": {"start": 1, "end": 4}}, "replacement": "ell"
    }}));
    vge_roundtrip(json!({"UpdateSize": {
        "id": "t.el", "new_size": {"x": 10.0, "y": 4.0}
    }}));
}

#[test]
fn prt_commands_round_trip() {
    prt_roundtrip(json!({"DeletePortal": {"id": "pane1"}}));
    prt_roundtrip(json!({"UpdateVisibility": {"id": "pane1", "is_visible": false}}));
    prt_roundtrip(json!({"SetCursorStyle": {"unfocused": "Dim"}}));
}

#[test]
fn request_ids_auto_assign_in_order() {
    let bytes = emit(
        "vge",
        &json!([{"Probe": null}, {"Probe": null}, {"Probe": null}]),
    );
    let ids: Vec<u32> = frames(&bytes).iter().map(|f| f.1).collect();
    assert_eq!(ids, vec![1, 2, 3], "ids must let a reply be matched to its command");
}

#[test]
fn no_response_stamps_the_sentinel() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut ch = Command::new(env!("CARGO_BIN_EXE_vproto"))
        .args(["emit", "--no-response"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    ch.stdin
        .as_mut()
        .unwrap()
        .write_all(json!([{"Probe": null}]).to_string().as_bytes())
        .unwrap();
    let out = ch.wait_with_output().unwrap();
    let ids: Vec<u32> = frames(&out.stdout).iter().map(|f| f.1).collect();
    assert_eq!(ids, vec![vge_protocol::frame::REQ_ID_NO_RESPONSE]);
}

#[test]
fn byte_payloads_come_from_a_file_or_base64() {
    // A `Vec<u8>` field as a JSON integer array is unusable at image
    // sizes, so both byte-carrying commands take an indirection.
    let dir = std::env::temp_dir().join("vproto-bytes");
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("inner.bin");
    std::fs::write(&f, b"\x1b_VGEinner\x1b\\").unwrap();

    let via_file = emit(
        "prt",
        &json!([{"WritePortal": {"id": "p", "data_file": f.to_str().unwrap()}}]),
    );
    let via_b64 = emit(
        "prt",
        &json!([{"WritePortal": {"id": "p", "data_base64": "G19WR0Vpbm5lchtc"}}]),
    );
    assert_eq!(via_file, via_b64, "the two spellings must produce one encoding");

    // And the decoded body really is the file's bytes.
    let fr = frames(&via_file);
    let decoded = prt_protocol::command::parse(fr[0].0, &fr[0].2).unwrap();
    match decoded {
        prt_protocol::command::Command::WritePortal(b) => {
            assert_eq!(b.data, b"\x1b_VGEinner\x1b\\");
        }
        other => panic!("expected WritePortal, got {other:?}"),
    }
}

#[test]
fn upload_image_file_expands_into_chunked_uploads() {
    let dir = std::env::temp_dir().join("vproto-img");
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("t.png");
    let mut img = image::RgbaImage::new(64, 32);
    for (x, y, p) in img.enumerate_pixels_mut() {
        *p = image::Rgba([(x * 4) as u8, (y * 8) as u8, 200, 255]);
    }
    img.save(&png).unwrap();

    let bytes = emit(
        "vge",
        &json!([{"UploadImageFile": {
            "id": "t.pic", "path": png.to_str().unwrap(), "encoding": "raw"
        }}]),
    );
    let f = frames(&bytes);
    assert!(!f.is_empty(), "no upload frames emitted");
    let last = vge_protocol::command::parse(f[f.len() - 1].0, &f[f.len() - 1].2).unwrap();
    match last {
        vge_protocol::command::Command::UploadImage(b) => {
            assert_eq!((b.width, b.height), (64, 32));
            assert!(b.is_last, "the final chunk must be marked");
            assert_eq!(b.total_bytes as usize, 64 * 32 * 4, "raw RGBA8 size");
        }
        other => panic!("expected UploadImage, got {other:?}"),
    }
}

#[test]
fn a_fractional_quality_is_refused() {
    // 0.85 is a legal value on the encoder's 0..=100 scale and produces
    // the worst image WebP can make. Whoever writes it means 85.
    let dir = std::env::temp_dir().join("vproto-img");
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("q.png");
    image::RgbaImage::new(8, 8).save(&png).unwrap();

    let err = emit_err(
        "vge",
        &json!([{"UploadImageFile": {
            "id": "t.q", "path": png.to_str().unwrap(), "quality": 0.85
        }}]),
    );
    assert!(
        err.contains("0..=100"),
        "the error must name the scale, got: {err}"
    );
}

#[test]
fn a_bare_object_is_diagnosed_not_just_rejected() {
    // The likeliest first mistake, and serde's own message for it is
    // about types rather than about the missing brackets.
    let err = emit_err("vge", &json!({"Probe": null}));
    assert!(err.contains("array"), "unhelpful error: {err}");
}
