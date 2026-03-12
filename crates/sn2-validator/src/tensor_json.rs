use anyhow::{Context, Result};
use base64::Engine;
use flate2::read::GzDecoder;
use ndarray::{ArrayD, IxDyn};
use sn2_types::json_tensor::{flatten_json_to_f64, infer_json_shape};
use std::io::Read;

pub fn decode_protobuf_tensor(b64: &str, shape: &[usize]) -> Result<ArrayD<f64>> {
    let compressed = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .context("base64 decode")?;
    let mut decoder = GzDecoder::new(&compressed[..]);
    let mut raw = Vec::new();
    decoder.read_to_end(&mut raw).context("gunzip")?;

    let mut floats: Vec<f64> = Vec::new();
    let mut offset = 0;
    while offset < raw.len() {
        let (tag, next) = read_varint(&raw, offset)?;
        offset = next;
        let field = tag >> 3;
        let wire_type = tag & 0x07;

        match wire_type {
            2 => {
                let (len, next) = read_varint(&raw, offset)?;
                offset = next;
                if field == 1 {
                    let end = offset + len;
                    anyhow::ensure!(end <= raw.len(), "packed field overflows buffer");
                    while offset + 4 <= end {
                        let val = f32::from_le_bytes([
                            raw[offset],
                            raw[offset + 1],
                            raw[offset + 2],
                            raw[offset + 3],
                        ]);
                        floats.push(val as f64);
                        offset += 4;
                    }
                    offset = end;
                } else {
                    offset += len;
                }
            }
            5 => {
                anyhow::ensure!(offset + 4 <= raw.len(), "fixed32 overflows buffer");
                if field == 1 {
                    let val = f32::from_le_bytes([
                        raw[offset],
                        raw[offset + 1],
                        raw[offset + 2],
                        raw[offset + 3],
                    ]);
                    floats.push(val as f64);
                }
                offset += 4;
            }
            0 => {
                let (_, next) = read_varint(&raw, offset)?;
                offset = next;
            }
            1 => {
                offset += 8;
            }
            _ => anyhow::bail!("unknown wire type {wire_type}"),
        }
    }

    let expected: usize = shape.iter().product();
    anyhow::ensure!(
        floats.len() == expected,
        "protobuf tensor has {} floats but shape {shape:?} expects {expected}",
        floats.len()
    );
    ArrayD::from_shape_vec(IxDyn(shape), floats).context("building array from protobuf")
}

fn read_varint(buf: &[u8], offset: usize) -> Result<(usize, usize)> {
    let mut result: usize = 0;
    let mut shift = 0;
    let mut pos = offset;
    while pos < buf.len() {
        let byte = buf[pos];
        pos += 1;
        result |= ((byte & 0x7f) as usize) << shift;
        if byte & 0x80 == 0 {
            return Ok((result, pos));
        }
        shift += 7;
        anyhow::ensure!(shift < 64, "varint too long");
    }
    anyhow::bail!("unterminated varint")
}

pub fn json_to_arrayd(value: &serde_json::Value) -> Result<ArrayD<f64>> {
    let flat = flatten_json_to_f64(value);
    let shape = infer_json_shape(value);
    if shape.is_empty() {
        anyhow::ensure!(
            flat.len() == 1,
            "scalar expected but got {} values",
            flat.len()
        );
        return ArrayD::from_shape_vec(IxDyn(&[]), flat).context("building 0-d array");
    }
    let expected: usize = shape.iter().product();
    anyhow::ensure!(
        flat.len() == expected,
        "shape {shape:?} expects {expected} elements but got {}",
        flat.len()
    );
    ArrayD::from_shape_vec(IxDyn(&shape), flat).context("building array from shape")
}

pub fn arrayd_to_json(arr: &ArrayD<f64>) -> serde_json::Value {
    if arr.ndim() == 0 {
        return serde_json::json!(arr.first().copied().unwrap_or(0.0));
    }
    let data: Vec<f64> = match arr.as_slice() {
        Some(s) => s.to_vec(),
        None => arr.iter().copied().collect(),
    };
    build_nested(&data, arr.shape(), 0)
}

fn build_nested(data: &[f64], shape: &[usize], dim: usize) -> serde_json::Value {
    if dim == shape.len() - 1 {
        return serde_json::Value::Array(data.iter().map(|&v| serde_json::json!(v)).collect());
    }
    let stride: usize = shape[dim + 1..].iter().product();
    serde_json::Value::Array(
        (0..shape[dim])
            .map(|i| build_nested(&data[i * stride..(i + 1) * stride], shape, dim + 1))
            .collect(),
    )
}
