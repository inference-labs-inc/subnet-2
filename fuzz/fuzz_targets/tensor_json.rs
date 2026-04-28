#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    let _ = sn2_types::tensor_codec::json_to_arrayd(&value);
    let _shape = sn2_types::json_tensor::infer_json_shape(&value);
    let _flat = sn2_types::json_tensor::flatten_json_to_f64(&value);
});
