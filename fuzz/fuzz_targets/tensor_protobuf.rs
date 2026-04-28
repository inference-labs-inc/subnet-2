#![no_main]

use libfuzzer_sys::fuzz_target;
use arbitrary::{Arbitrary, Unstructured};

#[derive(Arbitrary, Debug)]
struct Input {
    shape: Vec<u16>,
    payload: Vec<u8>,
}

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let Ok(input) = Input::arbitrary(&mut u) else {
        return;
    };
    if input.shape.len() > 6 {
        return;
    }
    let shape: Vec<usize> = input.shape.iter().map(|d| (*d as usize) % 64).collect();
    let _ = sn2_types::tensor_codec::decode_gzipped_protobuf_tensor(&input.payload, &shape);
});
