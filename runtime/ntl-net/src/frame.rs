//! Length-prefixed CBOR framing.
//!
//! A signal on the wire is a 4-byte big-endian length followed by that many
//! bytes of CBOR. This is the smallest framing that is honest about message
//! boundaries; the specification's magic-bytes header is a Phase 2 concern.

use ntl_core::signal::Signal;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Framing errors.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// The underlying socket failed.
    #[error("transport I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The peer closed the connection cleanly.
    #[error("peer closed the connection")]
    Closed,

    /// The frame claims a length beyond the protocol maximum.
    ///
    /// Checked before allocating, so an attacker cannot induce a large
    /// allocation with a small message.
    #[error("frame claims {claimed} bytes, exceeding the {max} byte maximum")]
    TooLarge {
        /// Length the header claimed.
        claimed: usize,
        /// The protocol maximum.
        max: usize,
    },

    /// The payload was not a decodable signal.
    #[error("malformed signal: {0}")]
    Malformed(String),
}

/// Read one signal.
///
/// The length is validated against [`Signal::MAX_SIZE`] **before** any buffer
/// is allocated: reserving on the strength of an attacker-supplied length is
/// how a framing layer becomes a memory-exhaustion vector.
///
/// # Errors
/// Returns [`FrameError::Closed`] at a clean end of stream, or another
/// variant on failure.
pub async fn read_signal<R>(reader: &mut R) -> Result<Signal, FrameError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(FrameError::Closed),
        Err(e) => return Err(FrameError::Io(e)),
    }

    let claimed = u32::from_be_bytes(len_buf) as usize;
    if claimed > Signal::MAX_SIZE {
        return Err(FrameError::TooLarge {
            claimed,
            max: Signal::MAX_SIZE,
        });
    }
    if claimed == 0 {
        return Err(FrameError::Malformed("zero-length frame".to_string()));
    }

    let mut body = vec![0u8; claimed];
    reader.read_exact(&mut body).await?;
    Signal::decode(&body).map_err(|e| FrameError::Malformed(e.to_string()))
}

/// Write one signal.
///
/// # Errors
/// Returns an error if encoding fails or the socket write fails.
pub async fn write_signal<W>(writer: &mut W, signal: &Signal) -> Result<(), FrameError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let body = signal
        .encode()
        .map_err(|e| FrameError::Malformed(e.to_string()))?;
    let len = u32::try_from(body.len()).map_err(|_| FrameError::TooLarge {
        claimed: body.len(),
        max: Signal::MAX_SIZE,
    })?;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntl_core::signal::NodeId;

    #[tokio::test]
    async fn signal_round_trips_through_a_frame() {
        let signal = Signal::data("test")
            .with_payload(serde_json::json!({"k": "v"}))
            .with_weight(0.7)
            .acknowledged()
            .build_unsigned(NodeId(vec![1u8; 32]));

        let mut buf = Vec::new();
        write_signal(&mut buf, &signal).await.expect("write");

        let mut cursor = std::io::Cursor::new(buf);
        let decoded = read_signal(&mut cursor).await.expect("read");

        assert_eq!(decoded.id, signal.id);
        assert_eq!(decoded.weight, signal.weight);
        assert_eq!(decoded.delivery, signal.delivery);
        assert_eq!(decoded.payload, signal.payload);
    }

    #[tokio::test]
    async fn clean_eof_is_reported_as_closed() {
        let mut cursor = std::io::Cursor::new(Vec::new());
        assert!(matches!(
            read_signal(&mut cursor).await,
            Err(FrameError::Closed)
        ));
    }

    #[tokio::test]
    async fn an_oversized_length_is_refused_before_allocating() {
        // A 4-byte header claiming 4 GiB must not cause a 4 GiB allocation.
        let mut framed = Vec::new();
        framed.extend_from_slice(&u32::MAX.to_be_bytes());
        let mut cursor = std::io::Cursor::new(framed);
        assert!(matches!(
            read_signal(&mut cursor).await,
            Err(FrameError::TooLarge { .. })
        ));
    }

    #[tokio::test]
    async fn a_zero_length_frame_is_refused() {
        let mut framed = Vec::new();
        framed.extend_from_slice(&0u32.to_be_bytes());
        let mut cursor = std::io::Cursor::new(framed);
        assert!(matches!(
            read_signal(&mut cursor).await,
            Err(FrameError::Malformed(_))
        ));
    }

    #[tokio::test]
    async fn garbage_is_reported_as_malformed_not_a_panic() {
        let mut framed = Vec::new();
        framed.extend_from_slice(&8u32.to_be_bytes());
        framed.extend_from_slice(&[0xFF; 8]);
        let mut cursor = std::io::Cursor::new(framed);
        assert!(matches!(
            read_signal(&mut cursor).await,
            Err(FrameError::Malformed(_))
        ));
    }

    #[tokio::test]
    async fn a_truncated_body_is_an_io_error() {
        let mut framed = Vec::new();
        framed.extend_from_slice(&64u32.to_be_bytes());
        framed.extend_from_slice(&[0u8; 4]); // far short of 64
        let mut cursor = std::io::Cursor::new(framed);
        assert!(matches!(
            read_signal(&mut cursor).await,
            Err(FrameError::Io(_))
        ));
    }

    #[tokio::test]
    async fn several_signals_stream_in_order() {
        let mut buf = Vec::new();
        let mut ids = Vec::new();
        for i in 0..5u8 {
            let s = Signal::data("t")
                .with_weight(0.1 * f32::from(i + 1))
                .build_unsigned(NodeId(vec![i; 32]));
            ids.push(s.id);
            write_signal(&mut buf, &s).await.expect("write");
        }

        let mut cursor = std::io::Cursor::new(buf);
        for expected in ids {
            assert_eq!(read_signal(&mut cursor).await.expect("read").id, expected);
        }
        assert!(matches!(
            read_signal(&mut cursor).await,
            Err(FrameError::Closed)
        ));
    }
}
