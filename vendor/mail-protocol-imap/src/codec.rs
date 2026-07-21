//! IMAP framing, encoding, and continuation codecs.
//!
//! The crate root keeps this module private and re-exports the stable public
//! surface. Submodules separate security-sensitive framing from protocol
//! semantics without adding runtime indirection.

use bytes::{BufMut, Bytes, BytesMut};
use mail_protocol_core::wire::{
    append_transactionally, eq_ascii, find_crlf, slice_ref as slice_for, split_token,
    split_token_preserve_tail,
};
use mail_protocol_core::{DecodeStatus, Decoder, Encoder, ErrorKind, Limits, ProtocolError};
use memchr::memchr2;

use crate::{
    AuthenticateContinuation, Command, CommandBody, CommandBodyRef, CommandRef, IdleDone, Response,
    SelectArguments, SequenceSetRef, Status, StoreOperation, UntaggedData,
    append::{validate_append_arguments, validate_store_flags},
    astring::{parse_astring_prefix, validate_astring},
    command::{CommandKind, classify_command},
    fetch::{
        DEFAULT_FETCH_MAX_DEPTH, split_sequence_and_items, validate_fetch_arguments,
        validate_uid_fetch_arguments,
    },
    id::validate_id_command,
    list::{DEFAULT_LIST_MAX_DEPTH, validate_list_arguments},
    parse_untagged, parse_untagged_with_max_depth,
    search::{DEFAULT_MAX_DEPTH, validate_search_program},
    select::validate_select_arguments,
    status_items::validate_status_items,
};

#[cfg(test)]
use crate::{CommandFrame, SequenceSet};

mod command;
mod command_parse;
mod continuation;
mod encode;
mod response;
mod scanner;
mod transmission;

#[cfg(test)]
mod tests;

pub use command::{CommandDecoder, LiteralRequest, ServerCommandDecoder, ServerCommandStatus};
pub use continuation::{
    AuthenticateContinuationDecoder, AuthenticateContinuationEncoder, IdleDoneDecoder,
    IdleDoneEncoder,
};
pub use encode::CommandEncoder;
pub use response::{ResponseDecoder, ResponseEncoder};
pub use transmission::{ClientCommandTransmission, CommandSendStep};

pub(crate) use command_parse::own_command;
pub(crate) use continuation::validate_base64;
pub(crate) use scanner::validate_tag;

use command_parse::{
    encode_astring_command, encode_raw_command, parse_command, validate_astring_argument,
    validate_uid_arguments,
};
use continuation::validate_initial_response;
use response::parse_response;
use scanner::{
    FrameMode, FrameProgress, FrameScanner, LiteralKind, literal_spec, restore_frame,
    validate_atom, validate_complete_line, validate_incomplete_line, validate_literal_mode,
    validate_raw,
};
