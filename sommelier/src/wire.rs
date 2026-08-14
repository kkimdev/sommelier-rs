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
    InvalidObjectId(u32),
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
        let total_len = self
            .payload
            .len()
            .checked_add(8)
            .ok_or(ProtocolError::MessageTooLarge(usize::MAX))?;
        if total_len > 0xFFFF {
            return Err(ProtocolError::MessageTooLarge(total_len));
        }
        if !total_len.is_multiple_of(4) {
            return Err(ProtocolError::InvalidMessageLength(total_len));
        }
        let total_len = total_len as u32;
        let mut msg = Vec::with_capacity(8 + self.payload.len());
        msg.extend_from_slice(&sender_id.to_ne_bytes());
        msg.extend_from_slice(&((total_len << 16) | opcode as u32).to_ne_bytes());
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
    /// Wire layout per the Wayland specification §4.3 (Wire Format):
    ///   - Word 0 (bytes 0–3): `sender_id` (u32, native-endian)
    ///   - Word 1 (bytes 4–7): `(total_len_bytes << 16) | opcode` (u32, native-endian)
    ///     - Upper 16 bits: total message length in bytes (header + payload)
    ///     - Lower 16 bits: request/event opcode
    ///   - Remaining bytes: payload accumulated via `write_*` calls
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
    use super::{MessageBuilder, ProtocolError, WireMessage};

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
