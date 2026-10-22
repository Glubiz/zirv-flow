//! The framing between the pool and a render host: a 4-byte big-endian length, then that many
//! bytes of JSON. Both directions are bounded, and a bound is checked before anything is
//! allocated for the payload.

use std::io::{self, Read, Write};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The largest request the host reads and the pool sends: a document (sources of at most 256 KiB
/// per field) plus the rendered person's and event's values.
pub const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;

/// The largest response the pool reads and the host sends. The renderer's output caps add up to
/// under 2 MiB, which JSON escaping can at worst multiply by six.
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Why a frame could not be read or written.
#[derive(Debug)]
pub(crate) enum FrameError {
    /// The stream ended before the first byte of a frame: the peer closed it.
    Closed,
    /// The stream ended inside a frame.
    Truncated,
    /// The length prefix is over the bound.
    TooLarge(usize),
    /// The stream failed.
    Io(io::Error),
}

fn check(length: usize, max: usize) -> Result<(), FrameError> {
    if length > max {
        Err(FrameError::TooLarge(length))
    } else {
        Ok(())
    }
}

fn eof_or(error: io::Error, truncated: bool) -> FrameError {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        if truncated {
            FrameError::Truncated
        } else {
            FrameError::Closed
        }
    } else {
        FrameError::Io(error)
    }
}

/// Read one frame, refusing a length over `max`.
pub(crate) fn read_frame(input: &mut impl Read, max: usize) -> Result<Vec<u8>, FrameError> {
    let mut prefix = [0u8; 4];
    // A close before the first byte is a clean end; a close after some of it is a cut frame.
    let first = loop {
        match input.read(&mut prefix[..1]) {
            Ok(read) => break read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(FrameError::Io(error)),
        }
    };
    if first == 0 {
        return Err(FrameError::Closed);
    }
    input
        .read_exact(&mut prefix[1..])
        .map_err(|error| eof_or(error, true))?;
    let length = u32::from_be_bytes(prefix) as usize;
    check(length, max)?;
    let mut payload = vec![0u8; length];
    input
        .read_exact(&mut payload)
        .map_err(|error| eof_or(error, true))?;
    Ok(payload)
}

/// Write one frame and flush.
pub(crate) fn write_frame(output: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    let length = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too long"))?;
    output.write_all(&length.to_be_bytes())?;
    output.write_all(payload)?;
    output.flush()
}

/// [`read_frame`] for the pool's side.
pub(crate) async fn read_frame_async(
    input: &mut (impl AsyncRead + Unpin),
    max: usize,
) -> Result<Vec<u8>, FrameError> {
    let mut prefix = [0u8; 4];
    let first = input.read(&mut prefix[..1]).await.map_err(FrameError::Io)?;
    if first == 0 {
        return Err(FrameError::Closed);
    }
    input
        .read_exact(&mut prefix[1..])
        .await
        .map_err(|error| eof_or(error, true))?;
    let length = u32::from_be_bytes(prefix) as usize;
    check(length, max)?;
    let mut payload = vec![0u8; length];
    input
        .read_exact(&mut payload)
        .await
        .map_err(|error| eof_or(error, true))?;
    Ok(payload)
}

/// [`write_frame`] for the pool's side.
pub(crate) async fn write_frame_async(
    output: &mut (impl AsyncWrite + Unpin),
    payload: &[u8],
) -> io::Result<()> {
    let length = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too long"))?;
    output.write_all(&length.to_be_bytes()).await?;
    output.write_all(payload).await?;
    output.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn framed(payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        write_frame(&mut out, payload).unwrap();
        out
    }

    #[test]
    fn a_frame_is_a_big_endian_length_then_the_payload() {
        assert_eq!(framed(b"abc"), [0, 0, 0, 3, b'a', b'b', b'c']);
        let input = framed(b"hello")
            .into_iter()
            .chain(framed(b"!"))
            .collect::<Vec<_>>();
        let mut reader = input.as_slice();
        assert_eq!(read_frame(&mut reader, 16).unwrap(), b"hello");
        assert_eq!(read_frame(&mut reader, 16).unwrap(), b"!");
        assert!(matches!(
            read_frame(&mut reader, 16),
            Err(FrameError::Closed)
        ));
    }

    #[test]
    fn an_oversized_length_is_refused_before_anything_is_read() {
        let mut input: &[u8] = &[0xff, 0xff, 0xff, 0xff];
        assert!(matches!(
            read_frame(&mut input, 1024),
            Err(FrameError::TooLarge(length)) if length == u32::MAX as usize
        ));
        let just_over = framed(&[0u8; 9]);
        assert!(matches!(
            read_frame(&mut just_over.as_slice(), 8),
            Err(FrameError::TooLarge(9))
        ));
    }

    #[test]
    fn a_cut_frame_is_truncated_not_closed() {
        for cut in [1, 2, 3, 4, 6] {
            let whole = framed(b"abcdef");
            let mut input = &whole[..cut];
            assert!(
                matches!(read_frame(&mut input, 16), Err(FrameError::Truncated)),
                "cut at {cut}"
            );
        }
    }

    #[tokio::test]
    async fn the_async_side_agrees() {
        let whole = framed(b"abcdef");
        let mut input = whole.as_slice();
        assert_eq!(read_frame_async(&mut input, 16).await.unwrap(), b"abcdef");
        assert!(matches!(
            read_frame_async(&mut input, 16).await,
            Err(FrameError::Closed)
        ));
        let mut cut = &whole[..5];
        assert!(matches!(
            read_frame_async(&mut cut, 16).await,
            Err(FrameError::Truncated)
        ));
        let mut big: &[u8] = &[0, 0, 1, 0];
        assert!(matches!(
            read_frame_async(&mut big, 16).await,
            Err(FrameError::TooLarge(256))
        ));
        let mut out = Vec::new();
        write_frame_async(&mut out, b"xy").await.unwrap();
        assert_eq!(out, [0, 0, 0, 2, b'x', b'y']);
    }
}
