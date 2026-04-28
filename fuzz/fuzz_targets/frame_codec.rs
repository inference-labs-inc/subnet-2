#![no_main]

use libfuzzer_sys::fuzz_target;
use std::io::Cursor;
use tokio::runtime::Builder;

fuzz_target!(|data: &[u8]| {
    let Ok(rt) = Builder::new_current_thread().build() else {
        return;
    };
    rt.block_on(async {
        let mut cursor = Cursor::new(data);
        loop {
            match sn2_frame_codec::read_frame(&mut cursor).await {
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(_) => break,
            }
        }
    });
});
