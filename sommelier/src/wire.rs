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

use std::borrow::Cow;
use std::collections::VecDeque;
use std::convert::TryInto;
use std::fmt;
use std::os::fd::OwnedFd;

/// The object ID of the `wl_display` singleton on every connection.
pub const WL_DISPLAY_ID: u32 = 1;

/// One encoded message and the descriptors it carries, in argument order.
///
/// The generated `req::` and `evt::` encoders and forwarding return it, and
/// the proxy's send queues hold it until the message is sent. It owns its
/// descriptors: dropping it, sent or not, closes them.
pub type Outgoing = (Vec<u8>, Vec<OwnedFd>);

/// Queue an encoded message, or log and drop one that could not be encoded.
///
/// Only the encoder of a message with a string or array fails, when the
/// data does not fit [`MessageHeader::MAX_LEN`]. That data can come from the
/// host or a guest, so dropping the one message keeps the session alive
/// instead of ending it.
///
/// # Arguments
///
/// * `what` - names the message in the log, such as
///   `format_args!("startup ID for zaura_surface {id}")`.
///
/// # Returns
///
/// Whether the message was queued.
pub fn queue_or_drop(
    queue: &mut Vec<Outgoing>,
    message: Result<Outgoing, ProtocolError>,
    what: impl fmt::Display,
) -> bool {
    match message {
        Ok(message) => {
            queue.push(message);
            true
        }
        Err(error) => {
            log::warn!("Dropping {what}: {error}");
            false
        }
    }
}

/// The header that starts every Wayland wire message.
///
/// Wire layout (Wayland specification, "Wire Format"), both words
/// native-endian:
///   - word 0 (bytes 0-3): the sender object ID;
///   - word 1 (bytes 4-7): the total message length in bytes, header
///     included, in the upper 16 bits and the opcode in the lower 16 bits.
///
/// This type owns that layout: every header is decoded by [`Self::parse`]
/// (or [`Self::frame`]) and written by [`Self::encode`].
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
    /// The longest message, header included: libwayland's
    /// `WL_MAX_MESSAGE_SIZE`. Both of its peers refuse a longer one, a
    /// client by closing the connection and a compositor with
    /// `invalid_method`, although the 16-bit length field could describe
    /// more. The proxy therefore neither accepts nor builds one.
    pub const MAX_LEN: usize = 4096;

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
    /// least a header, 4-byte aligned, and at most [`Self::MAX_LEN`].
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

/// A string argument that other arguments index by byte offset, such as
/// the text of `zwp_text_input_v3.set_surrounding_text`, whose cursor and
/// anchor are offsets into it. Every other string decodes to a `String`
/// with each sequence that is not UTF-8 replaced by U+FFFD
/// ([`WireMessage::read_string`]), but a replacement here would move the
/// offsets after it, so the handler learns that the string was not UTF-8
/// and drops it. The generator's `BYTE_INDEXED_STRINGS` lists these
/// arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexedString {
    Utf8(String),
    /// The string as received, without its terminating NUL.
    NotUtf8(Vec<u8>),
}

impl IndexedString {
    fn from_bytes(bytes: &[u8]) -> Self {
        match std::str::from_utf8(bytes) {
            Ok(text) => Self::Utf8(text.to_owned()),
            Err(_) => Self::NotUtf8(bytes.to_vec()),
        }
    }

    fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Utf8(text) => text.as_bytes(),
            Self::NotUtf8(bytes) => bytes,
        }
    }
}

/// For tests: production code gets an `IndexedString` only by decoding
/// one, which checks that it is UTF-8.
#[cfg(test)]
impl From<&str> for IndexedString {
    fn from(text: &str) -> Self {
        Self::Utf8(text.to_owned())
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
    /// The connection's received descriptors, in stream order. Decoding an
    /// `fd` argument takes the next one, so the decoded message owns it.
    fds: Option<&'a mut VecDeque<OwnedFd>>,
    pub offset: usize,
}

impl<'a> WireMessage<'a> {
    /// A message whose `fd` arguments are missing, for decoding messages
    /// that take none.
    pub fn new(sender_id: u32, opcode: u16, payload: &'a [u8]) -> Self {
        Self {
            sender_id,
            opcode,
            payload,
            fds: None,
            offset: 0,
        }
    }

    /// Take `fd` arguments from `fds`, the descriptors received with the
    /// stream the message came from.
    pub fn with_fds(self, fds: &'a mut VecDeque<OwnedFd>) -> Self {
        Self {
            fds: Some(fds),
            ..self
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

    /// The bytes of a string argument, without its terminating NUL, or
    /// `None` for a null `nullable` one.
    fn read_string_bytes(&mut self, nullable: bool) -> Result<Option<&'a [u8]>, ProtocolError> {
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

        // A Wayland string's length includes its terminating NUL, and
        // libwayland rejects a string with a NUL before it ("string has
        // embedded nul" in `wl_connection_demarshal`).
        let Some((&0, text)) = bytes.split_last() else {
            return Err(ProtocolError::InvalidString);
        };
        if text.contains(&0) {
            return Err(ProtocolError::InvalidString);
        }

        self.offset = end;
        Ok(Some(text))
    }

    /// `bytes` with each sequence that is not UTF-8 replaced by U+FFFD.
    /// Wayland requires UTF-8, but libwayland does not check it, so a
    /// client that sends Latin-1 keeps its session.
    fn lossy(&self, bytes: &[u8]) -> String {
        match String::from_utf8_lossy(bytes) {
            Cow::Borrowed(text) => text.to_owned(),
            Cow::Owned(text) => {
                log::debug!(
                    "Replaced invalid UTF-8 in a string argument of opcode {} of object {}",
                    self.opcode,
                    self.sender_id
                );
                text
            }
        }
    }

    /// A string argument, whose sequences that are not UTF-8 are replaced
    /// by U+FFFD.
    pub fn read_string(&mut self) -> Result<String, ProtocolError> {
        let bytes = self
            .read_string_bytes(false)?
            .ok_or(ProtocolError::InvalidString)?;
        Ok(self.lossy(bytes))
    }

    /// [`Self::read_string`] of a nullable argument.
    pub fn read_nullable_string(&mut self) -> Result<Option<String>, ProtocolError> {
        let bytes = self.read_string_bytes(true)?;
        Ok(bytes.map(|bytes| self.lossy(bytes)))
    }

    /// A string argument that other arguments index by byte offset.
    pub fn read_indexed_string(&mut self) -> Result<IndexedString, ProtocolError> {
        let bytes = self
            .read_string_bytes(false)?
            .ok_or(ProtocolError::InvalidString)?;
        Ok(IndexedString::from_bytes(bytes))
    }

    /// [`Self::read_indexed_string`] of a nullable argument.
    pub fn read_nullable_indexed_string(&mut self) -> Result<Option<IndexedString>, ProtocolError> {
        Ok(self.read_string_bytes(true)?.map(IndexedString::from_bytes))
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

    pub fn read_fd(&mut self) -> Result<OwnedFd, ProtocolError> {
        self.fds
            .as_mut()
            .and_then(|fds| fds.pop_front())
            .ok_or(ProtocolError::MissingFd)
    }

    pub fn is_payload_consumed(&self) -> bool {
        self.offset == self.payload.len()
    }

    /// `decoded`, the result of decoding this message, unless the decode
    /// succeeded without reading the whole payload: then `TrailingData`. A
    /// failed decode keeps its own error, which says why it stopped.
    pub fn check_consumed<T>(&self, decoded: Result<T, ProtocolError>) -> Result<T, ProtocolError> {
        match decoded {
            Ok(_) if !self.is_payload_consumed() => Err(ProtocolError::TrailingData),
            decoded => decoded,
        }
    }
}

/// The byte-level writer behind the generated encoders.
///
/// Production code builds messages only through the generated `req::` and
/// `evt::` encoders, which check each message against its XML.
/// `sommelier/clippy.toml` rejects `MessageBuilder::new` everywhere else,
/// and the private fields leave no other way to make a builder. A test
/// module that needs frames no encoder makes, such as malformed ones,
/// allows `clippy::disallowed_methods` itself.
pub struct MessageBuilder {
    payload: Vec<u8>,
    fds: Vec<OwnedFd>,
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
        self.write_string_bytes(val.as_bytes());
    }

    /// A string argument of `bytes`, which carry no NUL; for a string
    /// that is not UTF-8, which the fuzzer sends.
    pub(crate) fn write_string_bytes(&mut self, val: &[u8]) {
        let len = val.len() as u32 + 1; // +1 for null terminator
        self.write_u32(len);

        self.payload.extend_from_slice(val);
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

    /// Forward an [`IndexedString`] as it was received.
    pub fn write_indexed_string(&mut self, val: &IndexedString) {
        self.write_string_bytes(val.as_bytes());
    }

    pub fn write_nullable_indexed_string(&mut self, val: Option<&IndexedString>) {
        match val {
            Some(value) => self.write_indexed_string(value),
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

    pub fn write_fd(&mut self, fd: OwnedFd) {
        self.fds.push(fd);
    }
}

impl MessageBuilder {
    /// Assemble a complete Wayland wire message, returning an error instead
    /// of building one longer than [`MessageHeader::MAX_LEN`].
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

    /// Assemble the message together with the descriptors written with
    /// [`Self::write_fd`], as generated forwarding sends it.
    pub fn try_build(self, sender_id: u32, opcode: u16) -> Result<Outgoing, ProtocolError> {
        let message = self.try_build_message(sender_id, opcode)?;
        Ok((message, self.fds))
    }

    /// A builder that continues `payload`, for tests that edit raw frames.
    #[cfg(test)]
    pub fn from_payload(payload: Vec<u8>) -> Self {
        Self {
            payload,
            fds: Vec::new(),
        }
    }

    /// The payload written so far, for tests that edit or decode it.
    #[cfg(test)]
    pub fn into_payload(self) -> Vec<u8> {
        self.payload
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
    /// Panics if the total message is longer than
    /// [`MessageHeader::MAX_LEN`]. Silently truncating the length in release
    /// builds would desynchronise the stream, so this invariant is enforced
    /// in all builds. The generated encoders call it only for messages
    /// without strings or arrays, which always fit.
    pub fn build_message(self, sender_id: u32, opcode: u16) -> Vec<u8> {
        debug_assert!(
            self.fds.is_empty(),
            "build_message would close the descriptors; use try_build"
        );
        self.try_build_message(sender_id, opcode)
            .unwrap_or_else(|error| panic!("{error}"))
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::{
        Framing, IndexedString, MessageBuilder, MessageHeader, Outgoing, ProtocolError, WireMessage,
    };
    use crate::protocols::aura_shell::zaura_surface;
    use crate::protocols::linux_dmabuf_v1::{
        zwp_linux_buffer_params_v1, zwp_linux_dmabuf_feedback_v1,
    };
    use crate::protocols::viewporter::wp_viewport;
    use crate::protocols::wayland::{wl_data_offer, wl_registry, wl_shm, wl_surface};
    use crate::protocols::xdg_shell::xdg_toplevel;
    use crate::test_support::{any_fd_has_identity, fd_identity, raw_message, shm_pool_file, Rng};
    use std::collections::VecDeque;
    use std::os::fd::{AsRawFd, OwnedFd};

    /// Decoding holds for arbitrary bytes: a frame never claims more bytes
    /// than the stream has, no read moves past the payload, and what
    /// `MessageBuilder` writes decodes unchanged.
    #[test]
    fn arbitrary_bytes_decode_within_bounds_and_written_values_round_trip() {
        let mut rng = Rng::new(0);
        let file = shm_pool_file(&[]);
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

            let mut fds: VecDeque<OwnedFd> = (0..rng.below(3))
                .map(|_| file.try_clone().expect("dup a descriptor").into())
                .collect();
            let mut message = WireMessage::new(1, 0, &stream).with_fds(&mut fds);
            for _ in 0..8 {
                let _ = match rng.below(5) {
                    0 => message.read_u32().map(drop),
                    1 => message.read_string().map(drop),
                    2 => message.read_nullable_string().map(drop),
                    3 => message.read_array().map(drop),
                    _ => message.read_fd().map(drop),
                };
                assert!(message.offset <= message.payload.len());
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
            let mut message = WireMessage::new(1, 0, &builder.payload);
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

    /// The message `write` builds by hand, the way the proxy built every
    /// message before the generated encoders. Opcodes are literals, read
    /// off the XML, so these goldens do not trust the generated constants.
    fn hand_built(sender_id: u32, opcode: u16, write: impl FnOnce(&mut MessageBuilder)) -> Vec<u8> {
        let mut builder = MessageBuilder::new();
        write(&mut builder);
        builder.build_message(sender_id, opcode)
    }

    /// A descriptor with an identity no other open descriptor shares.
    fn descriptor() -> OwnedFd {
        shm_pool_file(&[]).into()
    }

    #[test]
    fn encoders_match_hand_built_scalar_messages() {
        assert_eq!(
            raw_message(wl_surface::req::damage(3, -1, 2, 30, 40)),
            (
                hand_built(3, 2, |b| {
                    b.write_i32(-1);
                    b.write_i32(2);
                    b.write_i32(30);
                    b.write_i32(40);
                }),
                Vec::new()
            )
        );
        assert_eq!(
            raw_message(wl_surface::req::commit(3)),
            (hand_built(3, 6, |_| {}), Vec::new())
        );
        // wl_fixed_t keeps its raw 24.8 bits.
        assert_eq!(
            raw_message(wp_viewport::req::set_source(
                4,
                -256,
                0x0100_0001,
                512,
                i32::MAX
            )),
            (
                hand_built(4, 1, |b| {
                    b.write_fixed(-256);
                    b.write_fixed(0x0100_0001);
                    b.write_fixed(512);
                    b.write_fixed(i32::MAX);
                }),
                Vec::new()
            )
        );
        // A nullable object encodes null as 0.
        for (buffer, raw) in [(None, 0), (Some(9), 9)] {
            assert_eq!(
                raw_message(wl_surface::req::attach(3, buffer, 0, 0)),
                (
                    hand_built(3, 1, |b| {
                        b.write_u32(raw);
                        b.write_i32(0);
                        b.write_i32(0);
                    }),
                    Vec::new()
                )
            );
        }
    }

    #[test]
    fn encoders_match_hand_built_string_and_array_messages() {
        // Strings that need 1, 0, 2 and 3 bytes of padding after their NUL.
        // These goldens share MessageBuilder's primitives; the round-trip
        // test below is what checks the layout against the decoder.
        for interface in ["wl_shm", "wl_seat", "wl_output", "abcd"] {
            assert_eq!(
                wl_registry::req::bind(2, 7, interface, 3, 12).map(raw_message),
                Ok((
                    hand_built(2, 0, |b| {
                        b.write_u32(7);
                        b.write_string(interface);
                        b.write_u32(3);
                        b.write_u32(12);
                    }),
                    Vec::new()
                ))
            );
        }
        for startup_id in [None, Some(""), Some("token")] {
            assert_eq!(
                zaura_surface::req::set_startup_id(5, startup_id).map(raw_message),
                Ok((
                    hand_built(5, 3, |b| b.write_nullable_string(startup_id)),
                    Vec::new()
                ))
            );
        }
        let states = [1u32, 4].map(u32::to_ne_bytes).concat();
        assert_eq!(
            xdg_toplevel::evt::configure(6, 800, 600, &states).map(raw_message),
            Ok((
                hand_built(6, 0, |b| {
                    b.write_i32(800);
                    b.write_i32(600);
                    b.write_array(&states);
                }),
                Vec::new()
            ))
        );
        // Two bytes of indices need two bytes of padding.
        assert_eq!(
            zwp_linux_dmabuf_feedback_v1::evt::tranche_formats(6, &[1, 0]).map(raw_message),
            Ok((hand_built(6, 5, |b| b.write_array(&[1, 0])), Vec::new()))
        );
    }

    #[test]
    fn encoders_carry_descriptors_outside_the_payload_in_order() {
        let pool = descriptor();
        let raw_pool = pool.as_raw_fd();
        assert_eq!(
            raw_message(wl_shm::req::create_pool(8, 20, pool, 4096)),
            (
                hand_built(8, 0, |b| {
                    b.write_u32(20);
                    b.write_i32(4096);
                }),
                vec![raw_pool]
            )
        );
        let plane = descriptor();
        let raw_plane = plane.as_raw_fd();
        assert_eq!(
            raw_message(zwp_linux_buffer_params_v1::req::add(
                9, plane, 1, 64, 256, 0, 1
            )),
            (
                hand_built(9, 1, |b| {
                    for value in [1, 64, 256, 0, 1] {
                        b.write_u32(value);
                    }
                }),
                vec![raw_plane]
            )
        );
        let pipe_end = descriptor();
        let raw_pipe = pipe_end.as_raw_fd();
        assert_eq!(
            wl_data_offer::req::receive(10, "text/plain", pipe_end).map(raw_message),
            Ok((
                hand_built(10, 1, |b| b.write_string("text/plain")),
                vec![raw_pipe]
            ))
        );
    }

    /// Decode `outgoing` with `decode`, checking that its header frames the
    /// whole message and that the decoder consumes the whole payload.
    fn round_trip<T>(
        (message, fds): Outgoing,
        decode: impl FnOnce(&mut WireMessage) -> Result<T, ProtocolError>,
    ) -> T {
        let header = MessageHeader::parse(&message).expect("header");
        assert_eq!(header.len, message.len());
        let payload = &message[MessageHeader::LEN..];
        let mut fds = VecDeque::from(fds);
        let mut wire =
            WireMessage::new(header.sender_id, header.opcode, payload).with_fds(&mut fds);
        let decoded = decode(&mut wire).expect("decode");
        assert!(wire.is_payload_consumed());
        decoded
    }

    #[test]
    fn encoded_messages_decode_to_the_same_arguments() {
        let bind = wl_registry::req::bind(2, 7, "wl_seat", 5, 12).unwrap();
        let wl_registry::Request::Bind { name, id } =
            round_trip(bind, wl_registry::Request::from_wire);
        assert_eq!((name, id), (7, ("wl_seat".to_string(), 5, 12)));

        let states = [2u32].map(u32::to_ne_bytes).concat();
        let configure = xdg_toplevel::evt::configure(6, -1, 0, &states).unwrap();
        let xdg_toplevel::Event::Configure {
            width,
            height,
            states: decoded_states,
        } = round_trip(configure, xdg_toplevel::Event::from_wire)
        else {
            panic!("configure decodes as another event");
        };
        assert_eq!((width, height, decoded_states), (-1, 0, states));

        let startup_id = zaura_surface::req::set_startup_id(5, None).unwrap();
        let zaura_surface::Request::SetStartupId { startup_id } =
            round_trip(startup_id, zaura_surface::Request::from_wire)
        else {
            panic!("set_startup_id decodes as another request");
        };
        assert_eq!(startup_id, None);

        let table = descriptor();
        let raw_table = table.as_raw_fd();
        let format_table = zwp_linux_dmabuf_feedback_v1::evt::format_table(6, table, 32);
        let zwp_linux_dmabuf_feedback_v1::Event::FormatTable { fd, size } =
            round_trip(format_table, zwp_linux_dmabuf_feedback_v1::Event::from_wire)
        else {
            panic!("format_table decodes as another event");
        };
        assert_eq!((fd.as_raw_fd(), size), (raw_table, 32));
    }

    #[test]
    fn an_encoder_that_cannot_build_its_message_closes_its_descriptors() {
        let pipe_end = descriptor();
        let identity = fd_identity(pipe_end.as_raw_fd());
        let mime_type = "x".repeat(4_084);
        assert!(matches!(
            wl_data_offer::req::receive(10, &mime_type, pipe_end),
            Err(ProtocolError::MessageTooLarge(_))
        ));
        assert!(
            !any_fd_has_identity(identity),
            "a message that is never sent must not leak its descriptor"
        );
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
    fn header_length_validity_matches_the_wire_limits() {
        let header = |len| MessageHeader {
            sender_id: 1,
            opcode: 0,
            len,
        };
        for valid in [8, 12, 4_096] {
            assert!(header(valid).has_valid_len(), "{valid}");
        }
        // libwayland refuses a message over 4096 bytes, although the 16-bit
        // length field can describe up to 65532.
        for invalid in [0, 4, 7, 10, 4_100, 65_532, 65_536] {
            assert!(!header(invalid).has_valid_len(), "{invalid}");
        }
    }

    #[test]
    fn wire_message_rejects_truncated_scalar() {
        let mut msg = WireMessage::new(1, 0, &[1, 2, 3]);
        assert_eq!(msg.read_u32(), Err(ProtocolError::InsufficientData));
    }

    #[test]
    fn wire_message_rejects_truncated_string_and_array_padding() {
        // Declared string length is four bytes, but only three bytes (and no
        // complete padded payload) are available.
        let string_length = 4u32.to_ne_bytes();
        let mut string_msg = WireMessage::new(1, 0, &string_length);
        assert_eq!(
            string_msg.read_string(),
            Err(ProtocolError::InsufficientData)
        );

        let mut array_payload = Vec::new();
        array_payload.extend_from_slice(&5u32.to_ne_bytes());
        array_payload.extend_from_slice(&[1, 2, 3, 4]);
        let mut array_msg = WireMessage::new(1, 0, &array_payload);
        assert_eq!(array_msg.read_array(), Err(ProtocolError::InsufficientData));
    }

    #[test]
    fn builder_round_trips_padded_string_and_array() {
        let mut builder = MessageBuilder::new();
        builder.write_string("가");
        builder.write_array(&[1, 2, 3]);
        let message = builder.build_message(7, 3);
        let mut wire = WireMessage::new(7, 3, &message[8..]);
        assert_eq!(wire.read_string().unwrap(), "가");
        assert_eq!(wire.read_array().unwrap(), vec![1, 2, 3]);
        assert_eq!(wire.offset, message.len() - 8);
    }

    /// A string that is not UTF-8 decodes with each invalid sequence
    /// replaced by U+FFFD, as libwayland passes it through.
    #[test]
    fn wire_message_replaces_invalid_utf8_in_strings() {
        let mut builder = MessageBuilder::new();
        builder.write_string("a_b_c");
        builder.write_nullable_string(Some("_"));
        let mut payload = builder.into_payload();
        // "a\xffb\xe9c", then "\xe9".
        payload[5] = 0xff;
        payload[7] = 0xe9;
        payload[16] = 0xe9;
        let mut wire = WireMessage::new(1, 0, &payload);
        assert_eq!(wire.read_string(), Ok("a\u{fffd}b\u{fffd}c".to_string()));
        assert_eq!(
            wire.read_nullable_string(),
            Ok(Some("\u{fffd}".to_string()))
        );
        assert!(wire.is_payload_consumed());
    }

    /// A string other arguments index by byte offset keeps its bytes when
    /// it is not UTF-8, so a handler can tell, and a forward can send it
    /// unchanged.
    #[test]
    fn indexed_strings_keep_bytes_that_are_not_utf8() {
        let mut builder = MessageBuilder::new();
        builder.write_string("가");
        builder.write_string("a_");
        builder.write_nullable_string(None);
        builder.write_nullable_string(Some("b_"));
        let mut payload = builder.into_payload();
        payload[13] = 0xe9;
        payload[25] = 0xe9;
        let mut wire = WireMessage::new(1, 0, &payload);
        let utf8 = wire.read_indexed_string().unwrap();
        let latin1 = wire.read_indexed_string().unwrap();
        assert_eq!(utf8, IndexedString::from("가"));
        assert_eq!(latin1, IndexedString::NotUtf8(b"a\xe9".to_vec()));
        assert_eq!(wire.read_nullable_indexed_string(), Ok(None));
        let nullable = wire.read_nullable_indexed_string().unwrap();
        assert_eq!(nullable, Some(IndexedString::NotUtf8(b"b\xe9".to_vec())));
        assert!(wire.is_payload_consumed());

        let mut builder = MessageBuilder::new();
        builder.write_indexed_string(&utf8);
        builder.write_indexed_string(&latin1);
        builder.write_nullable_indexed_string(None);
        builder.write_nullable_indexed_string(nullable.as_ref());
        assert_eq!(builder.into_payload(), payload);
    }

    #[test]
    fn wire_message_rejects_string_without_trailing_nul() {
        // Wayland's string length includes its terminating NUL. A payload
        // containing four non-NUL bytes must not be silently truncated to
        // "abc" by the protocol boundary.
        let mut payload = Vec::new();
        payload.extend_from_slice(&4u32.to_ne_bytes());
        payload.extend_from_slice(b"abcd");
        let mut wire = WireMessage::new(1, 0, &payload);
        assert_eq!(wire.read_string(), Err(ProtocolError::InvalidString));
        assert_eq!(
            wire.offset, 4,
            "invalid input must not consume the string payload"
        );
    }

    #[test]
    fn only_a_successful_decode_can_leave_trailing_data() {
        let payload = [0; 8];
        let mut wire = WireMessage::new(1, 0, &payload);
        assert_eq!(wire.read_u32(), Ok(0));

        assert_eq!(
            wire.check_consumed(Ok(())),
            Err(ProtocolError::TrailingData)
        );
        assert_eq!(
            wire.check_consumed::<()>(Err(ProtocolError::InvalidString)),
            Err(ProtocolError::InvalidString),
            "a decode that stopped early reports why"
        );
        assert_eq!(wire.read_u32(), Ok(0));
        assert_eq!(wire.check_consumed(Ok(())), Ok(()));
    }

    #[test]
    fn wire_message_rejects_string_with_embedded_nul() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&4u32.to_ne_bytes());
        payload.extend_from_slice(b"a\0b\0");
        let mut wire = WireMessage::new(1, 0, &payload);
        assert_eq!(wire.read_string(), Err(ProtocolError::InvalidString));
    }

    #[test]
    fn nullable_strings_preserve_null_and_empty_values() {
        let mut builder = MessageBuilder::new();
        builder.write_nullable_string(None);
        builder.write_nullable_string(Some(""));
        builder.write_nullable_string(Some("가"));
        let message = builder.build_message(7, 3);
        let mut wire = WireMessage::new(7, 3, &message[8..]);

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

        let mut wire = WireMessage::new(7, 3, &packet[8..]);
        let decoded = wire.read_fixed().unwrap();
        let mut forwarded = MessageBuilder::new();
        forwarded.write_fixed(decoded);

        assert_eq!(forwarded.payload, raw.to_ne_bytes());
    }

    #[test]
    fn wire_message_reports_trailing_payload() {
        let mut wire = WireMessage::new(1, 0, &[7, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(wire.read_u32(), Ok(7));
        assert!(!wire.is_payload_consumed());
        assert_eq!(wire.read_u32(), Ok(0));
        assert!(wire.is_payload_consumed());
    }

    #[test]
    #[should_panic(expected = "Wayland message is too large")]
    fn builder_rejects_oversized_wire_message() {
        let mut builder = MessageBuilder::new();
        builder.payload.resize(4_092, 0);
        let _ = builder.build_message(1, 0);
    }

    #[test]
    fn try_builder_reports_oversized_wire_message() {
        let mut builder = MessageBuilder::new();
        builder.payload.resize(4_088, 0);
        assert_eq!(builder.try_build_message(1, 0).map(|m| m.len()), Ok(4_096));
        builder.payload.resize(4_092, 0);
        assert_eq!(
            builder.try_build_message(1, 0),
            Err(ProtocolError::MessageTooLarge(4_100)),
            "libwayland refuses a message over 4096 bytes"
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
