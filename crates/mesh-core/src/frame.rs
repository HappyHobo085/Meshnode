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
/// The body buffer starts at most this large and grows only as bytes arrive, so a length
/// prefix alone never reserves the claimed size.
const INITIAL_BODY: usize = 64 * 1024;

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
    let mut body = Vec::new();
    read_body(r, len, &mut body).await?;
    Ok(Some(mesh_proto::decode(&body)?))
}

/// Reads exactly `len` bytes into `body`, growing it as they arrive. A stream that ends
/// first is `UnexpectedEof`.
async fn read_body<R: AsyncRead + Unpin>(
    r: &mut R,
    len: usize,
    body: &mut Vec<u8>,
) -> io::Result<()> {
    body.reserve(len.min(INITIAL_BODY));
    let n = r.take(len as u64).read_to_end(body).await?;
    if n < len {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn short_body_is_eof_without_reserving_the_claimed_length() {
        let claimed = 64 * 1024 * 1024;
        let mut body = Vec::new();
        let err = read_body(&mut &b"0123456789"[..], claimed, &mut body)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(body, b"0123456789");
        assert!(body.capacity() <= 2 * INITIAL_BODY, "{}", body.capacity());
    }

    #[tokio::test]
    async fn frame_claiming_the_cap_then_eof_fails() {
        let max = 64 * 1024 * 1024;
        let mut wire = (max as u32).to_be_bytes().to_vec();
        wire.extend_from_slice(&[1, 2, 3]);
        let r = read_frame::<_, String>(&mut &wire[..], max).await;
        assert!(
            matches!(&r, Err(FrameError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof),
            "{r:?}"
        );
    }

    #[tokio::test]
    async fn body_larger_than_the_first_buffer_reads_whole() {
        let msg = vec![7u8; 3 * INITIAL_BODY];
        let mut wire = Vec::new();
        write_frame(&mut wire, &msg).await.unwrap();
        let got: Vec<u8> = read_frame(&mut &wire[..], 4 * INITIAL_BODY)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got, msg);
    }
}
