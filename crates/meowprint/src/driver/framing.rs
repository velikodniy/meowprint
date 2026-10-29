//! Shared byte framing, CRC, and incremental reply decoding.
//!
//! Drivers supply frame headers, payload limits, and checksum requirements.
//!
use crate::{Result, error::invalid};

/// Compute payload-only CRC-8 with polynomial `0x07` and initial value zero.
///
/// Headers, length fields, and terminators are excluded.
#[must_use]
pub fn crc8(payload: &[u8]) -> u8 {
    payload.iter().fold(0, |mut crc, &byte| {
        crc ^= byte;
        for _ in 0..8 {
            crc = if crc & 0x80 == 0 {
                crc << 1
            } else {
                (crc << 1) ^ 0x07
            };
        }
        crc
    })
}

/// A decoded command, direction byte, and payload, without the frame header.
///
/// Unknown commands and payloads are preserved. CRC validation depends on the
/// decoder's policy; a frame alone does not establish its origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The family-specific command byte.
    pub command: u8,
    /// Direction or firmware-specific header byte, preserved verbatim.
    pub direction: u8,
    /// Exact bytes without an assumed reply schema.
    pub payload: Vec<u8>,
}
/// An outgoing command whose identity remains available after encoding.
#[derive(Debug)]
pub struct EncodedCommand {
    /// Command byte used to match replies.
    pub command: u8,
    /// Complete frame ready for transmission.
    pub bytes: Vec<u8>,
}

/// Encode a complete frame with a payload-only CRC and `0xff` terminator.
///
/// # Errors
/// Reject payloads above `max_payload` or 65535 bytes.
pub fn encode(
    header: [u8; 2],
    command: u8,
    direction: u8,
    payload: &[u8],
    max_payload: usize,
) -> Result<Vec<u8>> {
    validate_payload(payload.len(), max_payload)?;
    let mut bytes = vec![0; payload.len() + 8];
    bytes[6..6 + payload.len()].copy_from_slice(payload);
    finish_into(
        header,
        command,
        direction,
        payload.len(),
        max_payload,
        &mut bytes,
    )?;
    Ok(bytes)
}

/// Finish a frame whose payload is already in `output[6..6 + payload_len]`.
///
/// Write the header, length, payload-only CRC, and terminator without moving the
/// payload. Return the complete frame length, `payload_len + 8`.
///
/// # Errors
/// Reject payloads above `max_payload` or 65535 bytes, or an undersized buffer.
pub fn finish_into(
    header: [u8; 2],
    command: u8,
    direction: u8,
    payload_len: usize,
    max_payload: usize,
    output: &mut [u8],
) -> Result<usize> {
    validate_payload(payload_len, max_payload)?;
    if output.len() < payload_len + 8 {
        return Err(invalid("The frame output buffer is too small."));
    }
    let len = u16::try_from(payload_len)
        .map_err(|_| invalid("A frame cannot exceed 65535 payload bytes."))?;
    output[..2].copy_from_slice(&header);
    output[2] = command;
    output[3] = direction;
    output[4..6].copy_from_slice(&len.to_le_bytes());
    output[6 + payload_len] = crc8(&output[6..6 + payload_len]);
    output[7 + payload_len] = 0xff;
    Ok(payload_len + 8)
}

fn validate_payload(payload_len: usize, max_payload: usize) -> Result<()> {
    u16::try_from(payload_len)
        .map_err(|_| invalid("A frame cannot exceed 65535 payload bytes."))?;
    if payload_len > max_payload {
        return Err(invalid(
            "The frame exceeds the selected driver payload limit.",
        ));
    }
    Ok(())
}

/// An incremental reply parser with a driver-selected header and CRC policy.
///
/// Feed notifications in order using one decoder per connection. Incomplete
/// frames remain buffered; invalid headers, lengths, CRCs, and terminators are
/// skipped. Buffered length is bounded by the maximum frame size.
/// Request matching and timeouts belong to the session.
#[derive(Debug)]
pub struct Decoder {
    buffer: Vec<u8>,
    header: [u8; 2],
    max_payload: usize,
    received: u64,
    check_crc: bool,
}
impl Decoder {
    /// Create a parser for the driver’s header, payload ceiling, and checksum policy.
    ///
    /// Payload limits above 65535 are clamped.
    #[must_use]
    pub fn new(header: [u8; 2], max_payload: usize, check_crc: bool) -> Self {
        Self {
            buffer: Vec::new(),
            header,
            max_payload: max_payload.min(usize::from(u16::MAX)),
            received: 0,
            check_crc,
        }
    }
    /// Return the number of input bytes received, including discarded bytes.
    pub(crate) const fn position(&self) -> u64 {
        self.received
    }
    /// Decode frames with one-based positions of their first header byte.
    ///
    /// Positions remain stable when a frame spans multiple notifications.
    pub fn feed_positioned(&mut self, bytes: &[u8]) -> Vec<(u64, Frame)> {
        let mut frames = Vec::new();
        let header = self.header;
        for &byte in bytes {
            self.received += 1;
            self.buffer.push(byte);
            loop {
                if self.buffer.first().is_some_and(|&v| v != header[0]) {
                    self.buffer.remove(0);
                    continue;
                }
                if self.buffer.len() < 2 {
                    break;
                }
                if self.buffer[1] != header[1] {
                    self.buffer.remove(0);
                    continue;
                }
                if self.buffer.len() < 6 {
                    break;
                }
                let len = usize::from(u16::from_le_bytes([self.buffer[4], self.buffer[5]]));
                if len > self.max_payload {
                    self.buffer.remove(0);
                    continue;
                }
                let frame_len = len + if self.check_crc { 8 } else { 7 };
                if self.buffer.len() < frame_len {
                    break;
                }
                if self.buffer[frame_len - 1] != 0xff
                    || (self.check_crc && crc8(&self.buffer[6..6 + len]) != self.buffer[len + 6])
                {
                    self.buffer.remove(0);
                    continue;
                }
                frames.push((
                    self.received - self.buffer.len() as u64 + 1,
                    Frame {
                        command: self.buffer[2],
                        direction: self.buffer[3],
                        payload: self.buffer[6..6 + len].to_vec(),
                    },
                ));
                self.buffer.drain(..frame_len);
            }
        }
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::{Decoder, Frame, encode};

    #[test]
    fn framing_preserves_fragmented_payloads_and_rejects_bad_crc() -> crate::Result<()> {
        for (header, command, direction, bytes) in [
            (
                [0x51, 0x78],
                0xae,
                1,
                [0x51, 0x78, 0xae, 1, 1, 0, 0x10, 0x70, 0xff],
            ),
            (
                [0x22, 0x21],
                0xa1,
                3,
                [0x22, 0x21, 0xa1, 3, 1, 0, 0x10, 0x70, 0xff],
            ),
        ] {
            let frame = Frame {
                command,
                direction,
                payload: vec![0x10],
            };
            assert_eq!(
                encode(header, command, direction, &frame.payload, 255)?,
                bytes
            );
            let mut decoder = Decoder::new(header, 255, true);
            assert_eq!(decoder.feed_positioned(&bytes[..1]), []);
            assert_eq!(decoder.feed_positioned(&bytes[1..]), [(1, frame)]);
            let mut corrupt = bytes;
            corrupt[7] ^= 1;
            assert_eq!(decoder.feed_positioned(&corrupt), []);
            assert_eq!(decoder.feed_positioned(&bytes).len(), 1);
        }
        Ok(())
    }

    #[test]
    fn every_split_and_concatenation_preserve_frame_positions() -> crate::Result<()> {
        for (header, crc) in [
            ([0x51, 0x78], true),
            ([0x22, 0x21], true),
            ([0x22, 0x21], false),
        ] {
            let mut bytes = encode(header, 0xa1, 3, &[1, 2, 3], 255)?;
            if !crc {
                bytes.remove(bytes.len() - 2);
            }
            for split in 0..=bytes.len() {
                let mut decoder = Decoder::new(header, 255, crc);
                let mut frames = decoder.feed_positioned(&bytes[..split]);
                frames.extend(decoder.feed_positioned(&bytes[split..]));
                assert_eq!(frames.len(), 1);
                assert_eq!(frames[0].0, 1);
                assert_eq!(frames[0].1.payload, [1, 2, 3]);
            }
            let mut decoder = Decoder::new(header, 255, crc);
            let packets = [bytes.clone(), bytes.clone(), vec![header[0]]].concat();
            let frames = decoder.feed_positioned(&packets);
            assert_eq!(frames.len(), 2);
            assert_eq!(frames[1].0, bytes.len() as u64 + 1);
            let tail = decoder.feed_positioned(&bytes[1..]);
            assert_eq!(tail.len(), 1);
            assert_eq!(tail[0].0, 2 * bytes.len() as u64 + 1);
        }
        Ok(())
    }
    #[test]
    fn oversized_lengths_and_invalid_terminators_do_not_poison_decoder() -> crate::Result<()> {
        let bytes = encode([0x51, 0x78], 0xa3, 1, &[0], 255)?;
        let mut corrupt = bytes.clone();
        corrupt[8] = 0;
        let oversized = [0x51, 0x78, 0xa3, 1, 0xff, 0xff];
        let mut decoder = Decoder::new([0x51, 0x78], 255, true);
        assert_eq!(decoder.feed_positioned(&oversized), []);
        assert_eq!(decoder.feed_positioned(&corrupt), []);
        let frames = decoder.feed_positioned(&bytes);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, (oversized.len() + corrupt.len() + 1) as u64);
        Ok(())
    }

    #[test]
    fn finish_into_matches_encode_and_checks_output_limits() -> crate::Result<()> {
        let payload = [1, 2, 3, 4];
        let mut output = [0; 12];
        output[6..10].copy_from_slice(&payload);
        let len = super::finish_into([0x51, 0x78], 0xa1, 0, payload.len(), 4, &mut output)?;
        assert_eq!(len, 12);
        assert_eq!(output.to_vec(), encode([0x51, 0x78], 0xa1, 0, &payload, 4)?);
        assert!(super::finish_into([0; 2], 0, 0, 4, 4, &mut [0; 11]).is_err());
        assert!(super::finish_into([0; 2], 0, 0, 5, 4, &mut [0; 13]).is_err());
        Ok(())
    }
}
