#![no_main]

use libfuzzer_sys::fuzz_target;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::runtime::Builder;

struct ChunkedReader<'a> {
    chunks: Vec<&'a [u8]>,
    idx: usize,
    off: usize,
}

impl<'a> ChunkedReader<'a> {
    fn new(data: &'a [u8], chunk_size: usize) -> Self {
        let chunk_size = chunk_size.max(1);
        let mut chunks = Vec::new();
        let mut start = 0;
        while start < data.len() {
            let end = (start + chunk_size).min(data.len());
            chunks.push(&data[start..end]);
            start = end;
        }
        Self {
            chunks,
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
        if self.idx >= self.chunks.len() {
            return Poll::Ready(Ok(()));
        }
        let chunk = self.chunks[self.idx];
        let remaining = &chunk[self.off..];
        let n = remaining.len().min(buf.remaining());
        buf.put_slice(&remaining[..n]);
        self.off += n;
        if self.off >= chunk.len() {
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
    let rt = Builder::new_current_thread().build().unwrap();
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
