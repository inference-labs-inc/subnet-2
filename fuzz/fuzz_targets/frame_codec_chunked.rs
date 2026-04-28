#![no_main]

use libfuzzer_sys::fuzz_target;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::runtime::Builder;

struct ChunkedReader<'a> {
    data: &'a [u8],
    chunk_size: usize,
    idx: usize,
    off: usize,
}

impl<'a> ChunkedReader<'a> {
    fn new(data: &'a [u8], chunk_size: usize) -> Self {
        Self {
            data,
            chunk_size: chunk_size.max(1),
            idx: 0,
            off: 0,
        }
    }
}

impl AsyncRead for ChunkedReader<'_> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let chunk_size = self.chunk_size;
        let chunk_start = self.idx.saturating_mul(chunk_size);
        if chunk_start >= self.data.len() {
            return Poll::Ready(Ok(()));
        }
        let chunk_end = chunk_start.saturating_add(chunk_size).min(self.data.len());
        let chunk_len = chunk_end - chunk_start;
        let remaining = &self.data[chunk_start + self.off..chunk_end];
        let n = remaining.len().min(buf.remaining());
        buf.put_slice(&remaining[..n]);
        self.off += n;
        if self.off >= chunk_len {
            self.idx += 1;
            self.off = 0;
        }
        Poll::Ready(Ok(()))
    }
}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let chunk_size = (data[0] as usize) % 17 + 1;
    let body = &data[1..];
    let Ok(rt) = Builder::new_current_thread().build() else {
        return;
    };
    rt.block_on(async {
        let mut reader = ChunkedReader::new(body, chunk_size);
        for _ in 0..32 {
            match sn2_frame_codec::read_frame(&mut reader).await {
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(_) => break,
            }
        }
    });
});
