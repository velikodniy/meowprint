//! Compression of packed pixel bytes.

use crate::{Error, Result, error::invalid};
use flate2::{Compress, FlushCompress, Status};

/// Encode LSB-first monochrome pixels into color-and-count runs.
///
/// Each output byte stores the color in bit seven (one is black) and a run
/// length from 1 through 127 in the remaining bits. Return the bytes written,
/// or `None` if the buffer is too small; partial output may remain.
/// Empty input returns `Some(0)`. No framing or raw fallback is added.
pub fn rle_into(raw: &[u8], output: &mut [u8]) -> Option<usize> {
    let mut previous = 0;
    let mut count = 0;
    let mut written = 0;
    for &byte in raw {
        for bit in 0..8 {
            let color = (byte >> bit) & 1;
            if count != 0 && (color != previous || count == 127) {
                let slot = output.get_mut(written)?;
                *slot = (previous << 7) | count;
                written += 1;
                count = 0;
            }
            previous = color;
            count += 1;
        }
    }
    if count != 0 {
        let slot = output.get_mut(written)?;
        *slot = (previous << 7) | count;
        written += 1;
    }
    Some(written)
}

/// Compress one block into `output` using a reusable LZO dictionary.
///
/// Return the compressed byte count without length prefixes or framing.
/// Output may be partially written on error.
///
/// # Errors
/// Reject input outside 1 through 65535 bytes, insufficient output space, or
/// a compressor failure.
pub fn lzo_into(
    raw: &[u8],
    output: &mut [u8],
    dictionary: &mut lzokay::compress::Dict,
) -> Result<usize> {
    validate_block(raw)?;
    lzokay::compress::compress_no_alloc(raw, output, dictionary).map_err(|error| match error {
        lzokay::Error::OutputOverrun => {
            invalid("The frame exceeds the selected driver payload limit.")
        }
        error => Error::Compression(error.to_string()),
    })
}

/// Compress one independent block using a reusable zlib compressor.
///
/// Reset the compressor before each block and return the bytes written without
/// length prefixes or framing. The compressor must be configured for zlib
/// headers. Output may be partially written on error.
///
/// # Errors
/// Reject input outside 1 through 65535 bytes, insufficient output space, or
/// a compressor failure.
#[expect(
    clippy::cast_possible_truncation,
    reason = "Codec progress cannot exceed the input and output slices, whose sizes fit usize."
)]
pub fn zlib_into(raw: &[u8], output: &mut [u8], compressor: &mut Compress) -> Result<usize> {
    validate_block(raw)?;
    compressor.reset();
    let mut input_offset = 0;
    let mut output_offset = 0;
    loop {
        let before_in = compressor.total_in();
        let before_out = compressor.total_out();
        let (input, flush) = if input_offset < raw.len() {
            (&raw[input_offset..], FlushCompress::None)
        } else {
            (&[][..], FlushCompress::Finish)
        };
        let status = compressor
            .compress(input, &mut output[output_offset..], flush)
            .map_err(|error| Error::Compression(error.to_string()))?;
        input_offset += (compressor.total_in() - before_in) as usize;
        output_offset += (compressor.total_out() - before_out) as usize;
        if status == Status::StreamEnd {
            return Ok(output_offset);
        }
        if output_offset == output.len() {
            return Err(invalid(
                "The frame exceeds the selected driver payload limit.",
            ));
        }
        if before_in == compressor.total_in() && before_out == compressor.total_out() {
            return Err(Error::Compression(
                "The zlib compressor made no progress.".into(),
            ));
        }
    }
}

fn validate_block(raw: &[u8]) -> Result<()> {
    if raw.is_empty() || raw.len() > usize::from(u16::MAX) {
        return Err(invalid(
            "A compression block must contain 1 through 65535 bytes.",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    #[test]
    fn rle_runs_split_and_expand_to_the_original_bits() -> Result<()> {
        let mut encoded = [0; 48];
        let len = rle_into(&[0; 48], &mut encoded).ok_or_else(|| invalid("RLE overflow"))?;
        assert_eq!(&encoded[..len], [127, 127, 127, 3]);
        let len = rle_into(&[255; 48], &mut encoded).ok_or_else(|| invalid("RLE overflow"))?;
        assert_eq!(&encoded[..len], [255, 255, 255, 131]);
        let input = [0x81, 0x02, 0x55, 0xff, 0];
        let len = rle_into(&input, &mut encoded).ok_or_else(|| invalid("RLE overflow"))?;
        let actual: Vec<_> = encoded[..len]
            .iter()
            .flat_map(|&run| std::iter::repeat_n(run >> 7, usize::from(run & 127)))
            .collect();
        let expected: Vec<_> = input
            .iter()
            .flat_map(|byte| (0..8).map(move |bit| (byte >> bit) & 1))
            .collect();
        assert_eq!(actual, expected);
        Ok(())
    }

    #[test]
    fn rle_reports_equal_size_and_output_exhaustion() {
        let equal: Vec<u8> = (0..48).map(|i| if i % 2 == 0 { 0xff } else { 0 }).collect();
        let mut output = [0; 48];
        assert_eq!(rle_into(&equal, &mut output), Some(48));
        let alternating: Vec<u8> = (0..48)
            .map(|i| if i % 2 == 0 { 0x55 } else { 0xaa })
            .collect();
        assert_eq!(rle_into(&alternating, &mut output), None);
        assert_eq!(rle_into(&[0; 48], &mut [0; 3]), None);
    }

    #[test]
    fn zlib_exhaustion_and_exact_capacity_preserve_independent_streams() -> Result<()> {
        let mut compressor = Compress::new(Compression::default(), true);
        let mut expected = [0; 255];
        let length = zlib_into(&[0xa5; 48], &mut expected, &mut compressor)?;
        for capacity in [0, 1, length - 1] {
            assert!(matches!(
                zlib_into(&[0xa5; 48], &mut vec![0; capacity], &mut compressor),
                Err(Error::InvalidInput(_))
            ));
        }
        let mut exact = vec![0; length];
        assert_eq!(zlib_into(&[0xa5; 48], &mut exact, &mut compressor)?, length);
        assert_eq!(exact, expected[..length]);
        for raw in [vec![], vec![0; 65_536]] {
            assert!(matches!(
                zlib_into(&raw, &mut exact, &mut compressor),
                Err(Error::InvalidInput(_))
            ));
        }
        Ok(())
    }
    #[test]
    fn lzo_output_exhaustion_is_a_frame_limit_error() {
        let mut dictionary = lzokay::compress::Dict::new();
        let result = lzo_into(&[0xa5; 48], &mut [], &mut dictionary);
        assert!(matches!(result, Err(crate::Error::InvalidInput(_))));
    }
}
