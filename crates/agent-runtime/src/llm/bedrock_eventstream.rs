//! Pure binary frame decoder for the AWS event-stream wire format used by
//! Bedrock `invoke-with-response-stream` and `converse-stream` responses.
//!
//! Each frame is laid out as:
//! ```text
//! total_len  : u32 BE       (header_len + payload_len + 16)
//! header_len : u32 BE
//! prelude_crc: u32 BE       (CRC32 of total_len ++ header_len)
//! headers    : [name_len: u8, name, value_type: u8, value...]*
//! payload    : bytes
//! message_crc: u32 BE       (CRC32 of total_len .. payload end)
//! ```

use std::collections::HashMap;

use eyre::{bail, Result};

const PRELUDE_LEN: usize = 12;
const MESSAGE_CRC_LEN: usize = 4;
const HEADER_VALUE_STRING: u8 = 7;

/// One decoded AWS event-stream frame.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Value of the `:event-type` header (e.g. `"chunk"`, `"messageStart"`,
    /// `"exception"`). Empty when no `:event-type` header is present.
    pub event_type: String,
    /// Value of the `:message-type` header (`"event"` for normal frames,
    /// `"exception"` for service-side error frames). Empty when absent.
    pub message_type: String,
    /// Value of the `:exception-type` header for exception frames (e.g.
    /// `"ThrottlingException"`). Empty when absent.
    pub exception_type: String,
    pub payload: Vec<u8>,
}

/// Streaming decoder that accepts arbitrary byte chunks and yields whole
/// frames as they become available. Partial frames remain buffered.
#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Try to decode the next complete frame.
    ///
    /// Returns `Ok(None)` when fewer than one full frame is buffered.
    /// Returns `Err` for any malformed frame (bad CRC, unknown header format,
    /// truncated header section, etc.).
    pub fn next_frame(&mut self) -> Result<Option<Frame>> {
        if self.buf.len() < PRELUDE_LEN {
            return Ok(None);
        }

        let total_len = u32::from_be_bytes(self.buf[0..4].try_into().unwrap()) as usize;
        let header_len = u32::from_be_bytes(self.buf[4..8].try_into().unwrap()) as usize;
        let prelude_crc = u32::from_be_bytes(self.buf[8..12].try_into().unwrap());

        if total_len < PRELUDE_LEN + MESSAGE_CRC_LEN {
            bail!("event-stream frame total_len {total_len} smaller than prelude+crc");
        }
        if header_len > total_len.saturating_sub(PRELUDE_LEN + MESSAGE_CRC_LEN) {
            bail!("event-stream frame header_len {header_len} exceeds payload region");
        }

        let expected_prelude_crc = crc32fast::hash(&self.buf[0..8]);
        if expected_prelude_crc != prelude_crc {
            bail!(
                "event-stream prelude CRC mismatch: expected {:#x}, got {:#x}",
                expected_prelude_crc,
                prelude_crc
            );
        }

        if self.buf.len() < total_len {
            return Ok(None);
        }

        let message_crc = u32::from_be_bytes(
            self.buf[total_len - MESSAGE_CRC_LEN..total_len]
                .try_into()
                .unwrap(),
        );
        let expected_message_crc = crc32fast::hash(&self.buf[0..total_len - MESSAGE_CRC_LEN]);
        if expected_message_crc != message_crc {
            bail!(
                "event-stream message CRC mismatch: expected {:#x}, got {:#x}",
                expected_message_crc,
                message_crc
            );
        }

        let headers_start = PRELUDE_LEN;
        let headers_end = headers_start + header_len;
        let payload_end = total_len - MESSAGE_CRC_LEN;
        let headers = decode_headers(&self.buf[headers_start..headers_end])?;
        let payload = self.buf[headers_end..payload_end].to_vec();

        let take_header = |name: &str| headers.get(name).cloned().unwrap_or_default();
        let frame = Frame {
            event_type: take_header(":event-type"),
            message_type: take_header(":message-type"),
            exception_type: take_header(":exception-type"),
            payload,
        };

        self.buf.drain(0..total_len);
        Ok(Some(frame))
    }
}

fn decode_headers(mut bytes: &[u8]) -> Result<HashMap<String, String>> {
    let mut out: HashMap<String, String> = HashMap::new();
    while !bytes.is_empty() {
        if bytes.len() < 2 {
            bail!("truncated header");
        }
        let name_len = bytes[0] as usize;
        bytes = &bytes[1..];
        if bytes.len() < name_len + 1 {
            bail!("truncated header name");
        }
        let name = std::str::from_utf8(&bytes[..name_len])
            .map_err(|_| eyre::eyre!("non-utf8 header name"))?
            .to_string();
        bytes = &bytes[name_len..];
        let value_type = bytes[0];
        bytes = &bytes[1..];
        let value = if value_type == HEADER_VALUE_STRING {
            if bytes.len() < 2 {
                bail!("truncated string header value length");
            }
            let value_len = u16::from_be_bytes(bytes[..2].try_into().unwrap()) as usize;
            bytes = &bytes[2..];
            if bytes.len() < value_len {
                bail!("truncated string header value");
            }
            let v = std::str::from_utf8(&bytes[..value_len])
                .map_err(|_| eyre::eyre!("non-utf8 header value"))?
                .to_string();
            bytes = &bytes[value_len..];
            v
        } else {
            bail!("unsupported event-stream header value type {value_type}");
        };
        out.insert(name, value);
    }
    Ok(out)
}

/// Build a single AWS event-stream frame.
///
/// Used in tests and by any caller that needs to synthesise frames (e.g. test
/// fixtures). Headers are encoded as string-typed.
pub fn encode_frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
    let mut header_bytes: Vec<u8> = Vec::new();
    for (name, value) in headers {
        let name_bytes = name.as_bytes();
        let value_bytes = value.as_bytes();
        header_bytes.push(name_bytes.len() as u8);
        header_bytes.extend_from_slice(name_bytes);
        header_bytes.push(HEADER_VALUE_STRING);
        header_bytes.extend_from_slice(&(value_bytes.len() as u16).to_be_bytes());
        header_bytes.extend_from_slice(value_bytes);
    }
    let total_len = (PRELUDE_LEN + header_bytes.len() + payload.len() + MESSAGE_CRC_LEN) as u32;
    let header_len = header_bytes.len() as u32;

    let mut frame = Vec::with_capacity(total_len as usize);
    frame.extend_from_slice(&total_len.to_be_bytes());
    frame.extend_from_slice(&header_len.to_be_bytes());
    let prelude_crc = crc32fast::hash(&frame);
    frame.extend_from_slice(&prelude_crc.to_be_bytes());
    frame.extend_from_slice(&header_bytes);
    frame.extend_from_slice(payload);
    let msg_crc = crc32fast::hash(&frame);
    frame.extend_from_slice(&msg_crc.to_be_bytes());
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_frame() -> Vec<u8> {
        encode_frame(
            &[(":event-type", "chunk"), (":message-type", "event")],
            br#"{"hello":"world"}"#,
        )
    }

    #[test]
    fn decode_single_frame_roundtrip() {
        let frame = make_frame();
        let mut decoder = FrameDecoder::new();
        decoder.feed(&frame);
        let result = decoder.next_frame().unwrap().unwrap();
        assert_eq!(result.event_type, "chunk");
        assert_eq!(result.message_type, "event");
        assert_eq!(result.payload, br#"{"hello":"world"}"#);
        assert!(decoder.next_frame().unwrap().is_none());
    }

    #[test]
    fn decode_streamed_partial_frame_buffers() {
        let frame = make_frame();
        let (head, tail) = frame.split_at(frame.len() / 2);
        let mut decoder = FrameDecoder::new();
        decoder.feed(head);
        assert!(decoder.next_frame().unwrap().is_none());
        decoder.feed(tail);
        let result = decoder.next_frame().unwrap().unwrap();
        assert_eq!(result.event_type, "chunk");
    }

    #[test]
    fn decode_two_frames_back_to_back() {
        let a = encode_frame(&[(":event-type", "chunk")], b"a");
        let b = encode_frame(&[(":event-type", "chunk")], b"b");
        let mut decoder = FrameDecoder::new();
        decoder.feed(&a);
        decoder.feed(&b);
        assert_eq!(decoder.next_frame().unwrap().unwrap().payload, b"a");
        assert_eq!(decoder.next_frame().unwrap().unwrap().payload, b"b");
        assert!(decoder.next_frame().unwrap().is_none());
    }

    #[test]
    fn decode_rejects_bad_prelude_crc() {
        let mut frame = make_frame();
        frame[8] ^= 0xff;
        let mut decoder = FrameDecoder::new();
        decoder.feed(&frame);
        assert!(decoder.next_frame().is_err());
    }

    #[test]
    fn decode_rejects_bad_message_crc() {
        let mut frame = make_frame();
        let last = frame.len() - 1;
        frame[last] ^= 0xff;
        let mut decoder = FrameDecoder::new();
        decoder.feed(&frame);
        assert!(decoder.next_frame().is_err());
    }

    #[test]
    fn exception_frame_carries_exception_type_header() {
        let frame = encode_frame(
            &[
                (":event-type", "ThrottlingException"),
                (":message-type", "exception"),
                (":exception-type", "ThrottlingException"),
            ],
            br#"{"message":"slow down"}"#,
        );
        let mut decoder = FrameDecoder::new();
        decoder.feed(&frame);
        let f = decoder.next_frame().unwrap().unwrap();
        assert_eq!(f.message_type, "exception");
        assert_eq!(f.exception_type, "ThrottlingException");
        assert_eq!(f.payload, br#"{"message":"slow down"}"#);
    }

    #[test]
    fn unsupported_header_value_type_errors() {
        let mut bytes = Vec::new();
        bytes.push(4u8);
        bytes.extend_from_slice(b"name");
        bytes.push(0u8);
        let result = decode_headers(&bytes);
        assert!(result.is_err());
    }
}
