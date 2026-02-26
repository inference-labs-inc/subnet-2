use anyhow::{Context, Result};
use ndarray::{ArrayD, IxDyn};
use sn2_types::json_tensor::{flatten_json_to_f64, infer_json_shape};

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
