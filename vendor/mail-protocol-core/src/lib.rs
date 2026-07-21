//! Shared, runtime-independent building blocks for mail protocol codecs.

#![forbid(unsafe_code)]

mod codec;
mod error;
mod limits;
pub mod wire;

pub use bytes::{Bytes, BytesMut};
pub use codec::{DecodeStatus, Decoder, Encoder};
pub use error::{ErrorKind, ProtocolError};
pub use limits::Limits;

/// A protocol known by the workspace.
///
/// A variant denotes an architectural extension point, not necessarily an enabled
/// implementation in the facade crate.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum Protocol {
    /// Internet Message Access Protocol.
    Imap,
    /// Simple Mail Transfer Protocol.
    Smtp,
    /// Local Mail Transfer Protocol.
    Lmtp,
    /// Post Office Protocol version 3.
    Pop3,
    /// `ManageSieve` protocol.
    ManageSieve,
    /// JSON Meta Application Protocol for mail.
    Jmap,
}
