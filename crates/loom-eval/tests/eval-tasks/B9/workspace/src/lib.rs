//! Reading a child stream to EOF under a byte cap.

use tokio::io::AsyncReadExt;

/// Leading bytes kept per stream.
pub const STREAM_HEAD_BYTES: usize = 4096;

/// Trailing bytes kept per stream: the band of a log where a failing
/// command's diagnosis usually lands.
pub const STREAM_TAIL_BYTES: usize = 4096;

/// Reads a stream to EOF, keeping at most [`STREAM_HEAD_BYTES`] and
/// discarding the rest so the writer cannot block on a full pipe.
pub async fn read_capped<R>(mut reader: R) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut kept = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        if kept.len() < STREAM_HEAD_BYTES {
            let room = STREAM_HEAD_BYTES - kept.len();
            kept.extend_from_slice(&chunk[..room.min(read)]);
        }
    }
    Ok(kept)
}
