//! Control-channel handshake: mode tag + `proto_version` before tarpc.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::MODE_CONTROL;

/// Wire protocol version. Bump when breaking RPC or stream framing changes.
/// v2: adds `exit_code` i64 FAST+INDEXED field to Tantivy schema.
pub const PROTO_VERSION: u32 = 2;

/// Client: write `MODE_CONTROL` + little-endian `PROTO_VERSION`.
pub async fn write_control_client<W: AsyncWrite + Unpin>(w: &mut W) -> io::Result<()> {
    w.write_all(&[MODE_CONTROL]).await?;
    w.write_all(&PROTO_VERSION.to_le_bytes()).await?;
    w.flush().await?;
    Ok(())
}

/// Server: read `proto_version` after the control mode tag was already consumed.
pub async fn read_control_version_after_tag<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<()> {
    let mut ver = [0u8; 4];
    r.read_exact(&mut ver).await?;
    let client_ver = u32::from_le_bytes(ver);
    if client_ver != PROTO_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("proto version mismatch: client={client_ver}, server={PROTO_VERSION}"),
        ));
    }
    Ok(())
}

/// Server: read mode tag and version; reject mismatched clients.
pub async fn read_control_server<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<()> {
    let mut tag = [0u8; 1];
    r.read_exact(&mut tag).await?;
    if tag[0] != MODE_CONTROL {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "expected control mode tag {MODE_CONTROL:#04x}, got {:#04x}",
                tag[0]
            ),
        ));
    }
    read_control_version_after_tag(r).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn handshake_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(64);
        write_control_client(&mut a).await.unwrap();
        read_control_server(&mut b).await.unwrap();
    }

    #[tokio::test]
    async fn rejects_wrong_version() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&[MODE_CONTROL]).await.unwrap();
        a.write_all(&0u32.to_le_bytes()).await.unwrap();
        assert!(read_control_server(&mut b).await.is_err());
    }
}
