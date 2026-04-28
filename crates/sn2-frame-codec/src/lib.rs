use anyhow::{bail, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_FRAME_SIZE: usize = 512 * 1024 * 1024;

pub async fn read_frame<R: AsyncRead + Unpin>(stream: &mut R) -> Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 {
        return Ok(Some(Vec::new()));
    }
    if len > MAX_FRAME_SIZE {
        bail!("frame size {len} exceeds maximum {MAX_FRAME_SIZE}");
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

pub async fn write_frame<W: AsyncWrite + Unpin>(stream: &mut W, data: &[u8]) -> Result<()> {
    let len: u32 = data
        .len()
        .try_into()
        .map_err(|_| anyhow::anyhow!("frame payload {} bytes exceeds u32::MAX", data.len()))?;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(data).await?;
    stream.flush().await?;
    Ok(())
}
