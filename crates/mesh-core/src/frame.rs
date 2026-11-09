//! Length-prefixed frames: a `u32` big-endian length followed by `mesh_proto::encode` bytes.

use std::io;

use mesh_proto::ProtoError;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest frame on `sync/1`: 1 MiB of ops plus 1 KiB of envelope.
pub const SYNC_MAX_FRAME: usize = 1_049_600;
/// Largest frame on `join/1`.
pub const JOIN_MAX_FRAME: usize = 262_144;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("frame of {0} bytes exceeds the limit")]
    TooLarge(usize),
    #[error("protocol error: {0}")]
    Proto(#[from] ProtoError),
}

pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(
    w: &mut W,
    msg: &T,
) -> io::Result<()> {
    let body = mesh_proto::encode(msg);
    let len = u32::try_from(body.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(&body);
    w.write_all(&buf).await?;
    w.flush().await
}

/// `Ok(None)` on a clean EOF before the first byte of a length prefix.
pub async fn read_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(
    r: &mut R,
    max: usize,
) -> Result<Option<T>, FrameError> {
    let mut prefix = [0u8; 4];
    let mut got = 0;
    while got < prefix.len() {
        let n = r.read(&mut prefix[got..]).await?;
        if n == 0 {
            if got == 0 {
                return Ok(None);
            }
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
        }
        got += n;
    }
    let len = u32::from_be_bytes(prefix) as usize;
    if len > max {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(Some(mesh_proto::decode(&body)?))
}
