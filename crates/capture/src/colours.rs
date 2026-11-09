//! Which colour matrix a clip's pictures are stored with, by one rule for
//! the decoders, `media`'s probe and the exports: a clip then looks the same
//! wherever the app shows it, and an export (labelled BT.709) looks as the
//! clip did in other players.

/// Whether a clip's colours are BT.601's, else BT.709's. As its file says
/// (H.273's numbers, from its MP4's `colr` box or its H.264 stream: 5 and 6
/// are BT.601's, 1 BT.709's); where it doesn't say (or says "unspecified",
/// 2), as players take it (mpv's and libplacebo's rule): standard
/// definition, under 1280 wide and at most 576 high, BT.601; anything
/// bigger, BT.709.
pub fn bt601(matrix: Option<u16>, width: u32, height: u32) -> bool {
    match matrix {
        Some(5 | 6) => true,
        Some(2) | None => width < 1280 && height <= 576,
        Some(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a clip says goes, whatever its size.
    #[test]
    fn as_the_file_says() {
        assert!(bt601(Some(6), 1920, 1080));
        assert!(bt601(Some(5), 2560, 1440));
        assert!(!bt601(Some(1), 640, 480));
        assert!(!bt601(Some(9), 640, 480), "BT.2020 isn't BT.601");
    }

    /// Not said: standard definition is BT.601 (NTSC's and PAL's sizes, and
    /// below), high definition BT.709, by its width as much as its height.
    #[test]
    fn not_said_by_its_size() {
        for (w, h) in [(640, 480), (720, 480), (720, 576), (854, 480), (1024, 576), (640, 360)] {
            assert!(bt601(None, w, h), "{w}x{h}");
            assert!(bt601(Some(2), w, h), "{w}x{h} unspecified");
        }
        for (w, h) in [(1280, 720), (1920, 1080), (960, 720), (1280, 640), (1024, 600), (2560, 1440)] {
            assert!(!bt601(None, w, h), "{w}x{h}");
        }
    }
}
