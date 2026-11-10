/// UMP (Universal Media Protocol) parser for YouTube's SABR streaming protocol.
///
/// UMP frames arrive as a chunked HTTP stream. Each frame (called a "part") is:
///   [varint: part_type] [varint: part_size] [raw bytes: part_data]
///
/// The varint encoding is YouTube's own variable-length integer format, NOT
/// standard protobuf varint.

// ---------------------------------------------------------------------------
// Part type constants
// ---------------------------------------------------------------------------

pub const MEDIA_HEADER: u32 = 20;
pub const MEDIA: u32 = 21;
pub const MEDIA_END: u32 = 22;
pub const NEXT_REQUEST_POLICY: u32 = 35;
pub const FORMAT_INITIALIZATION_METADATA: u32 = 42;
pub const SABR_REDIRECT: u32 = 43;
pub const SABR_ERROR: u32 = 44;
pub const RELOAD_PLAYER_RESPONSE: u32 = 46;
pub const PLAYBACK_START_POLICY: u32 = 47;
pub const SABR_CONTEXT_UPDATE: u32 = 57;
pub const STREAM_PROTECTION_STATUS: u32 = 58;
pub const SABR_CONTEXT_SENDING_POLICY: u32 = 59;

// ---------------------------------------------------------------------------
// Variable-length integer codec
// ---------------------------------------------------------------------------

/// Read a YouTube-style varint from `buf`.
///
/// Returns `Some((value, bytes_consumed))` or `None` if `buf` is too short.
///
/// Encoding:
///   1 byte:  first < 128      -> value = byte
///   2 bytes: first 128..192   -> value = (b0 & 0x3F) + 64 * b1
///   3 bytes: first 192..224   -> value = (b0 & 0x1F) + 32 * (b1 + 256 * b2)
///   4 bytes: first 224..240   -> value = (b0 & 0x0F) + 16 * (b1 + 256 * (b2 + 256 * b3))
///   5 bytes: first >= 240     -> value = u32::from_le_bytes(buf[1..5])
pub fn read_varint(buf: &[u8]) -> Option<(u32, usize)> {
    let first = *buf.first()?;

    if first < 128 {
        // 1 byte
        Some((first as u32, 1))
    } else if first < 192 {
        // 2 bytes
        let b1 = *buf.get(1)?;
        let value = (first as u32 & 0x3F) + 64 * b1 as u32;
        Some((value, 2))
    } else if first < 224 {
        // 3 bytes
        let b1 = *buf.get(1)?;
        let b2 = *buf.get(2)?;
        let value = (first as u32 & 0x1F) + 32 * (b1 as u32 + 256 * b2 as u32);
        Some((value, 3))
    } else if first < 240 {
        // 4 bytes
        let b1 = *buf.get(1)?;
        let b2 = *buf.get(2)?;
        let b3 = *buf.get(3)?;
        let value =
            (first as u32 & 0x0F) + 16 * (b1 as u32 + 256 * (b2 as u32 + 256 * b3 as u32));
        Some((value, 4))
    } else {
        // 5 bytes: first >= 240, next 4 bytes are a little-endian u32
        if buf.len() < 5 {
            return None;
        }
        let value = u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]);
        Some((value, 5))
    }
}

/// Encode a value as a YouTube-style varint and append it to `out`.
pub fn write_varint(out: &mut Vec<u8>, value: u32) {
    if value < 128 {
        // 1 byte
        out.push(value as u8);
    } else if value < 1 << 14 {
        // 2 bytes: value = (b0 & 0x3F) + 64 * b1, max 0x3F + 64 * 255 = 2^14 - 1
        // b0 high bits = 10xx_xxxx (128..192)
        let lo = value % 64;
        let hi = value / 64;
        out.push(128 | lo as u8);
        out.push(hi as u8);
    } else if value < 1 << 21 {
        // 3 bytes: value = (b0 & 0x1F) + 32 * (b1 + 256 * b2), max 2^21 - 1
        // b0 high bits = 110x_xxxx (192..224)
        let lo = value % 32;
        let rest = value / 32;
        let b1 = rest % 256;
        let b2 = rest / 256;
        out.push(192 | lo as u8);
        out.push(b1 as u8);
        out.push(b2 as u8);
    } else if value < 1 << 28 {
        // 4 bytes: value = (b0 & 0x0F) + 16 * (b1 + 256 * (b2 + 256 * b3)), max 2^28 - 1
        // b0 high bits = 1110_xxxx (224..240)
        let lo = value % 16;
        let rest = value / 16;
        let b1 = rest % 256;
        let rest = rest / 256;
        let b2 = rest % 256;
        let b3 = rest / 256;
        out.push(224 | lo as u8);
        out.push(b1 as u8);
        out.push(b2 as u8);
        out.push(b3 as u8);
    } else {
        // 5 bytes: prefix byte >= 240, then raw LE u32
        out.push(240);
        out.extend_from_slice(&value.to_le_bytes());
    }
}

// ---------------------------------------------------------------------------
// UMP part
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct UmpPart {
    pub part_type: u32,
    pub data: Vec<u8>,
}

/// Compact one-line form of a response's part ids for the log, e.g.
/// `20,21x3,22,35`: consecutive repeats collapse to `idxN`.
pub fn summarize_part_ids(ids: &[u32]) -> String {
    let mut runs: Vec<(u32, usize)> = Vec::new();
    for &id in ids {
        match runs.last_mut() {
            Some((last, count)) if *last == id => *count += 1,
            _ => runs.push((id, 1)),
        }
    }
    if runs.is_empty() {
        return "-".to_owned();
    }
    runs.iter()
        .map(|&(id, n)| if n == 1 { id.to_string() } else { format!("{id}x{n}") })
        .collect::<Vec<_>>()
        .join(",")
}

// ---------------------------------------------------------------------------
// Streaming parser
// ---------------------------------------------------------------------------

/// Accumulates chunked HTTP response data and yields complete UMP parts.
pub struct UmpParser {
    buffer: Vec<u8>,
}

impl UmpParser {
    pub fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// Append raw bytes from the latest HTTP response chunk.
    pub fn push(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
    }

    /// Try to extract the next complete UMP part from the internal buffer.
    ///
    /// Returns `None` if there is not enough data yet -- call `push` with more
    /// bytes and try again.
    pub fn next_part(&mut self) -> Option<UmpPart> {
        let buf = &self.buffer;

        // 1. Read part_type varint.
        let (part_type, type_len) = read_varint(buf)?;

        // 2. Read part_size varint (starts right after the type varint).
        let (part_size, size_len) = read_varint(&buf[type_len..])?;

        let header_len = type_len + size_len;
        let total_len = header_len + part_size as usize;

        // 3. Check if the full part body is available.
        if buf.len() < total_len {
            return None;
        }

        // 4. Extract the part data.
        let data = buf[header_len..total_len].to_vec();

        // 5. Remove the consumed bytes from the front of the buffer.
        self.buffer.drain(..total_len);

        Some(UmpPart { part_type, data })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- varint round-trip ---------------------------------------------------

    /// Encode then decode `value`, asserting it round-trips in exactly `len` bytes.
    fn roundtrip(value: u32, len: usize) {
        let mut buf = Vec::new();
        write_varint(&mut buf, value);
        assert_eq!(buf.len(), len, "encoded length mismatch for {value}");
        let (decoded, consumed) = read_varint(&buf).expect("should decode");
        assert_eq!(decoded, value, "value mismatch for {value}");
        assert_eq!(consumed, len, "consumed length mismatch for {value}");
    }

    #[test]
    fn varint_1_byte() {
        roundtrip(0, 1);
        roundtrip(1, 1);
        roundtrip(127, 1);
    }

    #[test]
    fn varint_2_bytes() {
        roundtrip(128, 2);
        roundtrip(1000, 2);
        // Max 2-byte: 0x3F + 64 * 255 = 16383 = 2^14 - 1
        roundtrip(16383, 2);
    }

    #[test]
    fn varint_3_bytes() {
        roundtrip(16384, 3);
        roundtrip(16511, 3);
        roundtrip(100_000, 3);
        // Max 3-byte: 0x1F + 32 * (255 + 256 * 255) = 2^21 - 1
        roundtrip(2_097_151, 3);
    }

    #[test]
    fn varint_4_bytes() {
        roundtrip(2_097_152, 4);
        roundtrip(2_097_183, 4);
        roundtrip(10_000_000, 4);
        // Max 4-byte: 0x0F + 16 * (255 + 256 * (255 + 256 * 255)) = 2^28 - 1
        roundtrip(268_435_455, 4);
    }

    #[test]
    fn varint_5_bytes() {
        roundtrip(268_435_456, 5);
        roundtrip(268_435_471, 5);
        roundtrip(u32::MAX, 5);
    }

    // -- reference vectors ---------------------------------------------------
    //
    // Derived from the googlevideo / YouTube.js UMP reader: the leading 1-bits of
    // the first byte give the length, the remaining low bits of the first byte are
    // the least-significant bits, and following bytes are little-endian.

    const REFERENCE_VECTORS: &[(&[u8], u32)] = &[
        (&[0x00], 0),
        (&[0x7F], 127),
        (&[0x80, 0x01], 64),
        (&[0x80, 0x02], 128),
        (&[0xBF, 0xFF], 16_383),
        (&[0xC0, 0x00, 0x02], 16_384),
        (&[0xDF, 0xFF, 0xFF], 2_097_151),
        (&[0xE0, 0x00, 0x00, 0x02], 2_097_152),
        (&[0xEF, 0xFF, 0xFF, 0xFF], 268_435_455),
        (&[0xF0, 0x00, 0x00, 0x00, 0x10], 268_435_456),
        (&[0xF0, 0xFF, 0xFF, 0xFF, 0xFF], u32::MAX),
    ];

    #[test]
    fn decoder_matches_reference_vectors() {
        for &(bytes, expected) in REFERENCE_VECTORS {
            assert_eq!(
                read_varint(bytes),
                Some((expected, bytes.len())),
                "decoding {bytes:02X?}"
            );
        }
    }

    #[test]
    fn decoder_ignores_low_bits_of_5_byte_prefix() {
        assert_eq!(read_varint(&[0xFF, 0x01, 0x00, 0x00, 0x00]), Some((1, 5)));
    }

    #[test]
    fn encoder_matches_reference_vectors() {
        for &(bytes, value) in REFERENCE_VECTORS {
            // [0x80, 0x01] is a valid (non-minimal) encoding of 64; the encoder
            // uses the shortest form, so skip it here.
            if value == 64 {
                continue;
            }
            let mut out = Vec::new();
            write_varint(&mut out, value);
            assert_eq!(out, bytes, "encoding {value}");
        }
    }

    // -- read_varint edge cases ----------------------------------------------

    #[test]
    fn read_varint_empty() {
        assert!(read_varint(&[]).is_none());
    }

    #[test]
    fn read_varint_short_buffer() {
        // 2-byte varint but only 1 byte present
        assert!(read_varint(&[0x80]).is_none());
        // 5-byte varint but only 3 bytes present
        assert!(read_varint(&[0xF0, 0x01, 0x02]).is_none());
    }

    // -- part sequence summary -----------------------------------------------

    #[test]
    fn summary_collapses_runs_of_the_same_part() {
        let ids = [20, 21, 21, 21, 22, 20, 21, 22, 35];
        assert_eq!(summarize_part_ids(&ids), "20,21x3,22,20,21,22,35");
    }

    #[test]
    fn summary_of_reload_loop_response() {
        assert_eq!(
            summarize_part_ids(&[RELOAD_PLAYER_RESPONSE, PLAYBACK_START_POLICY, NEXT_REQUEST_POLICY]),
            "46,47,35"
        );
        assert_eq!(summarize_part_ids(&[]), "-");
    }

    // -- parser --------------------------------------------------------------

    #[test]
    fn parser_single_part() {
        let mut out = Vec::new();
        write_varint(&mut out, MEDIA_HEADER); // part type = 20
        write_varint(&mut out, 4); // part size = 4
        out.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // body

        let mut parser = UmpParser::new();
        parser.push(&out);

        let part = parser.next_part().expect("should yield a part");
        assert_eq!(part.part_type, MEDIA_HEADER);
        assert_eq!(part.data, vec![0xDE, 0xAD, 0xBE, 0xEF]);

        assert!(parser.next_part().is_none());
    }

    #[test]
    fn parser_multiple_parts() {
        let mut out = Vec::new();
        for i in 0..3 {
            write_varint(&mut out, MEDIA);
            write_varint(&mut out, 2);
            out.extend_from_slice(&[i, i + 10]);
        }

        let mut parser = UmpParser::new();
        parser.push(&out);

        for i in 0..3u8 {
            let part = parser.next_part().expect("should yield a part");
            assert_eq!(part.part_type, MEDIA);
            assert_eq!(part.data, vec![i, i + 10]);
        }
        assert!(parser.next_part().is_none());
    }

    #[test]
    fn parser_chunked_delivery() {
        // Build a part with a 4-byte body.
        let mut out = Vec::new();
        write_varint(&mut out, SABR_ERROR);
        write_varint(&mut out, 4);
        out.extend_from_slice(&[1, 2, 3, 4]);

        let mut parser = UmpParser::new();

        // Feed one byte at a time -- the parser must wait until it has everything.
        for (i, &byte) in out.iter().enumerate() {
            assert!(
                parser.next_part().is_none(),
                "should not yield before all bytes are pushed (byte {i})"
            );
            parser.push(&[byte]);
        }

        let part = parser.next_part().expect("should yield after all bytes");
        assert_eq!(part.part_type, SABR_ERROR);
        assert_eq!(part.data, vec![1, 2, 3, 4]);
    }

    #[test]
    fn parser_empty_body() {
        let mut out = Vec::new();
        write_varint(&mut out, MEDIA_END);
        write_varint(&mut out, 0);

        let mut parser = UmpParser::new();
        parser.push(&out);

        let part = parser.next_part().expect("should yield a part");
        assert_eq!(part.part_type, MEDIA_END);
        assert!(part.data.is_empty());
    }
}
