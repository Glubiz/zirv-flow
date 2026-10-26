//! Telling an image's type and size from the first bytes of its file. Only the header is read:
//! pixels are never decoded. Every read is bounds-checked, so any input gives an answer or an
//! error, never a panic.

/// The image types an upload may be.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ImageKind {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageKind {
    /// The kind a declared `Content-Type` media type names.
    pub(crate) fn from_media_type(media: &str) -> Option<Self> {
        match media.to_ascii_lowercase().as_str() {
            "image/png" => Some(Self::Png),
            "image/jpeg" => Some(Self::Jpeg),
            "image/gif" => Some(Self::Gif),
            "image/webp" => Some(Self::Webp),
            _ => None,
        }
    }

    pub(crate) fn content_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }

    /// The extension of the public URL.
    pub(crate) fn ext(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::Webp => "webp",
        }
    }
}

/// What the header says.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Sniffed {
    pub(crate) kind: ImageKind,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// Why a body is not a readable image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SniffError {
    /// No allowed image type starts like this.
    NotAnImage,
    /// The magic bytes match a type but the header is cut off, malformed or empty-sized.
    Unreadable,
}

fn slice(bytes: &[u8], at: usize, len: usize) -> Option<&[u8]> {
    bytes.get(at..at.checked_add(len)?)
}

fn be16(bytes: &[u8], at: usize) -> Option<u32> {
    let pair = slice(bytes, at, 2)?;
    Some((u32::from(*pair.first()?) << 8) | u32::from(*pair.get(1)?))
}

fn le16(bytes: &[u8], at: usize) -> Option<u32> {
    let pair = slice(bytes, at, 2)?;
    Some((u32::from(*pair.get(1)?) << 8) | u32::from(*pair.first()?))
}

fn le24(bytes: &[u8], at: usize) -> Option<u32> {
    let triple = slice(bytes, at, 3)?;
    Some(
        u32::from(*triple.first()?)
            | (u32::from(*triple.get(1)?) << 8)
            | (u32::from(*triple.get(2)?) << 16),
    )
}

fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    Some((be16(bytes, at)? << 16) | be16(bytes, at + 2)?)
}

fn le32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(le16(bytes, at)? | (le16(bytes, at + 2)? << 16))
}

/// Reads the type and the pixel size from the start of an image file.
pub(crate) fn sniff(bytes: &[u8]) -> Result<Sniffed, SniffError> {
    let (kind, size) = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        (ImageKind::Png, png_size(bytes))
    } else if bytes.starts_with(&[0xFF, 0xD8]) {
        (ImageKind::Jpeg, jpeg_size(bytes))
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        (ImageKind::Gif, gif_size(bytes))
    } else if bytes.starts_with(b"RIFF") && slice(bytes, 8, 4).is_some_and(|tag| tag == b"WEBP") {
        (ImageKind::Webp, webp_size(bytes))
    } else {
        return Err(SniffError::NotAnImage);
    };
    match size {
        Some((width, height)) if width > 0 && height > 0 => Ok(Sniffed {
            kind,
            width,
            height,
        }),
        _ => Err(SniffError::Unreadable),
    }
}

/// The signature, then the IHDR chunk: length 13, `IHDR`, width, height.
fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if be32(bytes, 8)? != 13 || slice(bytes, 12, 4)? != b"IHDR" {
        return None;
    }
    Some((be32(bytes, 16)?, be32(bytes, 20)?))
}

/// The logical screen size after the six-byte signature.
fn gif_size(bytes: &[u8]) -> Option<(u32, u32)> {
    Some((le16(bytes, 6)?, le16(bytes, 8)?))
}

/// Walks the segments after SOI, skipping each by its length, to the first start-of-frame marker
/// (SOF0 to SOF15, not DHT, JPG or DAC), whose payload holds the precision, height and width.
fn jpeg_size(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut at = 2_usize;
    loop {
        if *bytes.get(at)? != 0xFF {
            return None;
        }
        // Any run of 0xFF fill bytes may precede the marker code.
        while *bytes.get(at)? == 0xFF {
            at += 1;
        }
        let marker = *bytes.get(at)?;
        at += 1;
        match marker {
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                // The declared length covers itself, the precision, height, width and the
                // component count.
                if be16(bytes, at)? < 8 {
                    return None;
                }
                let height = be16(bytes, at.checked_add(3)?)?;
                let width = be16(bytes, at.checked_add(5)?)?;
                return Some((width, height));
            }
            // Markers with no length: TEM, RSTn, SOI. Then the image data or the end: no frame
            // header can follow.
            0x00 | 0x01 | 0xD0..=0xD8 => {}
            0xD9 | 0xDA => return None,
            _ => {
                let length = usize::try_from(be16(bytes, at)?).ok()?;
                if length < 2 {
                    return None;
                }
                at = at.checked_add(length)?;
            }
        }
    }
}

/// `RIFF`, the size, `WEBP`, then the first chunk: lossy `VP8 `, lossless `VP8L` or extended
/// `VP8X`. The chunk's declared size must cover the bytes read from it and lie inside the buffer.
fn webp_size(bytes: &[u8]) -> Option<(u32, u32)> {
    const DATA: usize = 20;
    let declared = usize::try_from(le32(bytes, 16)?).ok()?;
    let chunk_fits = |needed: usize| {
        declared >= needed
            && DATA
                .checked_add(declared)
                .is_some_and(|end| end <= bytes.len())
    };
    match slice(bytes, 12, 4)? {
        b"VP8 " => {
            if !chunk_fits(10) || slice(bytes, 23, 3)? != [0x9D, 0x01, 0x2A] {
                return None;
            }
            Some((le16(bytes, 26)? & 0x3FFF, le16(bytes, 28)? & 0x3FFF))
        }
        b"VP8L" => {
            if !chunk_fits(5) || *bytes.get(20)? != 0x2F {
                return None;
            }
            let bits = le32(bytes, 21)?;
            Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
        }
        b"VP8X" => {
            if !chunk_fits(10) {
                return None;
            }
            Some((le24(bytes, 24)? + 1, le24(bytes, 27)? + 1))
        }
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    //! Header-only images, enough for the sniffer.

    pub(crate) fn png(width: u32, height: u32) -> Vec<u8> {
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        out.extend_from_slice(&13_u32.to_be_bytes());
        out.extend_from_slice(b"IHDR");
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&height.to_be_bytes());
        out.extend_from_slice(&[8, 6, 0, 0, 0]);
        out
    }

    pub(crate) fn jpeg(width: u16, height: u16) -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8];
        // An APP0 segment to skip.
        out.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x06, b'J', b'F', b'I', b'F']);
        out.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 8]);
        out.extend_from_slice(&height.to_be_bytes());
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&[1, 1, 0x11, 0]);
        out
    }

    pub(crate) fn gif(width: u16, height: u16) -> Vec<u8> {
        let mut out = b"GIF89a".to_vec();
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0]);
        out
    }

    pub(crate) fn webp_lossy(width: u16, height: u16) -> Vec<u8> {
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&30_u32.to_le_bytes());
        out.extend_from_slice(b"WEBPVP8 ");
        out.extend_from_slice(&10_u32.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0x9D, 0x01, 0x2A]);
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out
    }

    pub(crate) fn webp_lossless(width: u32, height: u32) -> Vec<u8> {
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&13_u32.to_le_bytes());
        out.extend_from_slice(b"WEBPVP8L");
        out.extend_from_slice(&5_u32.to_le_bytes());
        out.push(0x2F);
        let bits = (width - 1) | ((height - 1) << 14);
        out.extend_from_slice(&bits.to_le_bytes());
        out
    }

    pub(crate) fn webp_extended(width: u32, height: u32) -> Vec<u8> {
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&18_u32.to_le_bytes());
        out.extend_from_slice(b"WEBPVP8X");
        out.extend_from_slice(&10_u32.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
        out.extend_from_slice(&(height - 1).to_le_bytes()[..3]);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    fn sized(kind: ImageKind, width: u32, height: u32) -> Result<Sniffed, SniffError> {
        Ok(Sniffed {
            kind,
            width,
            height,
        })
    }

    #[test]
    fn each_type_reports_its_size_from_the_header() {
        assert_eq!(sniff(&png(640, 480)), sized(ImageKind::Png, 640, 480));
        assert_eq!(sniff(&jpeg(300, 200)), sized(ImageKind::Jpeg, 300, 200));
        assert_eq!(sniff(&gif(10, 20)), sized(ImageKind::Gif, 10, 20));
        assert_eq!(sniff(&webp_lossy(33, 44)), sized(ImageKind::Webp, 33, 44));
        assert_eq!(
            sniff(&webp_lossless(500, 100)),
            sized(ImageKind::Webp, 500, 100)
        );
        assert_eq!(
            sniff(&webp_extended(1000, 2000)),
            sized(ImageKind::Webp, 1000, 2000)
        );
    }

    #[test]
    fn a_jpeg_frame_header_is_found_past_other_segments_and_fill_bytes() {
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xFF, 0xE1, 0x00, 0x04, 1, 2];
        bytes.extend_from_slice(&[0xFF, 0xC4, 0x00, 0x02]);
        bytes.extend_from_slice(&[0xFF, 0xC2, 0x00, 0x0B, 8, 0, 7, 0, 9, 1, 1, 0x11, 0]);

        assert_eq!(sniff(&bytes), sized(ImageKind::Jpeg, 9, 7));
    }

    #[test]
    fn a_jpeg_with_no_frame_header_before_the_scan_is_unreadable() {
        let bytes = [0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02, 0, 0, 0, 0];

        assert_eq!(sniff(&bytes), Err(SniffError::Unreadable));
    }

    #[test]
    fn text_svg_and_empty_bodies_are_not_images() {
        for body in [
            &b""[..],
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
            b"<?xml version=\"1.0\"?><svg/>",
            b"RIFF\0\0\0\0WAVE",
            b"GIF",
        ] {
            assert_eq!(sniff(body), Err(SniffError::NotAnImage), "{body:?}");
        }
    }

    #[test]
    fn a_jpeg_frame_header_that_declares_too_short_a_length_is_unreadable() {
        // Length 2 claims an empty segment; the bytes after it are not a frame header.
        let bytes = [
            0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x02, 8, 0, 9, 0, 7, 1, 1, 0x11, 0,
        ];
        let seven = [
            0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x07, 8, 0, 9, 0, 7, 1, 1, 0x11, 0,
        ];

        assert_eq!(sniff(&bytes), Err(SniffError::Unreadable));
        assert_eq!(sniff(&seven), Err(SniffError::Unreadable));
    }

    #[test]
    fn a_webp_chunk_that_declares_too_small_a_size_or_runs_past_the_buffer_is_unreadable() {
        let with_size = |mut bytes: Vec<u8>, size: u32| {
            bytes[16..20].copy_from_slice(&size.to_le_bytes());
            bytes
        };
        for full in [webp_lossy(8, 8), webp_lossless(8, 8), webp_extended(8, 8)] {
            assert!(sniff(&full).is_ok());
            assert_eq!(
                sniff(&with_size(full.clone(), 0)),
                Err(SniffError::Unreadable)
            );
            assert_eq!(
                sniff(&with_size(full.clone(), 4)),
                Err(SniffError::Unreadable)
            );
            assert_eq!(
                sniff(&with_size(full.clone(), 1000)),
                Err(SniffError::Unreadable)
            );
            assert_eq!(
                sniff(&with_size(full, u32::MAX)),
                Err(SniffError::Unreadable)
            );
        }
    }

    #[test]
    fn a_zero_size_is_unreadable() {
        assert_eq!(sniff(&png(0, 5)), Err(SniffError::Unreadable));
        assert_eq!(sniff(&gif(5, 0)), Err(SniffError::Unreadable));
    }

    #[test]
    fn every_truncation_of_a_valid_header_is_an_error_not_a_panic() {
        // Each header and the length at which it first holds the size.
        for (full, needed) in [
            (png(64, 64), 24),
            (jpeg(64, 64), 19),
            (gif(64, 64), 10),
            (webp_lossy(64, 64), 30),
            (webp_lossless(64, 64), 25),
            (webp_extended(64, 64), 30),
        ] {
            for cut in 0..needed {
                assert!(sniff(&full[..cut]).is_err(), "cut at {cut} of {full:?}");
            }
            assert!(sniff(&full[..needed]).is_ok(), "{full:?}");
        }
    }

    #[test]
    fn random_input_and_random_bodies_behind_a_real_magic_never_panic() {
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let prefixes: [&[u8]; 5] = [
            b"",
            b"\x89PNG\r\n\x1a\n",
            &[0xFF, 0xD8],
            b"GIF89a",
            b"RIFF\x10\0\0\0WEBPVP8X",
        ];
        for round in 0..4000_u64 {
            let prefix = prefixes[(round % 5) as usize];
            let length = (next() % 64) as usize;
            let mut body = prefix.to_vec();
            body.extend((0..length).map(|_| next().to_le_bytes()[0]));
            // The answer does not matter; reaching it does.
            let _ = sniff(&body);
            // A JPEG whose segments claim lengths past the buffer.
            let mut jpeg_body = vec![0xFF, 0xD8];
            for _ in 0..(next() % 8) {
                jpeg_body.extend_from_slice(&[0xFF, 0xE0 + (next() % 8) as u8]);
                jpeg_body.extend_from_slice(&((next() % 400) as u16).to_be_bytes());
                jpeg_body.extend((0..next() % 6).map(|_| next().to_le_bytes()[0]));
            }
            let _ = sniff(&jpeg_body);
        }
    }

    #[test]
    fn the_declared_media_type_names_a_kind_and_svg_names_none() {
        assert_eq!(
            ImageKind::from_media_type("IMAGE/PNG"),
            Some(ImageKind::Png)
        );
        assert_eq!(
            ImageKind::from_media_type("image/jpeg"),
            Some(ImageKind::Jpeg)
        );
        assert_eq!(ImageKind::from_media_type("image/svg+xml"), None);
        assert_eq!(ImageKind::from_media_type("text/html"), None);
        assert_eq!(ImageKind::Jpeg.ext(), "jpg");
        assert_eq!(ImageKind::Webp.content_type(), "image/webp");
    }
}
