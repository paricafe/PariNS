//! DNS over TCP framing, shared by downstream and upstream connections.
//! The caller owns the deadline for the entire frame, not each partial read.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub async fn read_frame(stream: &mut (impl AsyncRead + Unpin)) -> io::Result<Vec<u8>> {
    let length = stream.read_u16().await? as usize;
    if length < 12 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "DNS frame shorter than header",
        ));
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    Ok(bytes)
}

pub async fn write_frame(stream: &mut (impl AsyncWrite + Unpin), bytes: &[u8]) -> io::Result<()> {
    let length = u16::try_from(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "DNS frame exceeds 65535 bytes"))?;
    stream.write_u16(length).await?;
    stream.write_all(bytes).await
}
