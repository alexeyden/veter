// Spawn the vge-cli binary, capture stdout, feed it back through the
// vge-protocol parser, and assert we recover the typed Command. This is
// the closest we can get to "actual user runs vge-cli inside vterm" in
// CI.

use std::io::Read;
use std::process::{Command as Proc, Stdio};

use vge_protocol::apc::ApcStream;
use vge_protocol::codec::Reader;
use vge_protocol::command::{parse, Command, DrawCmd, Style};
use vge_protocol::frame::*;

use std::io::Write;

fn run_cli(args: &[&str]) -> Vec<u8> {
    let bin = env!("CARGO_BIN_EXE_vge-cli");
    let mut child = Proc::new(bin)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn vge-cli");
    let mut out = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut out)
        .expect("read stdout");
    let status = child.wait().expect("wait");
    assert!(status.success(), "vge-cli exited with {status:?}");
    out
}

fn parse_envelope(bytes: &[u8]) -> Vec<(u8, u32, Vec<u8>)> {
    let mut s = ApcStream::new();
    let out = s.feed(bytes);
    assert_eq!(out.payloads.len(), 1, "expected exactly one envelope");
    let payload = &out.payloads[0];
    let mut r = Reader::new(payload);
    assert_eq!(r.u8().unwrap(), PROTOCOL_VERSION);
    let _len = r.u32().unwrap();

    let mut frames = Vec::new();
    while !r.at_end() {
        let ty = r.u8().unwrap();
        let req_id = r.u32().unwrap();
        let body_len = r.u32().unwrap() as usize;
        let body = r.take(body_len).unwrap().to_vec();
        frames.push((ty, req_id, body));
    }
    frames
}

#[test]
fn probe_round_trip() {
    let env = run_cli(&["probe"]);
    let frames = parse_envelope(&env);
    assert_eq!(frames.len(), 1);
    let (ty, req_id, body) = &frames[0];
    assert_eq!(*ty, CMD_PROBE);
    assert_eq!(*req_id, 0);
    assert!(body.is_empty());
    let cmd = parse(*ty, body).unwrap();
    assert!(matches!(cmd, Command::Probe));
}

#[test]
fn create_rect_round_trip() {
    let env = run_cli(&[
        "create-rect",
        "myrect",
        "--at",
        "5,3",
        "--size",
        "10,5",
        "--color",
        "ff0000ff",
    ]);
    let frames = parse_envelope(&env);
    assert_eq!(frames.len(), 1);
    let (ty, _, body) = &frames[0];
    assert_eq!(*ty, CMD_CREATE_ELEMENT);
    let cmd = parse(*ty, body).unwrap();
    match cmd {
        Command::CreateElement(b) => {
            assert_eq!(b.id, "myrect");
            assert_eq!(b.commands.len(), 1);
            match &b.commands[0] {
                DrawCmd::FillRectangles { fill, rects } => {
                    assert!(matches!(fill, Style::Flat(_)));
                    assert_eq!(rects.len(), 1);
                    assert_eq!(rects[0].w, 10.0);
                    assert_eq!(rects[0].h, 5.0);
                }
                _ => panic!("expected FillRectangles"),
            }
            assert_eq!(b.origin.x, 5.0);
            assert_eq!(b.origin.y, 3.0);
        }
        _ => panic!("expected CreateElement"),
    }
}

#[test]
fn fill_polygon_round_trip() {
    let env = run_cli(&[
        "fill-polygon",
        "tri",
        "--points",
        "0,0",
        "4,0",
        "2,3",
        "--color",
        "ffaa00ff",
    ]);
    let frames = parse_envelope(&env);
    let (ty, _, body) = &frames[0];
    let cmd = parse(*ty, body).unwrap();
    match cmd {
        Command::CreateElement(b) => match &b.commands[0] {
            DrawCmd::FillPolygon { points, .. } => assert_eq!(points.len(), 3),
            _ => panic!("expected FillPolygon"),
        },
        _ => panic!("expected CreateElement"),
    }
}

#[test]
fn set_style_round_trip() {
    let env = run_cli(&["set-style", "accent", "--color", "00aaffff"]);
    let frames = parse_envelope(&env);
    let (ty, _, body) = &frames[0];
    assert_eq!(*ty, CMD_SET_GLOBAL_STYLE);
    let cmd = parse(*ty, body).unwrap();
    match cmd {
        Command::SetGlobalStyle { id, .. } => assert_eq!(id, "accent"),
        _ => panic!("expected SetGlobalStyle"),
    }
}

#[test]
fn upload_raw_round_trip() {
    let dir = std::env::temp_dir();
    let path = dir.join("vge_cli_upload_raw_round_trip.rgba");
    {
        let mut f = std::fs::File::create(&path).unwrap();
        // 2x2 RGBA: red, green, blue, white.
        f.write_all(&[
            0xFF, 0x00, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFF,
        ])
        .unwrap();
    }
    let env = run_cli(&[
        "upload-raw",
        "demo",
        "--width",
        "2",
        "--height",
        "2",
        "--file",
        path.to_str().unwrap(),
    ]);
    let frames = parse_envelope(&env);
    let (ty, _, body) = &frames[0];
    assert_eq!(*ty, CMD_UPLOAD_IMAGE);
    let cmd = parse(*ty, body).unwrap();
    match cmd {
        Command::UploadImage(b) => {
            assert_eq!(b.id, "demo");
            assert_eq!(b.encoding, 0x01);
            assert_eq!(b.width, 2);
            assert_eq!(b.height, 2);
            assert_eq!(b.data.len(), 16);
        }
        _ => panic!("expected UploadImage"),
    }
}

#[test]
fn create_image_round_trip() {
    let env = run_cli(&[
        "create-image",
        "picture",
        "--image",
        "demo",
        "--at",
        "5,3",
        "--size",
        "8,6",
    ]);
    let frames = parse_envelope(&env);
    let (ty, _, body) = &frames[0];
    let cmd = parse(*ty, body).unwrap();
    match cmd {
        Command::CreateElement(b) => {
            assert_eq!(b.id, "picture");
            match &b.commands[0] {
                DrawCmd::DrawImage {
                    target_rect,
                    image_id,
                } => {
                    assert_eq!(image_id, "demo");
                    assert_eq!(target_rect.w, 8.0);
                    assert_eq!(target_rect.h, 6.0);
                }
                _ => panic!("expected DrawImage"),
            }
            assert_eq!(b.origin.x, 5.0);
            assert_eq!(b.origin.y, 3.0);
        }
        _ => panic!("expected CreateElement"),
    }
}

#[test]
fn drop_image_round_trip() {
    let env = run_cli(&["drop-image", "demo"]);
    let frames = parse_envelope(&env);
    let (ty, _, body) = &frames[0];
    assert_eq!(*ty, CMD_DROP_IMAGE);
    let cmd = parse(*ty, body).unwrap();
    match cmd {
        Command::DropImage { id } => assert_eq!(id, "demo"),
        _ => panic!("expected DropImage"),
    }
}

#[test]
fn linear_gradient_round_trip() {
    let env = run_cli(&[
        "create-rect",
        "grad",
        "--at",
        "0,0",
        "--size",
        "10,10",
        "--linear",
        "0,0:10,0:ff0000ff:0000ffff",
    ]);
    let frames = parse_envelope(&env);
    let (ty, _, body) = &frames[0];
    let cmd = parse(*ty, body).unwrap();
    match cmd {
        Command::CreateElement(b) => match &b.commands[0] {
            DrawCmd::FillRectangles { fill, .. } => {
                assert!(matches!(fill, Style::LinearGradient { .. }));
            }
            _ => panic!("expected FillRectangles"),
        },
        _ => panic!("expected CreateElement"),
    }
}
