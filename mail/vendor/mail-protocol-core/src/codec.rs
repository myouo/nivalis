use bytes::BytesMut;

use crate::ProtocolError;

/// Result of one incremental decode attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DecodeStatus<T> {
    /// A complete item was removed from the input buffer.
    Complete(T),
    /// More bytes are required; the input buffer was not consumed.
    Incomplete,
}

/// Incrementally decodes protocol items from a caller-owned buffer.
pub trait Decoder {
    /// Successfully decoded value.
    type Item;

    /// Attempts to decode one value.
    ///
    /// Implementations must not consume `src` when returning `Incomplete` or an
    /// error. This makes retry and diagnostic behavior deterministic. Stateful
    /// implementations may cache progress through an incomplete prefix, so the
    /// caller must only append to that same logical buffer before retrying. To
    /// switch buffers, use the implementation's reset operation when available,
    /// or create a fresh decoder.
    ///
    /// # Errors
    ///
    /// Returns a categorized protocol error when a complete or oversized input
    /// cannot be decoded safely.
    fn decode(&mut self, src: &mut BytesMut) -> Result<DecodeStatus<Self::Item>, ProtocolError>;
}

/// Encodes a protocol item into a caller-owned buffer.
pub trait Encoder<T: ?Sized> {
    /// Appends a wire representation of `item` to `dst`.
    ///
    /// # Errors
    ///
    /// Returns a categorized protocol error when `item` cannot be represented
    /// without producing invalid wire syntax. On error, implementations leave
    /// the existing bytes and length of `dst` unchanged; reserved capacity may
    /// still grow.
    fn encode(&mut self, item: &T, dst: &mut BytesMut) -> Result<(), ProtocolError>;
}
