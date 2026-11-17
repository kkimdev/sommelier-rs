/*
Copyright 2026 Google LLC

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

     https://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use std::convert::TryInto;
use std::fmt;
use std::os::unix::io::RawFd;

/// The object ID of the `wl_display` singleton on every connection.
pub const WL_DISPLAY_ID: u32 = 1;

/// The header that starts every Wayland wire message.
///
/// Wire layout (Wayland specification, "Wire Format"), both words
/// native-endian:
///   - word 0 (bytes 0-3): the sender object ID;
///   - word 1 (bytes 4-7): the total message length in bytes, header
///     included, in the upper 16 bits and the opcode in the lower 16 bits.
///
/// This type owns that layout: every header is decoded by [`Self::parse`]
/// (or [`Self::frame`]) and written by [`Self::encode`] or
/// [`Self::set_sender`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageHeader {
    pub sender_id: u32,
    pub opcode: u16,
    /// The total message length in bytes, header included.
    pub len: usize,
}

/// How much of a message the start of a byte stream holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// The header or the body is still arriving.
    Partial,
    /// The length field cannot describe a message; the stream is corrupt.
    Invalid(MessageHeader),
    /// A whole message of `len` bytes.
    Complete(MessageHeader),
}

impl MessageHeader {
    /// The size of the header in bytes.
    pub const LEN: usize = 8;
    /// The largest length the 16-bit length field can carry.
    const MAX_LEN: usize = u16::MAX as usize;

    /// Decode the header at the start of `bytes`, or `None` while fewer than
    /// [`Self::LEN`] bytes are available.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let (sender, rest) = bytes.split_first_chunk::<4>()?;
        let word = u32::from_ne_bytes(*rest.first_chunk::<4>()?);
        Some(Self {
            sender_id: u32::from_ne_bytes(*sender),
            opcode: (word & 0xffff) as u16,
            len: (word >> 16) as usize,
        })
    }

    /// Whether the length field describes a message the wire can carry: at
    /// least a header, 4-byte aligned, and within the 16-bit field.
    pub fn has_valid_len(&self) -> bool {
        (Self::LEN..=Self::MAX_LEN).contains(&self.len) && self.len.is_multiple_of(4)
    }

    /// Frame the message at the start of the byte stream `stream`.
    pub fn frame(stream: &[u8]) -> Framing {
        match Self::parse(stream) {
            None => Framing::Partial,
            Some(header) if !header.has_valid_len() => Framing::Invalid(header),
            Some(header) if header.len > stream.len() => Framing::Partial,
            Some(header) => Framing::Complete(header),
        }
    }

    /// Replace the sender ID of the encoded message `message`.
    pub fn set_sender(message: &mut [u8], sender_id: u32) {
        let sender = message
            .first_chunk_mut::<4>()
            .expect("an encoded message starts with its header");
        *sender = sender_id.to_ne_bytes();
    }

    /// Encode the header, or report why its length cannot be encoded.
    fn encode(&self) -> Result<[u8; Self::LEN], ProtocolError> {
        if self.len > Self::MAX_LEN {
            return Err(ProtocolError::MessageTooLarge(self.len));
        }
        if !self.has_valid_len() {
            return Err(ProtocolError::InvalidMessageLength(self.len));
        }
        let word = ((self.len as u32) << 16) | u32::from(self.opcode);
        let mut header = [0; Self::LEN];
        header[..4].copy_from_slice(&self.sender_id.to_ne_bytes());
        header[4..].copy_from_slice(&word.to_ne_bytes());
        Ok(header)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Forward,
    Drop,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ProtocolError {
    InsufficientData,
    InvalidString,
    MissingFd,
    /// An object argument or `new_id` that cannot be used.
    InvalidObjectId(u32),
    /// The message's sender is not a usable object: unknown, or destroyed
    /// and awaiting `delete_id`.
    InvalidSender(u32),
    UnsupportedVersion {
        object_id: u32,
        required: u32,
        actual: u32,
    },
    MessageTooLarge(usize),
    InvalidMessageLength(usize),
    TrailingData,
    UnknownOpcode(u16),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolError::InsufficientData => write!(f, "Insufficient data in wire message"),
            ProtocolError::InvalidString => write!(f, "Invalid string in wire message"),
            ProtocolError::MissingFd => write!(f, "Missing file descriptor in wire message"),
            ProtocolError::InvalidObjectId(id) => {
                write!(f, "Invalid or unmapped Wayland object ID: {}", id)
            }
            ProtocolError::InvalidSender(id) => {
                write!(f, "Invalid or unmapped Wayland sender ID: {}", id)
            }
            ProtocolError::UnsupportedVersion {
                object_id,
                required,
                actual,
            } => write!(
                f,
                "Wayland object {} only supports version {}, but version {} is required",
                object_id, actual, required
            ),
            ProtocolError::MessageTooLarge(length) => {
                write!(
                    f,
                    "Wayland message is too large for the wire format: {}",
                    length
                )
            }
            ProtocolError::InvalidMessageLength(length) => {
                write!(f, "Invalid Wayland message length: {}", length)
            }
            ProtocolError::TrailingData => write!(f, "Trailing data in wire message"),
            ProtocolError::UnknownOpcode(opcode) => write!(f, "Unknown opcode: {}", opcode),
        }
    }
}

impl std::error::Error for ProtocolError {}

#[derive(Debug)]
pub struct WireMessage<'a> {
    pub sender_id: u32,
    pub opcode: u16,
    pub payload: &'a [u8],
    pub fds: &'a [RawFd],
    pub offset: usize,
    pub fd_offset: usize,
}

impl<'a> WireMessage<'a> {
    pub fn new(sender_id: u32, opcode: u16, payload: &'a [u8], fds: &'a [RawFd]) -> Self {
        Self {
            sender_id,
            opcode,
            payload,
            fds,
            offset: 0,
            fd_offset: 0,
        }
    }

    pub fn read_u32(&mut self) -> Result<u32, ProtocolError> {
        let Some(end) = self.offset.checked_add(4) else {
            return Err(ProtocolError::InsufficientData);
        };
        if end > self.payload.len() {
            log::error!(
                "InsufficientData in read_u32: offset {} > payload len {}",
                end,
                self.payload.len()
            );
            return Err(ProtocolError::InsufficientData);
        }
        let bytes = &self.payload[self.offset..end];
        self.offset = end;
        Ok(u32::from_ne_bytes(bytes.try_into().unwrap()))
    }

    pub fn read_i32(&mut self) -> Result<i32, ProtocolError> {
        self.read_u32().map(|v| v as i32)
    }

    /// Read a Wayland `fixed` value without converting it through a float.
    ///
    /// The wire representation is a signed 24.8 integer (`wl_fixed_t`).
    /// Keeping that raw value is important when proxying: converting through
    /// `f32` loses low bits for values outside the exactly-representable
    /// integer range and changes the client's message on re-encoding.
    pub fn read_fixed(&mut self) -> Result<i32, ProtocolError> {
        self.read_i32()
    }

    fn read_string_value(&mut self, nullable: bool) -> Result<Option<String>, ProtocolError> {
        let len = self.read_u32()? as usize;
        if len == 0 {
            return if nullable {
                Ok(None)
            } else {
                Err(ProtocolError::InvalidString)
            };
        }
        // Strings are padded to 32-bit boundary
        let Some(padded_len) = len.checked_add(3).map(|value| value & !3) else {
            return Err(ProtocolError::InsufficientData);
        };

        let Some(end) = self.offset.checked_add(padded_len) else {
            return Err(ProtocolError::InsufficientData);
        };
        if end > self.payload.len() {
            log::error!(
                "InsufficientData in read_string: offset {} + padded_len {} > payload len {}",
                self.offset,
                padded_len,
                self.payload.len()
            );
            return Err(ProtocolError::InsufficientData);
        }

        let Some(bytes_end) = self.offset.checked_add(len) else {
            return Err(ProtocolError::InsufficientData);
        };
        let bytes = &self.payload[self.offset..bytes_end];

        // Wayland strings include null terminator in the length.
        // We expect at least one byte for the null terminator if len > 0.
        if bytes.last() != Some(&0) {
            return Err(ProtocolError::InvalidString);
        }

        // To match C behavior, we truncate at the first null terminator if there are interior nulls
        let null_pos = bytes
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(len.saturating_sub(1));
        let s = std::str::from_utf8(&bytes[..null_pos])
            .map_err(|_| ProtocolError::InvalidString)?
            .to_owned();

        self.offset = end;
        Ok(Some(s))
    }

    pub fn read_string(&mut self) -> Result<String, ProtocolError> {
        self.read_string_value(false)?
            .ok_or(ProtocolError::InvalidString)
    }

    pub fn read_nullable_string(&mut self) -> Result<Option<String>, ProtocolError> {
        self.read_string_value(true)
    }

    pub fn read_array(&mut self) -> Result<Vec<u8>, ProtocolError> {
        let len = self.read_u32()? as usize;
        let Some(padded_len) = len.checked_add(3).map(|value| value & !3) else {
            return Err(ProtocolError::InsufficientData);
        };

        let Some(end) = self.offset.checked_add(padded_len) else {
            return Err(ProtocolError::InsufficientData);
        };
        if end > self.payload.len() {
            log::error!(
                "InsufficientData in read_array: offset {} + padded_len {} > payload len {}",
                self.offset,
                padded_len,
                self.payload.len()
            );
            return Err(ProtocolError::InsufficientData);
        }

        let Some(bytes_end) = self.offset.checked_add(len) else {
            return Err(ProtocolError::InsufficientData);
        };
        let bytes = self.payload[self.offset..bytes_end].to_vec();
        self.offset = end;
        Ok(bytes)
    }

    pub fn read_fd(&mut self) -> Result<RawFd, ProtocolError> {
        if self.fd_offset < self.fds.len() {
            let fd = self.fds[self.fd_offset];
            self.fd_offset += 1;
            Ok(fd)
        } else {
            Err(ProtocolError::MissingFd)
        }
    }

    pub fn is_payload_consumed(&self) -> bool {
        self.offset == self.payload.len()
    }
}

pub struct MessageBuilder {
    pub payload: Vec<u8>,
    pub fds: Vec<RawFd>,
}

impl MessageBuilder {
    pub fn new() -> Self {
        Self {
            payload: Vec::new(),
            fds: Vec::new(),
        }
    }

    pub fn write_u32(&mut self, val: u32) {
        self.payload.extend_from_slice(&val.to_ne_bytes());
    }

    pub fn write_i32(&mut self, val: i32) {
        self.write_u32(val as u32);
    }

    /// Write a raw Wayland signed 24.8 (`wl_fixed_t`) value.
    pub fn write_fixed(&mut self, val: i32) {
        self.write_i32(val);
    }

    pub fn write_string(&mut self, val: &str) {
        let len = val.len() as u32 + 1; // +1 for null terminator
        self.write_u32(len);

        self.payload.extend_from_slice(val.as_bytes());
        self.payload.push(0); // null terminator

        // padding
        let padded_len = (len + 3) & !3;
        let padding = padded_len - len;
        self.payload
            .resize(self.payload.len() + padding as usize, 0);
    }

    pub fn write_nullable_string(&mut self, val: Option<&str>) {
        match val {
            Some(value) => self.write_string(value),
            None => self.write_u32(0),
        }
    }

    pub fn write_array(&mut self, val: &[u8]) {
        let len = val.len() as u32;
        self.write_u32(len);

        self.payload.extend_from_slice(val);

        let padded_len = (len + 3) & !3;
        let padding = padded_len - len;
        self.payload
            .resize(self.payload.len() + padding as usize, 0);
    }

    pub fn write_fd(&mut self, fd: RawFd) {
        self.fds.push(fd);
    }
}

impl Default for MessageBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl MessageBuilder {
    /// Assemble a complete Wayland wire message, returning an error instead
    /// of truncating the 16-bit length field when the payload is too large.
    pub fn try_build_message(&self, sender_id: u32, opcode: u16) -> Result<Vec<u8>, ProtocolError> {
        let len = self
            .payload
            .len()
            .checked_add(MessageHeader::LEN)
            .ok_or(ProtocolError::MessageTooLarge(usize::MAX))?;
        let header = MessageHeader {
            sender_id,
            opcode,
            len,
        }
        .encode()?;
        let mut msg = Vec::with_capacity(len);
        msg.extend_from_slice(&header);
        msg.extend_from_slice(&self.payload);
        Ok(msg)
    }

    /// Assemble a complete Wayland wire message into a `Vec<u8>`, consuming `self`.
    ///
    /// `build_message` is the **terminal call** on a `MessageBuilder`. After calling
    /// it the builder is consumed and cannot be reused. This is intentional: it
    /// prevents accidentally building two messages from the same payload buffer,
    /// which would silently duplicate wire data.
    ///
    /// The header layout is owned by [`MessageHeader`]; the payload is the
    /// bytes accumulated via `write_*` calls.
    ///
    /// # Panics
    /// Panics if the total message does not fit within Wayland's 16-bit
    /// length field. Silently truncating the length in release builds would
    /// desynchronise the stream, so this invariant is enforced in all builds.
    ///
    /// # Example
    /// ```rust,ignore
    /// let mut b = MessageBuilder::new();
    /// b.write_u32(serial);
    /// b.write_u32(handled as u32);
    /// let msg = b.build_message(sender_id, ACK_KEY_OPCODE);
    /// ```
    pub fn build_message(self, sender_id: u32, opcode: u16) -> Vec<u8> {
        self.try_build_message(sender_id, opcode)
            .unwrap_or_else(|error| panic!("{error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::{Framing, MessageBuilder, MessageHeader, ProtocolError, WireMessage};
    use crate::test_support::Rng;

    /// Decoding holds for arbitrary bytes: a frame never claims more bytes
    /// than the stream has, no read moves past the payload or the
    /// descriptors, and what `MessageBuilder` writes decodes unchanged.
    #[test]
    fn arbitrary_bytes_decode_within_bounds_and_written_values_round_trip() {
        let mut rng = Rng::new(0);
        let (mut partial, mut invalid, mut complete) = (0, 0, 0);
        for _ in 0..20_000 {
            let len = rng.below(64);
            let mut stream = rng.bytes(len);
            // Random length words are almost never valid, so half the time
            // write a valid one, about as often too long as not.
            if stream.len() >= MessageHeader::LEN && rng.one_in(2) {
                let message_len = 4 * (2 + rng.below(stream.len() / 2)) as u32;
                stream[4..8].copy_from_slice(&((message_len << 16) | 1).to_ne_bytes());
            }
            match MessageHeader::frame(&stream) {
                Framing::Partial => {
                    partial += 1;
                    assert!(MessageHeader::parse(&stream)
                        .is_none_or(|header| header.len > stream.len()))
                }
                Framing::Invalid(header) => {
                    invalid += 1;
                    assert!(!header.has_valid_len())
                }
                Framing::Complete(header) => {
                    complete += 1;
                    assert!(header.has_valid_len() && header.len <= stream.len())
                }
            }

            let fds = [3, 4];
            let mut message = WireMessage::new(1, 0, &stream, &fds[..rng.below(fds.len() + 1)]);
            for _ in 0..8 {
                let _ = match rng.below(5) {
                    0 => message.read_u32().map(drop),
                    1 => message.read_string().map(drop),
                    2 => message.read_nullable_string().map(drop),
                    3 => message.read_array().map(drop),
                    _ => message.read_fd().map(drop),
                };
                assert!(message.offset <= message.payload.len());
                assert!(message.fd_offset <= message.fds.len());
            }

            let value = rng.next_u32();
            let text: String = (0..rng.below(12))
                .map(|_| char::from(b' ' + rng.below(95) as u8))
                .collect();
            let len = rng.below(9);
            let array = rng.bytes(len);
            let mut builder = MessageBuilder::new();
            builder.write_u32(value);
            builder.write_string(&text);
            builder.write_array(&array);
            builder.write_nullable_string(None);
            let mut message = WireMessage::new(1, 0, &builder.payload, &[]);
            assert_eq!(message.read_u32(), Ok(value));
            assert_eq!(message.read_string(), Ok(text));
            assert_eq!(message.read_array(), Ok(array));
            assert_eq!(message.read_nullable_string(), Ok(None));
            assert!(message.is_payload_consumed());
        }
        for (framing, count) in [
            ("partial", partial),
            ("invalid", invalid),
            ("complete", complete),
        ] {
            assert!(count > 1000, "only {count} {framing} frames");
        }
    }

    #[test]
    fn header_length_is_the_high_half_of_the_second_word() {
        // Wire bytes are native-endian words; on little-endian targets the
        // opcode occupies bytes 4..6 and the length bytes 6..8, so decoding
        // the length from bytes 4..6 reads the opcode instead.
        let message = MessageBuilder::new().build_message(7, 3);
        assert_eq!(
            MessageHeader::parse(&message),
            Some(MessageHeader {
                sender_id: 7,
                opcode: 3,
                len: 8,
            })
        );
        assert_eq!(MessageHeader::parse(&message[..7]), None);
    }

    #[test]
    fn framing_waits_for_the_whole_message_and_flags_corrupt_lengths() {
        let mut builder = MessageBuilder::new();
        builder.write_u32(2);
        let mut message = builder.build_message(1, 0);
        let header = MessageHeader::parse(&message).unwrap();
        for partial in 0..message.len() {
            assert_eq!(MessageHeader::frame(&message[..partial]), Framing::Partial);
        }
        assert_eq!(MessageHeader::frame(&message), Framing::Complete(header));
        message.extend_from_slice(&[0; 4]);
        assert_eq!(
            MessageHeader::frame(&message),
            Framing::Complete(header),
            "bytes of the next message do not change this one"
        );

        // A length below the header size can never complete.
        message[4..8].copy_from_slice(&(6_u32 << 16).to_ne_bytes());
        assert!(matches!(
            MessageHeader::frame(&message),
            Framing::Invalid(_)
        ));
    }

    #[test]
    fn set_sender_rewrites_only_the_sender() {
        let mut message = MessageBuilder::new().build_message(7, 3);
        MessageHeader::set_sender(&mut message, 9);
        assert_eq!(
            MessageHeader::parse(&message),
            Some(MessageHeader {
                sender_id: 9,
                opcode: 3,
                len: 8,
            })
        );
    }

    #[test]
    fn header_length_validity_matches_the_wire_limits() {
        let header = |len| MessageHeader {
            sender_id: 1,
            opcode: 0,
            len,
        };
        for valid in [8, 12, 65_532] {
            assert!(header(valid).has_valid_len(), "{valid}");
        }
        for invalid in [0, 4, 7, 10, 65_536] {
            assert!(!header(invalid).has_valid_len(), "{invalid}");
        }
    }

    #[test]
    fn wire_message_rejects_truncated_scalar() {
        let mut msg = WireMessage::new(1, 0, &[1, 2, 3], &[]);
        assert_eq!(msg.read_u32(), Err(ProtocolError::InsufficientData));
    }

    #[test]
    fn wire_message_rejects_truncated_string_and_array_padding() {
        // Declared string length is four bytes, but only three bytes (and no
        // complete padded payload) are available.
        let string_length = 4u32.to_ne_bytes();
        let mut string_msg = WireMessage::new(1, 0, &string_length, &[]);
        assert_eq!(
            string_msg.read_string(),
            Err(ProtocolError::InsufficientData)
        );

        let mut array_payload = Vec::new();
        array_payload.extend_from_slice(&5u32.to_ne_bytes());
        array_payload.extend_from_slice(&[1, 2, 3, 4]);
        let mut array_msg = WireMessage::new(1, 0, &array_payload, &[]);
        assert_eq!(array_msg.read_array(), Err(ProtocolError::InsufficientData));
    }

    #[test]
    fn builder_round_trips_padded_string_and_array() {
        let mut builder = MessageBuilder::new();
        builder.write_string("가");
        builder.write_array(&[1, 2, 3]);
        let message = builder.build_message(7, 3);
        let mut wire = WireMessage::new(7, 3, &message[8..], &[]);
        assert_eq!(wire.read_string().unwrap(), "가");
        assert_eq!(wire.read_array().unwrap(), vec![1, 2, 3]);
        assert_eq!(wire.offset, message.len() - 8);
    }

    #[test]
    fn wire_message_rejects_invalid_utf8_strings() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&4u32.to_ne_bytes());
        payload.extend_from_slice(&[0xff, 0x00, 0x00, 0x00]);
        let mut wire = WireMessage::new(1, 0, &payload, &[]);
        assert_eq!(wire.read_string(), Err(ProtocolError::InvalidString));
    }

    #[test]
    fn wire_message_rejects_string_without_trailing_nul() {
        // Wayland's string length includes its terminating NUL. A payload
        // containing four non-NUL bytes must not be silently truncated to
        // "abc" by the protocol boundary.
        let mut payload = Vec::new();
        payload.extend_from_slice(&4u32.to_ne_bytes());
        payload.extend_from_slice(b"abcd");
        let mut wire = WireMessage::new(1, 0, &payload, &[]);
        assert_eq!(wire.read_string(), Err(ProtocolError::InvalidString));
        assert_eq!(
            wire.offset, 4,
            "invalid input must not consume the string payload"
        );
    }

    #[test]
    fn nullable_strings_preserve_null_and_empty_values() {
        let mut builder = MessageBuilder::new();
        builder.write_nullable_string(None);
        builder.write_nullable_string(Some(""));
        builder.write_nullable_string(Some("가"));
        let message = builder.build_message(7, 3);
        let mut wire = WireMessage::new(7, 3, &message[8..], &[]);

        assert_eq!(wire.read_nullable_string().unwrap(), None);
        assert_eq!(wire.read_nullable_string().unwrap(), Some(String::new()));
        assert_eq!(wire.read_nullable_string().unwrap(), Some("가".to_string()));
        assert!(wire.is_payload_consumed());
    }

    #[test]
    fn fixed_wire_values_preserve_all_raw_bits() {
        // 0x01000001 is not exactly representable as an f32 integer. A
        // float-based decode/re-encode silently changes it to 0x01000000.
        let raw = 0x0100_0001i32;
        let mut incoming = MessageBuilder::new();
        incoming.write_i32(raw);
        let packet = incoming.build_message(7, 3);

        let mut wire = WireMessage::new(7, 3, &packet[8..], &[]);
        let decoded = wire.read_fixed().unwrap();
        let mut forwarded = MessageBuilder::new();
        forwarded.write_fixed(decoded);

        assert_eq!(forwarded.payload, raw.to_ne_bytes());
    }

    #[test]
    fn wire_message_reports_trailing_payload() {
        let mut wire = WireMessage::new(1, 0, &[7, 0, 0, 0, 0, 0, 0, 0], &[]);
        assert_eq!(wire.read_u32(), Ok(7));
        assert!(!wire.is_payload_consumed());
        assert_eq!(wire.read_u32(), Ok(0));
        assert!(wire.is_payload_consumed());
    }

    #[test]
    #[should_panic(expected = "Wayland message is too large")]
    fn builder_rejects_oversized_wire_message() {
        let mut builder = MessageBuilder::new();
        builder.payload.resize(65_528, 0);
        let _ = builder.build_message(1, 0);
    }

    #[test]
    fn try_builder_reports_oversized_wire_message() {
        let mut builder = MessageBuilder::new();
        builder.payload.resize(65_528, 0);
        assert_eq!(
            builder.try_build_message(1, 0),
            Err(ProtocolError::MessageTooLarge(65_536))
        );
    }

    #[test]
    fn try_builder_rejects_unaligned_wire_message_length() {
        let mut builder = MessageBuilder::new();
        builder.payload.push(0);
        assert_eq!(
            builder.try_build_message(1, 0),
            Err(ProtocolError::InvalidMessageLength(9)),
            "Wayland message lengths must be 4-byte aligned"
        );
    }
}
