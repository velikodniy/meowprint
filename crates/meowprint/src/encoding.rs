//! Compression of packed pixel bytes.

use crate::{Error, Result, error::invalid};
use std::io::Write;

/// Encode runs from monochrome bytes, with the leftmost pixel in bit zero.
///
/// RLE stores repeated pixels as color and count. Each output byte represents
/// 1 through 127 pixels. Bit seven is the color, with one for black.
/// The remaining bits contain the count. Empty input produces empty output.
/// This helper does not add a command header or fall back to raw bytes.
///
#[must_use]
pub fn rle(row: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut previous = 0;
    let mut count = 0;
    for &byte in row {
        for bit in 0..8 {
            let color = (byte >> bit) & 1;
            if count != 0 && (color != previous || count == 127) {
                output.push((previous << 7) | count);
                count = 0;
            }
            previous = color;
            count += 1;
        }
    }
    if count != 0 {
        output.push((previous << 7) | count);
    }
    output
}

/// Compress pixels with LZO and prepend lengths for a compressed raster command.
///
/// The payload starts with the raw byte count and then the compressed byte count.
/// Both are unsigned 16-bit integers in little-endian order. Compressed bytes
/// follow them. This helper does not add a protocol frame or enforce a driver limit.
///
/// # Errors
/// Reject empty blocks, lengths above 65535, or a compressor error.
pub fn lzo_payload(raw: &[u8]) -> Result<Vec<u8>> {
    validate_block(raw)?;
    let compressed = lzokay_native::compress(raw).map_err(|e| Error::Compression(e.to_string()))?;
    prefixed(raw.len(), &compressed)
}

/// Compress pixels with zlib and prepend the same lengths as [`lzo_payload`].
///
/// This representation is experimental and lacks hardware testing in the reference.
/// Select it through [`crate::V5gOptions::encoding`].
///
/// # Errors
/// Reject empty blocks, lengths above 65535, or a compressor error.
pub fn zlib_payload(raw: &[u8]) -> Result<Vec<u8>> {
    validate_block(raw)?;
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(raw)
        .map_err(|e| Error::Compression(e.to_string()))?;
    let compressed = encoder
        .finish()
        .map_err(|e| Error::Compression(e.to_string()))?;
    prefixed(raw.len(), &compressed)
}
fn validate_block(raw: &[u8]) -> Result<()> {
    if raw.is_empty() || raw.len() > usize::from(u16::MAX) {
        return Err(invalid(
            "A compression block must contain 1 through 65535 bytes.",
        ));
    }
    Ok(())
}
fn prefixed(raw_len: usize, compressed: &[u8]) -> Result<Vec<u8>> {
    let raw_len = u16::try_from(raw_len)
        .map_err(|_| invalid("The uncompressed block exceeds 65535 bytes."))?;
    let compressed_len = u16::try_from(compressed.len())
        .map_err(|_| invalid("The compressed block exceeds 65535 bytes."))?;
    let mut payload = Vec::with_capacity(compressed.len() + 4);
    payload.extend_from_slice(&raw_len.to_le_bytes());
    payload.extend_from_slice(&compressed_len.to_le_bytes());
    payload.extend_from_slice(compressed);
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use crate::encoding;
    #[test]
    fn rle_runs_split_and_expand_to_the_original_bits() {
        assert_eq!(encoding::rle(&[0; 48]), [127, 127, 127, 3]);
        assert_eq!(encoding::rle(&[255; 48]), [255, 255, 255, 131]);
        let input = [0x81, 0x02, 0x55, 0xff, 0];
        let runs = encoding::rle(&input);
        let actual: Vec<_> = runs
            .iter()
            .flat_map(|&run| std::iter::repeat_n(run >> 7, usize::from(run & 127)))
            .collect();
        let expected: Vec<_> = input
            .iter()
            .flat_map(|byte| (0..8).map(move |bit| (byte >> bit) & 1))
            .collect();
        assert_eq!(actual, expected);
    }
}
