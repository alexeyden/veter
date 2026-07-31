//! Everything JSON cannot carry literally.
//!
//! Two `Vec<u8>` fields exist across the protocols this tool speaks —
//! `UploadImage.data` (VGE §8.2) and `WritePortal.data` (PRT §6.9) — and
//! serde would render either as an array of integers, which for a 4 MB
//! image is not a serialization so much as a denial of service. Both
//! accept `data_file` or `data_base64` instead, rewritten here into the
//! real bytes before the command reaches serde.
//!
//! The rewrite lives in this crate rather than behind the protocol
//! crates' serde derive on purpose: reading a file is I/O, and the wire
//! crates do none.
//!
//! On top of that sits `UploadImageFile`, a synthetic command with no
//! wire counterpart. It expands into however many real `UploadImage`
//! frames the chunk cap requires, so a script never computes offsets.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value, json};

use crate::proto::{Proto, unb64};

/// Bytes per `UploadImage` chunk. Comfortably under the 1 MiB
/// `max_write_bytes` a `WritePortal` relay imposes, so an upload sent
/// into a pane is never rejected for being one oversized write.
const CHUNK: usize = 128 * 1024;

/// Rewrite a command array in place: expand `UploadImageFile`, and
/// resolve `data_file` / `data_base64` on the two byte-carrying
/// commands. Returns the expanded array.
pub fn expand(proto: Proto, commands: Vec<Value>) -> Result<Vec<Value>> {
    let mut out = Vec::with_capacity(commands.len());
    for (i, cmd) in commands.into_iter().enumerate() {
        let ctx = || format!("command[{i}]");
        match single_key(&cmd) {
            Some("UploadImageFile") if proto == Proto::Vge => {
                let body = cmd["UploadImageFile"].clone();
                out.extend(expand_image_file(&body).with_context(ctx)?);
            }
            Some("UploadImage") | Some("WritePortal") => {
                out.push(resolve_bytes(cmd).with_context(ctx)?);
            }
            _ => out.push(cmd),
        }
    }
    Ok(out)
}

/// The single key of a `{"Name": {...}}` command object, if it has
/// exactly one. Unit commands serialize as a bare string (`"Probe"`),
/// which has no key and needs no rewriting.
fn single_key(v: &Value) -> Option<&str> {
    let obj = v.as_object()?;
    if obj.len() != 1 {
        return None;
    }
    obj.keys().next().map(String::as_str)
}

/// Replace `data_file` / `data_base64` in a command body with a real
/// `data` array. Leaves an explicit `data` alone, so the literal form
/// still works for small hand-written payloads.
fn resolve_bytes(cmd: Value) -> Result<Value> {
    let Value::Object(mut outer) = cmd else {
        return Ok(cmd);
    };
    let key = outer.keys().next().expect("checked by caller").clone();
    let Some(Value::Object(body)) = outer.get_mut(&key) else {
        return Ok(Value::Object(outer));
    };

    let from_file = body.remove("data_file");
    let from_b64 = body.remove("data_base64");
    let bytes = match (from_file, from_b64) {
        (Some(_), Some(_)) => bail!("data_file and data_base64 are mutually exclusive"),
        (Some(p), None) => {
            let path = p.as_str().ok_or_else(|| anyhow!("data_file must be a string"))?;
            std::fs::read(path).with_context(|| format!("read data_file {path}"))?
        }
        (None, Some(b)) => {
            unb64(b.as_str().ok_or_else(|| anyhow!("data_base64 must be a string"))?)?
        }
        (None, None) => return Ok(Value::Object(outer)),
    };
    if body.contains_key("data") {
        bail!("`data` is already present; drop it or drop data_file/data_base64");
    }
    body.insert("data".into(), json!(bytes));
    Ok(Value::Object(outer))
}

/// Turn one `UploadImageFile` into the `UploadImage` frames it stands
/// for: decode the file, optionally fit it to a cell footprint, encode,
/// and split at [`CHUNK`].
fn expand_image_file(body: &Value) -> Result<Vec<Value>> {
    let obj = body
        .as_object()
        .ok_or_else(|| anyhow!("UploadImageFile body must be an object"))?;
    let id = str_field(obj, "id")?;
    let path = str_field(obj, "path")?;

    let img = image::open(path)
        .with_context(|| format!("decode {path}"))?
        .to_rgba8();
    let (mut w, mut h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        bail!("{path} has a zero dimension");
    }

    // Optional explicit pixel target. Callers that want cell-based
    // fitting run `vproto measure` first and pass the result here, so
    // the arithmetic has exactly one implementation.
    if let Some(px) = obj.get("target_px") {
        let a = px
            .as_array()
            .filter(|a| a.len() == 2)
            .ok_or_else(|| anyhow!("target_px must be [width, height] in pixels"))?;
        w = a[0].as_u64().ok_or_else(|| anyhow!("target_px[0]"))? as u32;
        h = a[1].as_u64().ok_or_else(|| anyhow!("target_px[1]"))? as u32;
        if w == 0 || h == 0 {
            bail!("target_px components must be non-zero");
        }
    }

    let rgba = if (w, h) == (img.width(), img.height()) {
        img.into_raw()
    } else {
        image::imageops::resize(&img, w, h, image::imageops::FilterType::Lanczos3).into_raw()
    };

    let enc = encoding_from(obj)?;
    let (enc_byte, payload) = vge_render::encode_payload(rgba, w, h, enc)?;

    let retention = obj
        .get("retention")
        .cloned()
        .unwrap_or_else(|| json!("Auto"));
    let total = payload.len() as u32;
    let n = payload.len().div_ceil(CHUNK).max(1);
    let mut frames = Vec::with_capacity(n);
    for i in 0..n {
        let start = i * CHUNK;
        let end = ((i + 1) * CHUNK).min(payload.len());
        frames.push(json!({ "UploadImage": {
            "id": id,
            "retention": retention,
            "encoding": enc_byte,
            "width": w,
            "height": h,
            "total_bytes": total,
            "chunk_offset": start as u32,
            "is_last": i == n - 1,
            "data": &payload[start..end],
        }}));
    }
    Ok(frames)
}

fn encoding_from(obj: &Map<String, Value>) -> Result<vge_render::Encoding> {
    let name = obj.get("encoding").and_then(Value::as_str).unwrap_or("webp-lossy");
    // Quality is the encoder's 0..=100 scale. A caller passing 0.85 for
    // "85%" gets a legal value and an unusably bad image, so reject the
    // range that can only be a mistake.
    let quality = obj.get("quality").and_then(Value::as_f64).unwrap_or(85.0) as f32;
    if quality > 0.0 && quality < 1.0 {
        bail!("quality is 0..=100, not a fraction — {quality} is almost certainly meant as {}", quality * 100.0);
    }
    Ok(match name {
        "raw" => vge_render::Encoding::Raw,
        "webp-lossless" => vge_render::Encoding::WebpLossless,
        "webp-lossy" => vge_render::Encoding::WebpLossy(quality),
        other => bail!("unknown encoding {other:?} (raw | webp-lossless | webp-lossy)"),
    })
}

fn str_field<'a>(obj: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    obj.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("UploadImageFile needs a string `{key}`"))
}
