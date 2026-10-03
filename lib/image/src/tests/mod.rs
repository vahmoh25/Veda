//! Cross-codec tests: robustness fuzzing and validation against real-world files.

mod fuzz;
mod real_images;

use crate::{Image, ImageError, decode, decode_lenient, png};

#[test]
fn lenient_decoding_skips_only_checksums() {
    let img = Image::from_fn(16, 12, |x, y| 0xFF00_0000 | x << 18 | y << 10 | (x ^ y) << 2);
    let file = png::encode(&img, 6).unwrap();
    assert_eq!(decode_lenient(&file).unwrap(), (img.clone(), false));
    // A damaged IHDR CRC fails normal decoding but not lenient decoding, which reports it.
    let mut bad_crc = file.clone();
    bad_crc[29] ^= 0x5A;
    assert_eq!(decode(&bad_crc), Err(ImageError::ChecksumMismatch("PNG chunk CRC")));
    assert_eq!(decode_lenient(&bad_crc).unwrap(), (img, true));
    // Other errors are passed through.
    assert_eq!(decode_lenient(&file[..file.len() / 2]).unwrap_err(), ImageError::Truncated);
    assert_eq!(decode_lenient(b"not a picture").unwrap_err(), ImageError::UnknownFormat);
}
