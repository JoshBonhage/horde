//! Length-prefixed postcard frames for the render channel.
//!
//! Layout: a 4-byte little-endian length followed by that many postcard bytes.

use anyhow::{anyhow, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Guards against a desynchronised stream turning a garbage length into a huge allocation.
const MAX_FRAME: u32 = 64 * 1024 * 1024;

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let body = postcard::to_stdvec(value)?;
    let len = u32::try_from(body.len()).map_err(|_| anyhow!("frame too large"))?;
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

pub async fn write_frame<W, T>(w: &mut W, value: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let buf = encode(value)?;
    w.write_all(&buf).await?;
    w.flush().await?;
    Ok(())
}

pub async fn read_frame<R, T>(r: &mut R) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_le_bytes(len);
    if len > MAX_FRAME {
        return Err(anyhow!("frame of {len} bytes exceeds the {MAX_FRAME} byte limit"));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    Ok(postcard::from_bytes(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{ClientFrame, Cmd};

    #[tokio::test]
    async fn round_trips_frames_back_to_back() {
        let a = ClientFrame::Input { pane: 7, bytes: b"hello".to_vec() };
        let b = ClientFrame::Command(Cmd::SplitRight);

        let mut buf = Vec::new();
        buf.extend(encode(&a).unwrap());
        buf.extend(encode(&b).unwrap());

        let mut cursor = std::io::Cursor::new(buf);
        let got_a: ClientFrame = read_frame(&mut cursor).await.unwrap();
        let got_b: ClientFrame = read_frame(&mut cursor).await.unwrap();

        assert!(matches!(got_a, ClientFrame::Input { pane: 7, .. }));
        assert!(matches!(got_b, ClientFrame::Command(Cmd::SplitRight)));
    }

    #[tokio::test]
    async fn oversized_length_is_rejected_without_allocating() {
        let mut bytes = (MAX_FRAME + 1).to_le_bytes().to_vec();
        bytes.extend_from_slice(b"junk");
        let mut cursor = std::io::Cursor::new(bytes);
        let err = read_frame::<_, ClientFrame>(&mut cursor).await.unwrap_err();
        assert!(err.to_string().contains("exceeds"), "{err}");
    }

    #[tokio::test]
    async fn truncated_frame_is_an_error_not_a_hang() {
        let mut bytes = 100u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(b"short");
        let mut cursor = std::io::Cursor::new(bytes);
        assert!(read_frame::<_, ClientFrame>(&mut cursor).await.is_err());
    }
}
