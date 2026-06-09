use crate::json_tensor::{flatten_json_to_f64, infer_json_shape};
use anyhow::{Context, Result};
use flate2::read::GzDecoder;
use ndarray::{ArrayD, IxDyn};
use std::io::Read;

const MAX_TENSOR_ELEMENTS: usize = 16_777_216;
const MAX_PROTOBUF_BYTES: usize = 128 * 1024 * 1024;
const MAX_PROTO_SHAPE_DIMS: usize = 32;
pub const MSGPACK_MAX_DEPTH: usize = 64;

pub fn decode_gzipped_protobuf_tensor(gzipped: &[u8], shape: &[usize]) -> Result<ArrayD<f64>> {
    let expected = shape
        .iter()
        .try_fold(1usize, |acc, &dim| acc.checked_mul(dim))
        .context("shape overflow")?;
    anyhow::ensure!(
        expected <= MAX_TENSOR_ELEMENTS,
        "tensor shape {shape:?} has {expected} elements, max is {MAX_TENSOR_ELEMENTS}"
    );
    let max_raw_len = expected
        .checked_mul(5)
        .and_then(|n| n.checked_add(1024))
        .context("shape overflow")?;
    anyhow::ensure!(
        max_raw_len <= MAX_PROTOBUF_BYTES,
        "protobuf payload size exceeds {MAX_PROTOBUF_BYTES} bytes"
    );
    anyhow::ensure!(
        gzipped.len() <= MAX_PROTOBUF_BYTES,
        "compressed input length {} exceeds limit",
        gzipped.len()
    );

    let decoder = GzDecoder::new(gzipped);
    let mut raw = Vec::new();
    decoder
        .take(max_raw_len as u64 + 1)
        .read_to_end(&mut raw)
        .context("gunzip")?;
    anyhow::ensure!(
        raw.len() <= max_raw_len,
        "decompressed protobuf exceeds max size for shape {shape:?}"
    );

    parse_protobuf_floats(&raw, shape, expected)
}

fn parse_protobuf_floats(raw: &[u8], shape: &[usize], expected: usize) -> Result<ArrayD<f64>> {
    let mut floats: Vec<f64> = Vec::with_capacity(expected);
    let mut proto_shape: Vec<usize> = Vec::new();
    let mut proto_shape_seen = false;
    let mut offset = 0;
    while offset < raw.len() {
        let (tag, next) = read_varint(raw, offset)?;
        offset = next;
        let field = tag >> 3;
        let wire_type = tag & 0x07;

        match wire_type {
            2 => {
                offset = parse_length_delimited(
                    raw,
                    offset,
                    field,
                    expected,
                    &mut floats,
                    &mut proto_shape,
                    &mut proto_shape_seen,
                )?;
            }
            5 => {
                offset = parse_fixed32(raw, offset, field, expected, &mut floats)?;
            }
            0 => {
                offset = parse_varint_field(
                    raw,
                    offset,
                    field,
                    &mut proto_shape,
                    &mut proto_shape_seen,
                )?;
            }
            1 => {
                offset = skip_fixed64(raw, offset)?;
            }
            _ => anyhow::bail!("unknown wire type {wire_type}"),
        }
    }

    if proto_shape_seen {
        anyhow::ensure!(
            proto_shape == shape,
            "protobuf shape {proto_shape:?} does not match declared shape {shape:?}"
        );
    }

    anyhow::ensure!(
        floats.len() == expected,
        "protobuf tensor has {} floats but shape {shape:?} expects {expected}",
        floats.len()
    );
    ArrayD::from_shape_vec(IxDyn(shape), floats).context("building array from protobuf")
}

fn parse_length_delimited(
    raw: &[u8],
    offset: usize,
    field: usize,
    expected: usize,
    floats: &mut Vec<f64>,
    proto_shape: &mut Vec<usize>,
    proto_shape_seen: &mut bool,
) -> Result<usize> {
    let (len, next) = read_varint(raw, offset)?;
    let mut offset = next;
    let end = offset
        .checked_add(len)
        .context("length-delimited field overflow")?;
    anyhow::ensure!(end <= raw.len(), "length-delimited field overflows buffer");
    if field == 1 {
        anyhow::ensure!(
            len % 4 == 0,
            "packed float field length {len} not a multiple of 4"
        );
        let max_new = len / 4;
        let projected = floats.len().saturating_add(max_new);
        anyhow::ensure!(
            projected <= expected,
            "packed float field would push tensor past declared size: have {} + {} > {}",
            floats.len(),
            max_new,
            expected
        );
        floats.reserve(max_new);
        while offset < end {
            let val = f32::from_le_bytes([
                raw[offset],
                raw[offset + 1],
                raw[offset + 2],
                raw[offset + 3],
            ]);
            floats.push(val as f64);
            offset += 4;
        }
    } else if field == 2 {
        *proto_shape_seen = true;
        let mut pos = offset;
        while pos < end {
            let (val, next) = read_varint(raw, pos)?;
            anyhow::ensure!(next <= end, "packed shape varint crosses field boundary");
            anyhow::ensure!(
                proto_shape.len() < MAX_PROTO_SHAPE_DIMS,
                "protobuf shape exceeds {MAX_PROTO_SHAPE_DIMS} dims"
            );
            proto_shape.push(val);
            pos = next;
        }
    }
    Ok(end)
}

fn parse_fixed32(
    raw: &[u8],
    offset: usize,
    field: usize,
    expected: usize,
    floats: &mut Vec<f64>,
) -> Result<usize> {
    let end = offset.checked_add(4).context("fixed32 overflow")?;
    anyhow::ensure!(end <= raw.len(), "fixed32 overflows buffer");
    if field == 1 {
        anyhow::ensure!(
            floats.len() < expected,
            "unpacked float would push tensor past declared size {expected}"
        );
        let val = f32::from_le_bytes([
            raw[offset],
            raw[offset + 1],
            raw[offset + 2],
            raw[offset + 3],
        ]);
        floats.push(val as f64);
    }
    Ok(end)
}

fn parse_varint_field(
    raw: &[u8],
    offset: usize,
    field: usize,
    proto_shape: &mut Vec<usize>,
    proto_shape_seen: &mut bool,
) -> Result<usize> {
    let (val, next) = read_varint(raw, offset)?;
    if field == 2 {
        *proto_shape_seen = true;
        anyhow::ensure!(
            proto_shape.len() < MAX_PROTO_SHAPE_DIMS,
            "protobuf shape exceeds {MAX_PROTO_SHAPE_DIMS} dims"
        );
        proto_shape.push(val);
    }
    Ok(next)
}

fn skip_fixed64(raw: &[u8], offset: usize) -> Result<usize> {
    let end = offset.checked_add(8).context("fixed64 overflow")?;
    anyhow::ensure!(end <= raw.len(), "fixed64 overflows buffer");
    Ok(end)
}

fn read_varint(buf: &[u8], offset: usize) -> Result<(usize, usize)> {
    let mut result: usize = 0;
    let mut shift: u32 = 0;
    let mut pos = offset;
    while pos < buf.len() {
        let byte = buf[pos];
        pos += 1;
        let payload = (byte & 0x7f) as usize;
        let chunk = payload
            .checked_shl(shift)
            .filter(|c| (c >> shift) == payload)
            .context("varint exceeds platform usize")?;
        result = result.checked_add(chunk).context("varint overflow")?;
        if byte & 0x80 == 0 {
            return Ok((result, pos));
        }
        shift = shift
            .checked_add(7)
            .context("varint exceeds platform usize")?;
        anyhow::ensure!(shift < usize::BITS, "varint exceeds platform usize");
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
    let expected: usize = shape
        .iter()
        .try_fold(1usize, |acc, &dim| acc.checked_mul(dim))
        .context("shape overflow")?;
    anyhow::ensure!(
        expected <= MAX_TENSOR_ELEMENTS,
        "tensor shape {shape:?} has {expected} elements, max is {MAX_TENSOR_ELEMENTS}"
    );
    anyhow::ensure!(
        flat.len() == expected,
        "shape {shape:?} expects {expected} elements but got {}",
        flat.len()
    );
    ArrayD::from_shape_vec(IxDyn(&shape), flat).context("building array from shape")
}

fn f64_to_json(v: f64) -> serde_json::Value {
    serde_json::Number::from_f64(v)
        .map(serde_json::Value::Number)
        .unwrap_or(serde_json::Value::Null)
}

pub fn arrayd_to_json(arr: &ArrayD<f64>) -> serde_json::Value {
    if arr.ndim() == 0 {
        return f64_to_json(arr.first().copied().unwrap_or(0.0));
    }
    let data: Vec<f64> = match arr.as_slice() {
        Some(s) => s.to_vec(),
        None => arr.iter().copied().collect(),
    };
    build_nested(&data, arr.shape(), 0)
}

fn build_nested(data: &[f64], shape: &[usize], dim: usize) -> serde_json::Value {
    if dim == shape.len() - 1 {
        return serde_json::Value::Array(data.iter().map(|&v| f64_to_json(v)).collect());
    }
    let stride: usize = shape[dim + 1..].iter().product();
    serde_json::Value::Array(
        (0..shape[dim])
            .map(|i| build_nested(&data[i * stride..(i + 1) * stride], shape, dim + 1))
            .collect(),
    )
}

pub fn arrayd_to_msgpack_value(arr: &ArrayD<f64>) -> rmpv::Value {
    if arr.ndim() == 0 {
        return rmpv::Value::F64(arr.first().copied().unwrap_or(0.0));
    }
    let data: Vec<f64> = match arr.as_slice() {
        Some(s) => s.to_vec(),
        None => arr.iter().copied().collect(),
    };
    build_nested_rmpv(&data, arr.shape(), 0)
}

fn build_nested_rmpv(data: &[f64], shape: &[usize], dim: usize) -> rmpv::Value {
    if dim == shape.len() - 1 {
        return rmpv::Value::Array(data.iter().map(|&v| rmpv::Value::F64(v)).collect());
    }
    let stride: usize = shape[dim + 1..].iter().product();
    rmpv::Value::Array(
        (0..shape[dim])
            .map(|i| build_nested_rmpv(&data[i * stride..(i + 1) * stride], shape, dim + 1))
            .collect(),
    )
}

pub fn encode_msgpack_value(value: &rmpv::Value) -> bytes::Bytes {
    let mut buf = Vec::new();
    rmpv::encode::write_value(&mut buf, value).expect("writing to Vec<u8> is infallible");
    bytes::Bytes::from(buf)
}

pub fn input_data_payload(arr: &ArrayD<f64>) -> bytes::Bytes {
    let map = rmpv::Value::Map(vec![(
        rmpv::Value::String("input_data".into()),
        arrayd_to_msgpack_value(arr),
    )]);
    encode_msgpack_value(&map)
}

pub fn decode_msgpack_value(bytes: &[u8]) -> anyhow::Result<rmpv::Value> {
    rmpv::decode::read_value_with_max_depth(&mut &bytes[..], MSGPACK_MAX_DEPTH)
        .context("decoding msgpack value")
}

pub fn decode_msgpack_to_json(bytes: &[u8]) -> anyhow::Result<serde_json::Value> {
    let mut deserializer = rmp_serde::Deserializer::from_read_ref(bytes);
    deserializer.set_max_depth(MSGPACK_MAX_DEPTH);
    serde::Deserialize::deserialize(&mut deserializer).context("decoding msgpack to json")
}
