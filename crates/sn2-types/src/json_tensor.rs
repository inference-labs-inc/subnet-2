pub fn flatten_json_to_f64(value: &serde_json::Value) -> Vec<f64> {
    match value {
        serde_json::Value::Number(n) => vec![n.as_f64().unwrap_or(0.0)],
        serde_json::Value::Array(arr) => arr.iter().flat_map(flatten_json_to_f64).collect(),
        _ => vec![],
    }
}

pub fn infer_json_shape(value: &serde_json::Value) -> Vec<usize> {
    let mut shape = Vec::new();
    let mut current = value;
    while let serde_json::Value::Array(arr) = current {
        shape.push(arr.len());
        match arr.first() {
            Some(first) => current = first,
            None => break,
        }
    }
    shape
}
