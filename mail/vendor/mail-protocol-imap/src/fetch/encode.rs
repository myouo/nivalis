use bytes::BytesMut;
use mail_protocol_core::ProtocolError;
use mail_protocol_core::wire::append_transactionally;

use super::FetchArguments;

impl FetchArguments {
    /// Appends the exact validated FETCH arguments to `dst` atomically.
    ///
    /// This encoding boundary does not reinterpret the validated wire value,
    /// allocate a replacement payload, or introduce dynamic dispatch.
    ///
    /// # Errors
    ///
    /// Returns an error if a future encoder validation step fails. On error,
    /// `dst` is restored to its original length.
    pub fn encode(&self, dst: &mut BytesMut) -> Result<(), ProtocolError> {
        append_transactionally(dst, |dst| {
            dst.extend_from_slice(self.as_bytes());
            Ok(())
        })
    }
}
