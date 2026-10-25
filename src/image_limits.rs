//! How big a picture from a phone may be before DeskLink decodes it. A file
//! of a few hundred KB can claim 60,000 x 60,000 pixels; decoding it in full
//! is gigabytes, and the app (with every terminal it hosts) is the one killed.
//! The size is read from the header first, and a picture over the bound is
//! refused rather than decoded.

use std::path::Path;

/// Pixels a picture may have to be decoded at all (a 64-megapixel photo).
pub(crate) const MAX_DECODE_PIXELS: u64 = 64_000_000;
/// What a clipboard picture may have (a 4K screen and more).
pub(crate) const MAX_CLIPBOARD_PIXELS: u64 = 32_000_000;
/// The longest side a decoded picture may have.
pub(crate) const MAX_DECODE_SIDE: u32 = 16_384;

/// Whether a `width` x `height` picture is within the bounds.
pub(crate) fn within_bounds(width: i64, height: i64, max_pixels: u64) -> bool {
    let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height)) else {
        return false;
    };
    width > 0
        && height > 0
        && width <= MAX_DECODE_SIDE
        && height <= MAX_DECODE_SIDE
        && u64::from(width) * u64::from(height) <= max_pixels
}

/// Reads the size from `path`'s header and checks it. A file whose header
/// gdk-pixbuf cannot read is not an image to decode.
pub(crate) fn file_within_bounds(path: &Path, max_pixels: u64) -> bool {
    gtk::gdk_pixbuf::Pixbuf::file_info(path).is_some_and(|(_, width, height)| {
        within_bounds(i64::from(width), i64::from(height), max_pixels)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_picture_past_the_bounds_is_refused() {
        assert!(within_bounds(4032, 3024, MAX_DECODE_PIXELS));
        assert!(within_bounds(8000, 8000, MAX_DECODE_PIXELS));
        // A bomb: tiny on disk, enormous decoded.
        assert!(!within_bounds(60_000, 60_000, MAX_DECODE_PIXELS));
        assert!(
            !within_bounds(100_000, 10, MAX_DECODE_PIXELS),
            "one side too long"
        );
        assert!(!within_bounds(9000, 9000, MAX_DECODE_PIXELS), "81 Mpx");
        assert!(!within_bounds(0, 100, MAX_DECODE_PIXELS));
        assert!(!within_bounds(-5, 100, MAX_DECODE_PIXELS));
        assert!(within_bounds(2000, 2000, 4_000_000));
        assert!(!within_bounds(2001, 2000, 4_000_000));
    }

    /// A real PNG header that claims 60,000 x 60,000 is read from the header
    /// and refused without being decoded.
    #[test]
    fn a_png_bomb_header_is_refused_before_decoding() {
        let dir = std::env::temp_dir().join(format!("desklink-bomb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bomb.png");
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        let mut header = Vec::new();
        header.extend_from_slice(&60_000u32.to_be_bytes());
        header.extend_from_slice(&60_000u32.to_be_bytes());
        header.extend_from_slice(&[8, 6, 0, 0, 0]);
        png.extend_from_slice(&header);
        let mut crc_input = b"IHDR".to_vec();
        crc_input.extend_from_slice(&header);
        let crc = crc32(&crc_input);
        png.extend_from_slice(&crc.to_be_bytes());
        std::fs::write(&path, &png).unwrap();
        assert!(!file_within_bounds(&path, MAX_DECODE_PIXELS));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
}
