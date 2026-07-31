use bytes::BytesMut;
use mail_protocol_core::ProtocolError;
use mail_protocol_core::wire::append_transactionally;

use super::{ListArguments, ListResponse};

fn encode_validated(wire: &[u8], dst: &mut BytesMut) -> Result<(), ProtocolError> {
    append_transactionally(dst, |dst| {
        dst.extend_from_slice(wire);
        Ok(())
    })
}

impl ListArguments {
    /// Appends the exact validated LIST arguments to `dst` atomically.
    ///
    /// # Errors
    ///
    /// Returns an error if a future encoder validation step fails. On error,
    /// `dst` is restored to its original length.
    pub fn encode(&self, dst: &mut BytesMut) -> Result<(), ProtocolError> {
        encode_validated(self.as_bytes(), dst)
    }
}

impl ListResponse {
    /// Appends the exact validated LIST response data to `dst` atomically.
    ///
    /// # Errors
    ///
    /// Returns an error if a future encoder validation step fails. On error,
    /// `dst` is restored to its original length.
    pub fn encode(&self, dst: &mut BytesMut) -> Result<(), ProtocolError> {
        encode_validated(self.as_bytes(), dst)
    }
}
